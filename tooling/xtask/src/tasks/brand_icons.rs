use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow, bail};
use clap::Parser;
use resvg::tiny_skia::{
    self, BlendMode, FillRule, Paint, PathBuilder, PathSegment, Pixmap, Rect, Shader, Transform,
};
use resvg::usvg;

use crate::workspace::load_workspace;

/// The artwork directory, relative to the repository root.
const SOURCE_DIRECTORY: &str = "assets/branding";
const COLOR_MARK_FILE: &str = "ForgeKairos-mark-color.svg";
const MARK_FILE: &str = "ForgeKairos-mark.svg";
const COMPACT_MARK_FILE: &str = "ForgeKairos-16px.svg";

const PNG_SIZE: u32 = 512;
const PNG_SIZE_2X: u32 = 1024;
const ICO_SIZES: &[u32] = &[16, 24, 32, 48, 64, 128, 256];
/// At and below this size the full mark's thin notches blur into noise, so the
/// simplified mark drawn for 16 px is used instead.
const COMPACT_MARK_MAX_SIZE: u32 = 32;
/// The mark has no backplate, so only a thin margin keeps it from touching the
/// edges; a wider one makes it look small next to other apps' icons.
const ICON_PADDING_RATIO: f32 = 0.04;
const COMPACT_ICON_PADDING_RATIO: f32 = 0.0;

#[derive(Parser)]
pub struct BrandIconsArgs {
    /// Directory holding the ForgeKairos artwork. Defaults to `assets/branding`.
    #[arg(long)]
    source: Option<PathBuf>,
    /// Repository root the generated files are written into. Defaults to the
    /// Cargo workspace root.
    #[arg(long)]
    repository_root: Option<PathBuf>,
}

pub fn run_brand_icons(args: BrandIconsArgs) -> Result<()> {
    let repository_root = match args.repository_root {
        Some(root) => root,
        None => load_workspace()?.workspace_root.into_std_path_buf(),
    };
    let source = args
        .source
        .unwrap_or_else(|| repository_root.join(SOURCE_DIRECTORY));
    let artwork = Artwork::load(&source)?;

    let mut changed = 0;
    for output in generate(&artwork)? {
        let destination = repository_root.join(&output.relative_path);
        let unchanged = std::fs::read(&destination)
            .map(|existing| existing == output.contents)
            .unwrap_or(false);
        if unchanged {
            continue;
        }
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(&destination, &output.contents)
            .with_context(|| format!("writing {}", destination.display()))?;
        println!("updated {}", output.relative_path);
        changed += 1;
    }
    println!("{changed} file(s) updated");
    Ok(())
}

pub struct Artwork {
    color_mark: Vec<Shape>,
    mark: Vec<Shape>,
    compact_mark: Vec<Shape>,
}

impl Artwork {
    pub fn load(directory: &Path) -> Result<Self> {
        let read = |name: &str| -> Result<Vec<Shape>> {
            let path = directory.join(name);
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            shapes_from_svg(&text).with_context(|| format!("parsing {}", path.display()))
        };
        Self::from_shapes(
            read(COLOR_MARK_FILE)?,
            read(MARK_FILE)?,
            read(COMPACT_MARK_FILE)?,
        )
    }

    fn from_shapes(
        color_mark: Vec<Shape>,
        mark: Vec<Shape>,
        compact_mark: Vec<Shape>,
    ) -> Result<Self> {
        for (name, shapes) in [
            (COLOR_MARK_FILE, &color_mark),
            (MARK_FILE, &mark),
            (COMPACT_MARK_FILE, &compact_mark),
        ] {
            if shapes.is_empty() {
                bail!("{name} draws nothing");
            }
        }
        Ok(Self {
            color_mark,
            mark,
            compact_mark,
        })
    }

    /// The simplified mark painted with the color mark's fill, mapped from the
    /// color mark's bounds onto the simplified mark's bounds so the gradient
    /// runs across the same part of the shape.
    fn color_compact_mark(&self) -> Result<Vec<Shape>> {
        let color_bounds = bounds_of(&self.color_mark)?;
        let compact_bounds = bounds_of(&self.compact_mark)?;
        let fill = self
            .color_mark
            .first()
            .map(|shape| shape.fill.clone())
            .ok_or_else(|| anyhow!("{COLOR_MARK_FILE} draws nothing"))?
            .transformed(rect_to_rect(color_bounds, compact_bounds));
        Ok(self
            .compact_mark
            .iter()
            .map(|shape| Shape {
                fill: fill.clone(),
                ..shape.clone()
            })
            .collect())
    }
}

#[derive(Clone)]
struct Shape {
    path: tiny_skia::Path,
    rule: FillRule,
    fill: ShapeFill,
    opacity: f32,
}

#[derive(Clone)]
enum ShapeFill {
    Solid([u8; 3]),
    Linear {
        start: (f32, f32),
        end: (f32, f32),
        stops: Vec<GradientStop>,
        spread: usvg::SpreadMethod,
        /// Maps gradient space onto the same space the shape's path is in.
        transform: Transform,
    },
}

#[derive(Clone)]
struct GradientStop {
    offset: f32,
    color: [u8; 3],
    opacity: f32,
}

impl ShapeFill {
    fn transformed(self, transform: Transform) -> Self {
        match self {
            ShapeFill::Solid(color) => ShapeFill::Solid(color),
            ShapeFill::Linear {
                start,
                end,
                stops,
                spread,
                transform: gradient_transform,
            } => ShapeFill::Linear {
                start,
                end,
                stops,
                spread,
                transform: gradient_transform.post_concat(transform),
            },
        }
    }
}

impl Shape {
    fn transformed(&self, transform: Transform) -> Result<Self> {
        Ok(Self {
            path: self
                .path
                .clone()
                .transform(transform)
                .ok_or_else(|| anyhow!("the mark collapses under the icon transform"))?,
            rule: self.rule,
            fill: self.fill.clone().transformed(transform),
            opacity: self.opacity,
        })
    }
}

fn shapes_from_svg(text: &str) -> Result<Vec<Shape>> {
    let tree = usvg::Tree::from_str(text, &usvg::Options::default())?;
    let mut shapes = Vec::new();
    collect_shapes(tree.root(), &mut shapes)?;
    Ok(shapes)
}

/// Only plain filled paths are accepted: anything else would be drawn
/// differently (or not at all) by the hand-written SVG this tool emits.
fn collect_shapes(group: &usvg::Group, shapes: &mut Vec<Shape>) -> Result<()> {
    if group.opacity().get() < 1.0
        || group.clip_path().is_some()
        || group.mask().is_some()
        || !group.filters().is_empty()
    {
        bail!("group opacity, clipping, masks and filters are not supported");
    }
    for node in group.children() {
        match node {
            usvg::Node::Group(child) => collect_shapes(child, shapes)?,
            usvg::Node::Path(path) => {
                if !path.is_visible() {
                    continue;
                }
                if path.stroke().is_some() {
                    bail!("stroked paths are not supported; outline them first");
                }
                let Some(fill) = path.fill() else {
                    continue;
                };
                let transform = path.abs_transform();
                let shape_fill = match fill.paint() {
                    usvg::Paint::Color(color) => {
                        ShapeFill::Solid([color.red, color.green, color.blue])
                    }
                    usvg::Paint::LinearGradient(gradient) => ShapeFill::Linear {
                        start: (gradient.x1(), gradient.y1()),
                        end: (gradient.x2(), gradient.y2()),
                        stops: gradient
                            .stops()
                            .iter()
                            .map(|stop| GradientStop {
                                offset: stop.offset().get(),
                                color: [stop.color().red, stop.color().green, stop.color().blue],
                                opacity: stop.opacity().get(),
                            })
                            .collect(),
                        spread: gradient.spread_method(),
                        transform: gradient.transform().post_concat(transform),
                    },
                    usvg::Paint::RadialGradient(_) | usvg::Paint::Pattern(_) => {
                        bail!("only solid and linear gradient fills are supported")
                    }
                };
                shapes.push(Shape {
                    path: path
                        .data()
                        .clone()
                        .transform(transform)
                        .ok_or_else(|| anyhow!("a path collapses under its transform"))?,
                    rule: match fill.rule() {
                        usvg::FillRule::NonZero => FillRule::Winding,
                        usvg::FillRule::EvenOdd => FillRule::EvenOdd,
                    },
                    fill: shape_fill,
                    opacity: fill.opacity().get(),
                });
            }
            usvg::Node::Image(_) | usvg::Node::Text(_) => {
                bail!("images and text are not supported; convert text to paths")
            }
        }
    }
    Ok(())
}

fn bounds_of(shapes: &[Shape]) -> Result<Rect> {
    let mut bounds: Option<(f32, f32, f32, f32)> = None;
    for shape in shapes {
        let Some(path_bounds) = shape.path.compute_tight_bounds() else {
            continue;
        };
        bounds = Some(match bounds {
            None => (
                path_bounds.left(),
                path_bounds.top(),
                path_bounds.right(),
                path_bounds.bottom(),
            ),
            Some((left, top, right, bottom)) => (
                left.min(path_bounds.left()),
                top.min(path_bounds.top()),
                right.max(path_bounds.right()),
                bottom.max(path_bounds.bottom()),
            ),
        });
    }
    let (left, top, right, bottom) = bounds.ok_or_else(|| anyhow!("the mark has no area"))?;
    Rect::from_ltrb(left, top, right, bottom).ok_or_else(|| anyhow!("the mark has no area"))
}

fn rect_to_rect(from: Rect, to: Rect) -> Transform {
    let scale_x = to.width() / from.width();
    let scale_y = to.height() / from.height();
    Transform::from_row(
        scale_x,
        0.0,
        0.0,
        scale_y,
        to.left() - from.left() * scale_x,
        to.top() - from.top() * scale_y,
    )
}

/// Scales the shapes uniformly to fit `frame` and centers them in it.
fn fit_into(shapes: &[Shape], frame: Rect) -> Result<Vec<Shape>> {
    let bounds = bounds_of(shapes)?;
    let scale = (frame.width() / bounds.width()).min(frame.height() / bounds.height());
    let transform = Transform::from_row(
        scale,
        0.0,
        0.0,
        scale,
        frame.left() + (frame.width() - bounds.width() * scale) / 2.0 - bounds.left() * scale,
        frame.top() + (frame.height() - bounds.height() * scale) / 2.0 - bounds.top() * scale,
    );
    shapes
        .iter()
        .map(|shape| shape.transformed(transform))
        .collect()
}

fn format_number(value: f32) -> String {
    let rounded = (f64::from(value) * 1000.0).round() / 1000.0;
    if rounded == 0.0 {
        return "0".to_string();
    }
    let text = format!("{rounded:.3}");
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}

fn path_data(path: &tiny_skia::Path) -> String {
    let mut data = String::new();
    let push_command = |data: &mut String, command: &str, points: &[tiny_skia::Point]| {
        if !data.is_empty() {
            data.push(' ');
        }
        data.push_str(command);
        for (index, point) in points.iter().enumerate() {
            if index > 0 {
                data.push(' ');
            }
            data.push_str(&format_number(point.x));
            data.push(' ');
            data.push_str(&format_number(point.y));
        }
    };
    for segment in path.segments() {
        match segment {
            PathSegment::MoveTo(to) => push_command(&mut data, "M", &[to]),
            PathSegment::LineTo(to) => push_command(&mut data, "L", &[to]),
            PathSegment::QuadTo(control, to) => push_command(&mut data, "Q", &[control, to]),
            PathSegment::CubicTo(first, second, to) => {
                push_command(&mut data, "C", &[first, second, to])
            }
            PathSegment::Close => push_command(&mut data, "Z", &[]),
        }
    }
    data
}

fn fill_rule_attribute(rule: FillRule) -> &'static str {
    match rule {
        FillRule::EvenOdd => " fill-rule=\"evenodd\"",
        FillRule::Winding => "",
    }
}

fn hex_color([red, green, blue]: [u8; 3]) -> String {
    format!("#{red:02X}{green:02X}{blue:02X}")
}

/// A full-color SVG of the shapes, used only as input to the rasterizer.
fn color_svg_document(size: u32, shapes: &[Shape]) -> String {
    let mut definitions = String::new();
    let mut body = String::new();
    for (index, shape) in shapes.iter().enumerate() {
        let fill = match &shape.fill {
            ShapeFill::Solid(color) => hex_color(*color),
            ShapeFill::Linear {
                start,
                end,
                stops,
                spread,
                transform,
            } => {
                let spread = match spread {
                    usvg::SpreadMethod::Pad => "pad",
                    usvg::SpreadMethod::Reflect => "reflect",
                    usvg::SpreadMethod::Repeat => "repeat",
                };
                write!(
                    definitions,
                    "<linearGradient id=\"g{index}\" gradientUnits=\"userSpaceOnUse\" \
                     x1=\"{}\" y1=\"{}\" x2=\"{}\" y2=\"{}\" spreadMethod=\"{spread}\" \
                     gradientTransform=\"matrix({} {} {} {} {} {})\">",
                    start.0,
                    start.1,
                    end.0,
                    end.1,
                    transform.sx,
                    transform.ky,
                    transform.kx,
                    transform.sy,
                    transform.tx,
                    transform.ty,
                )
                .ok();
                for stop in stops {
                    write!(
                        definitions,
                        "<stop offset=\"{}\" stop-color=\"{}\" stop-opacity=\"{}\"/>",
                        stop.offset,
                        hex_color(stop.color),
                        stop.opacity
                    )
                    .ok();
                }
                definitions.push_str("</linearGradient>");
                format!("url(#g{index})")
            }
        };
        write!(
            body,
            "<path d=\"{}\" fill=\"{fill}\" fill-opacity=\"{}\"{}/>",
            path_data(&shape.path),
            shape.opacity,
            fill_rule_attribute(shape.rule),
        )
        .ok();
    }
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{size}\" height=\"{size}\" \
         viewBox=\"0 0 {size} {size}\"><defs>{definitions}</defs>{body}</svg>"
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    Stable,
    Dev,
    Nightly,
    Preview,
}

impl Channel {
    const ALL: [Channel; 4] = [
        Channel::Stable,
        Channel::Dev,
        Channel::Nightly,
        Channel::Preview,
    ];

    fn file_suffix(self) -> &'static str {
        match self {
            Channel::Stable => "",
            Channel::Dev => "-dev",
            Channel::Nightly => "-nightly",
            Channel::Preview => "-preview",
        }
    }

    /// Colors far from the mark's blue-to-violet so the badge reads at 16 px.
    fn badge_color(self) -> Option<[u8; 3]> {
        match self {
            Channel::Stable => None,
            Channel::Dev => Some([0xFF, 0x7A, 0x00]),
            Channel::Nightly => Some([0xF0, 0x44, 0x38]),
            Channel::Preview => Some([0x12, 0xB7, 0x6A]),
        }
    }
}

fn render_icon(artwork: &Artwork, size: u32, channel: Channel) -> Result<Pixmap> {
    let (shapes, padding_ratio) = if size <= COMPACT_MARK_MAX_SIZE {
        (artwork.color_compact_mark()?, COMPACT_ICON_PADDING_RATIO)
    } else {
        (artwork.color_mark.clone(), ICON_PADDING_RATIO)
    };
    let edge = size as f32;
    let padding = edge * padding_ratio;
    let frame = Rect::from_xywh(padding, padding, edge - padding * 2.0, edge - padding * 2.0)
        .ok_or_else(|| anyhow!("icon size {size} leaves no room for the mark"))?;
    let document = color_svg_document(size, &fit_into(&shapes, frame)?);
    let tree = usvg::Tree::from_str(&document, &usvg::Options::default())
        .context("parsing the generated icon SVG")?;
    let mut pixmap =
        Pixmap::new(size, size).ok_or_else(|| anyhow!("cannot allocate a {size} px icon"))?;
    resvg::render(&tree, Transform::identity(), &mut pixmap.as_mut());
    if let Some(color) = channel.badge_color() {
        draw_channel_badge(&mut pixmap, color)?;
    }
    Ok(pixmap)
}

struct BadgeGeometry {
    center: f32,
    radius: f32,
    gap: f32,
}

impl BadgeGeometry {
    /// The badge is relatively larger on small icons, where it must still be
    /// recognizable, and smaller on large ones, where it would crowd the mark.
    fn for_edge(edge: u32) -> Self {
        let edge_length = edge as f32;
        let radius_ratio = if edge <= COMPACT_MARK_MAX_SIZE {
            0.2
        } else {
            0.17
        };
        let radius = edge_length * radius_ratio;
        Self {
            center: edge_length - radius - edge_length * 0.01,
            radius,
            gap: (edge_length * 0.05).max(1.0),
        }
    }
}

/// A dot in the bottom-right corner with a transparent ring cut around it, so
/// it stays separate from the mark even at 16 px.
fn draw_channel_badge(pixmap: &mut Pixmap, [red, green, blue]: [u8; 3]) -> Result<()> {
    let BadgeGeometry {
        center,
        radius,
        gap,
    } = BadgeGeometry::for_edge(pixmap.width());

    let cutout = PathBuilder::from_circle(center, center, radius + gap)
        .ok_or_else(|| anyhow!("badge cutout has no area"))?;
    let mut clear = Paint::default();
    clear.anti_alias = true;
    clear.blend_mode = BlendMode::Clear;
    pixmap.fill_path(
        &cutout,
        &clear,
        FillRule::Winding,
        Transform::identity(),
        None,
    );

    let dot = PathBuilder::from_circle(center, center, radius)
        .ok_or_else(|| anyhow!("badge has no area"))?;
    let mut paint = Paint::default();
    paint.anti_alias = true;
    paint.shader = Shader::SolidColor(tiny_skia::Color::from_rgba8(red, green, blue, 255));
    pixmap.fill_path(&dot, &paint, FillRule::Winding, Transform::identity(), None);
    Ok(())
}

fn straight_rgba(pixmap: &Pixmap) -> Vec<[u8; 4]> {
    pixmap
        .pixels()
        .iter()
        .map(|pixel| {
            let color = pixel.demultiply();
            [color.red(), color.green(), color.blue(), color.alpha()]
        })
        .collect()
}

fn encode_png(pixmap: &Pixmap) -> Result<Vec<u8>> {
    pixmap.encode_png().context("encoding PNG")
}

/// Small frames are stored as 32-bit DIBs and only the 256 px frame as PNG,
/// matching what the previous icons held: some consumers of `.ico` files
/// (resource compilers, installers, older shells) only read DIB frames.
fn encode_ico(frames: &[Pixmap]) -> Result<Vec<u8>> {
    let mut images = Vec::new();
    for frame in frames {
        let size = frame.width();
        if frame.height() != size || size == 0 || size > 256 {
            bail!("ICO frames must be square and at most 256 px");
        }
        if size == 256 {
            images.push(encode_png(frame)?);
        } else {
            images.push(encode_dib(frame));
        }
    }

    let count = u16::try_from(frames.len()).context("too many ICO frames")?;
    let mut output = Vec::new();
    output.extend_from_slice(&0u16.to_le_bytes());
    output.extend_from_slice(&1u16.to_le_bytes());
    output.extend_from_slice(&count.to_le_bytes());
    let mut offset = 6 + 16 * u32::from(count);
    for (frame, image) in frames.iter().zip(&images) {
        // 256 is stored as 0 in the one-byte width and height fields.
        let dimension = u8::try_from(frame.width()).unwrap_or(0);
        let length = u32::try_from(image.len()).context("ICO frame too large")?;
        output.push(dimension);
        output.push(dimension);
        output.push(0);
        output.push(0);
        output.extend_from_slice(&1u16.to_le_bytes());
        output.extend_from_slice(&32u16.to_le_bytes());
        output.extend_from_slice(&length.to_le_bytes());
        output.extend_from_slice(&offset.to_le_bytes());
        offset = offset.checked_add(length).context("ICO file too large")?;
    }
    for image in images {
        output.extend_from_slice(&image);
    }
    Ok(output)
}

fn encode_dib(frame: &Pixmap) -> Vec<u8> {
    let size = frame.width() as usize;
    let pixels = straight_rgba(frame);
    let mask_row_bytes = size.div_ceil(32) * 4;
    let image_bytes = size * size * 4 + mask_row_bytes * size;

    let mut output = Vec::with_capacity(40 + image_bytes);
    output.extend_from_slice(&40u32.to_le_bytes());
    output.extend_from_slice(&(size as i32).to_le_bytes());
    // The height covers both the color bitmap and the AND mask.
    output.extend_from_slice(&((size * 2) as i32).to_le_bytes());
    output.extend_from_slice(&1u16.to_le_bytes());
    output.extend_from_slice(&32u16.to_le_bytes());
    output.extend_from_slice(&0u32.to_le_bytes());
    output.extend_from_slice(&(image_bytes as u32).to_le_bytes());
    output.extend_from_slice(&[0; 16]);

    for row in pixels.chunks(size).rev() {
        for [red, green, blue, alpha] in row {
            output.extend_from_slice(&[*blue, *green, *red, *alpha]);
        }
    }
    for row in pixels.chunks(size).rev() {
        let mut mask = vec![0u8; mask_row_bytes];
        for (column, pixel) in row.iter().enumerate() {
            if pixel[3] == 0
                && let Some(byte) = mask.get_mut(column / 8)
            {
                *byte |= 0x80 >> (column % 8);
            }
        }
        output.extend_from_slice(&mask);
    }
    output
}

pub struct GeneratedFile {
    pub relative_path: String,
    pub contents: Vec<u8>,
}

#[derive(Clone, Copy)]
enum MarkVariant {
    Full,
    Compact,
}

/// An in-app SVG drawn as a single-color mask: the mark placed in `frame`,
/// with fixed artwork around it carried over from the icon it replaces.
struct MonochromeIcon {
    relative_path: &'static str,
    width: f32,
    height: f32,
    mark: MarkVariant,
    frame: (f32, f32, f32, f32),
    mark_opacity: Option<f32>,
    /// An even-odd path the mark is clipped to, used to keep a gap around a
    /// stroke drawn across it.
    mark_clip: Option<&'static str>,
    before_mark: &'static str,
    after_mark: &'static str,
}

const ZED_PREDICT_MARK_FRAME: (f32, f32, f32, f32) = (1.75, 4.5, 6.5, 7.0);

const MONOCHROME_ICONS: &[MonochromeIcon] = &[
    MonochromeIcon {
        relative_path: "assets/images/zed_logo.svg",
        width: 96.0,
        height: 96.0,
        mark: MarkVariant::Full,
        frame: (0.0, 0.0, 96.0, 96.0),
        mark_opacity: None,
        mark_clip: None,
        before_mark: "",
        after_mark: "",
    },
    MonochromeIcon {
        relative_path: "assets/icons/forge_kairos.svg",
        width: 16.0,
        height: 16.0,
        mark: MarkVariant::Compact,
        frame: (2.0, 2.0, 12.0, 12.0),
        mark_opacity: None,
        mark_clip: None,
        before_mark: "",
        after_mark: "",
    },
    MonochromeIcon {
        relative_path: "assets/icons/zed_agent.svg",
        width: 16.0,
        height: 16.0,
        mark: MarkVariant::Compact,
        frame: (5.0, 4.5, 7.0, 7.0),
        mark_opacity: None,
        mark_clip: None,
        before_mark: "",
        after_mark: ZED_AGENT_SPARKLES,
    },
    MonochromeIcon {
        relative_path: "assets/icons/zed_predict.svg",
        width: 16.0,
        height: 16.0,
        mark: MarkVariant::Compact,
        frame: ZED_PREDICT_MARK_FRAME,
        mark_opacity: None,
        mark_clip: None,
        before_mark: "<path d=\"M12 5L14 8L12 11\" stroke=\"black\" stroke-width=\"1.2\"/>\n\
                      <path d=\"M10 6.5L11 8L10 9.5\" stroke=\"black\" stroke-width=\"1.2\"/>",
        after_mark: "",
    },
    MonochromeIcon {
        relative_path: "assets/icons/zed_predict_disabled.svg",
        width: 16.0,
        height: 16.0,
        mark: MarkVariant::Compact,
        frame: ZED_PREDICT_MARK_FRAME,
        mark_opacity: Some(0.6),
        mark_clip: Some("M0 0H16V16H0Z M-0.434 2.709L1.434 0.491L16.434 13.091L14.566 15.309Z"),
        before_mark: "",
        after_mark: ZED_PREDICT_DISABLED_ARROWS_AND_SLASH,
    },
    MonochromeIcon {
        relative_path: "assets/icons/zed_predict_down.svg",
        width: 16.0,
        height: 16.0,
        mark: MarkVariant::Compact,
        frame: ZED_PREDICT_MARK_FRAME,
        mark_opacity: None,
        mark_clip: None,
        before_mark: "<path d=\"M15 9.33333L14.5 9.66667L12.5 11L10.5 9.66667L10 9.33333\" stroke=\"black\" stroke-width=\"1.2\"/>\n\
                      <path d=\"M12.5 11V4.5\" stroke=\"black\" stroke-width=\"1.2\"/>",
        after_mark: "",
    },
    MonochromeIcon {
        relative_path: "assets/icons/zed_predict_error.svg",
        width: 16.0,
        height: 16.0,
        mark: MarkVariant::Compact,
        frame: ZED_PREDICT_MARK_FRAME,
        mark_opacity: Some(0.6),
        mark_clip: None,
        before_mark: "",
        after_mark: "<path d=\"M14 8L10 12M14 12L10 8\" stroke=\"black\" stroke-width=\"1.2\"/>",
    },
    MonochromeIcon {
        relative_path: "assets/icons/zed_predict_up.svg",
        width: 16.0,
        height: 16.0,
        mark: MarkVariant::Compact,
        frame: ZED_PREDICT_MARK_FRAME,
        mark_opacity: None,
        mark_clip: None,
        before_mark: "<path d=\"M10 6.66667L10.5 6.33333L12.5 5L14.5 6.33333L15 6.66667\" stroke=\"black\" stroke-width=\"1.2\"/>\n\
                      <path d=\"M12.5 11V5\" stroke=\"black\" stroke-width=\"1.2\"/>",
        after_mark: "",
    },
    MonochromeIcon {
        relative_path: "assets/images/zed_x_copilot.svg",
        width: 93.0,
        height: 32.0,
        mark: MarkVariant::Full,
        frame: (64.6666, 2.6666, 26.6667, 26.6667),
        mark_opacity: None,
        mark_clip: None,
        before_mark: COPILOT_LOGO_AND_CROSS,
        after_mark: "",
    },
];

const ZED_AGENT_SPARKLES: &str = r#"<path d="M2 8.5C2.27614 8.5 2.5 8.27614 2.5 8C2.5 7.72386 2.27614 7.5 2 7.5C1.72386 7.5 1.5 7.72386 1.5 8C1.5 8.27614 1.72386 8.5 2 8.5Z" fill="black"/>
<path opacity="0.6" d="M2.99976 6.33002C3.2759 6.33002 3.49976 6.10616 3.49976 5.83002C3.49976 5.55387 3.2759 5.33002 2.99976 5.33002C2.72361 5.33002 2.49976 5.55387 2.49976 5.83002C2.49976 6.10616 2.72361 6.33002 2.99976 6.33002Z" fill="black"/>
<path opacity="0.6" d="M2.99976 10.66C3.2759 10.66 3.49976 10.4361 3.49976 10.16C3.49976 9.88383 3.2759 9.65997 2.99976 9.65997C2.72361 9.65997 2.49976 9.88383 2.49976 10.16C2.49976 10.4361 2.72361 10.66 2.99976 10.66Z" fill="black"/>
<path d="M15 8.5C15.2761 8.5 15.5 8.27614 15.5 8C15.5 7.72386 15.2761 7.5 15 7.5C14.7239 7.5 14.5 7.72386 14.5 8C14.5 8.27614 14.7239 8.5 15 8.5Z" fill="black"/>
<path opacity="0.6" d="M14 6.33002C14.2761 6.33002 14.5 6.10616 14.5 5.83002C14.5 5.55387 14.2761 5.33002 14 5.33002C13.7239 5.33002 13.5 5.55387 13.5 5.83002C13.5 6.10616 13.7239 6.33002 14 6.33002Z" fill="black"/>
<path opacity="0.6" d="M14 10.66C14.2761 10.66 14.5 10.4361 14.5 10.16C14.5 9.88383 14.2761 9.65997 14 9.65997C13.7239 9.65997 13.5 9.88383 13.5 10.16C13.5 10.4361 13.7239 10.66 14 10.66Z" fill="black"/>
<path d="M8.49219 2C8.76833 2 8.99219 1.77614 8.99219 1.5C8.99219 1.22386 8.76833 1 8.49219 1C8.21605 1 7.99219 1.22386 7.99219 1.5C7.99219 1.77614 8.21605 2 8.49219 2Z" fill="black"/>
<path opacity="0.6" d="M6 3C6.27614 3 6.5 2.77614 6.5 2.5C6.5 2.22386 6.27614 2 6 2C5.72386 2 5.5 2.22386 5.5 2.5C5.5 2.77614 5.72386 3 6 3Z" fill="black"/>
<path d="M4 4C4.27614 4 4.5 3.77614 4.5 3.5C4.5 3.22386 4.27614 3 4 3C3.72386 3 3.5 3.22386 3.5 3.5C3.5 3.77614 3.72386 4 4 4Z" fill="black"/>
<path d="M3.99976 13C4.2759 13 4.49976 12.7761 4.49976 12.5C4.49976 12.2239 4.2759 12 3.99976 12C3.72361 12 3.49976 12.2239 3.49976 12.5C3.49976 12.7761 3.72361 13 3.99976 13Z" fill="black"/>
<path opacity="0.2" d="M2 12.5C2.27614 12.5 2.5 12.2761 2.5 12C2.5 11.7239 2.27614 11.5 2 11.5C1.72386 11.5 1.5 11.7239 1.5 12C1.5 12.2761 1.72386 12.5 2 12.5Z" fill="black"/>
<path opacity="0.2" d="M2 4.5C2.27614 4.5 2.5 4.27614 2.5 4C2.5 3.72386 2.27614 3.5 2 3.5C1.72386 3.5 1.5 3.72386 1.5 4C1.5 4.27614 1.72386 4.5 2 4.5Z" fill="black"/>
<path opacity="0.2" d="M15 12.5C15.2761 12.5 15.5 12.2761 15.5 12C15.5 11.7239 15.2761 11.5 15 11.5C14.7239 11.5 14.5 11.7239 14.5 12C14.5 12.2761 14.7239 12.5 15 12.5Z" fill="black"/>
<path opacity="0.2" d="M15 4.5C15.2761 4.5 15.5 4.27614 15.5 4C15.5 3.72386 15.2761 3.5 15 3.5C14.7239 3.5 14.5 3.72386 14.5 4C14.5 4.27614 14.7239 4.5 15 4.5Z" fill="black"/>
<path opacity="0.5" d="M3.99976 15C4.2759 15 4.49976 14.7761 4.49976 14.5C4.49976 14.2239 4.2759 14 3.99976 14C3.72361 14 3.49976 14.2239 3.49976 14.5C3.49976 14.7761 3.72361 15 3.99976 15Z" fill="black"/>
<path opacity="0.5" d="M4 2C4.27614 2 4.5 1.77614 4.5 1.5C4.5 1.22386 4.27614 1 4 1C3.72386 1 3.5 1.22386 3.5 1.5C3.5 1.77614 3.72386 2 4 2Z" fill="black"/>
<path opacity="0.5" d="M13 15C13.2761 15 13.5 14.7761 13.5 14.5C13.5 14.2239 13.2761 14 13 14C12.7239 14 12.5 14.2239 12.5 14.5C12.5 14.7761 12.7239 15 13 15Z" fill="black"/>
<path opacity="0.5" d="M13 2C13.2761 2 13.5 1.77614 13.5 1.5C13.5 1.22386 13.2761 1 13 1C12.7239 1 12.5 1.22386 12.5 1.5C12.5 1.77614 12.7239 2 13 2Z" fill="black"/>
<path d="M13 4C13.2761 4 13.5 3.77614 13.5 3.5C13.5 3.22386 13.2761 3 13 3C12.7239 3 12.5 3.22386 12.5 3.5C12.5 3.77614 12.7239 4 13 4Z" fill="black"/>
<path d="M13 13C13.2761 13 13.5 12.7761 13.5 12.5C13.5 12.2239 13.2761 12 13 12C12.7239 12 12.5 12.2239 12.5 12.5C12.5 12.7761 12.7239 13 13 13Z" fill="black"/>
<path opacity="0.6" d="M11 3C11.2761 3 11.5 2.77614 11.5 2.5C11.5 2.22386 11.2761 2 11 2C10.7239 2 10.5 2.22386 10.5 2.5C10.5 2.77614 10.7239 3 11 3Z" fill="black"/>
<path d="M8.5 15C8.77614 15 9 14.7761 9 14.5C9 14.2239 8.77614 14 8.5 14C8.22386 14 8 14.2239 8 14.5C8 14.7761 8.22386 15 8.5 15Z" fill="black"/>
<path opacity="0.6" d="M6 14C6.27614 14 6.5 13.7761 6.5 13.5C6.5 13.2239 6.27614 13 6 13C5.72386 13 5.5 13.2239 5.5 13.5C5.5 13.7761 5.72386 14 6 14Z" fill="black"/>
<path opacity="0.6" d="M11 14C11.2761 14 11.5 13.7761 11.5 13.5C11.5 13.2239 11.2761 13 11 13C10.7239 13 10.5 13.2239 10.5 13.5C10.5 13.7761 10.7239 14 11 14Z" fill="black"/>"#;

const ZED_PREDICT_DISABLED_ARROWS_AND_SLASH: &str = r#"<path opacity="0.6" fill-rule="evenodd" clip-rule="evenodd" d="M10.9624 9.40853L11.9014 8L10.6241 6.08397L9.37598 6.91603L10.0986 8L9.80184 8.44518L10.9624 9.40853Z" fill="black"/>
<path opacity="0.6" fill-rule="evenodd" clip-rule="evenodd" d="M12.8936 11.0116L14.9014 8L12.6241 4.58397L11.376 5.41603L13.0986 8L11.7331 10.0483L12.8936 11.0116Z" fill="black"/>
<path fill-rule="evenodd" clip-rule="evenodd" d="M14.1225 13.809C14.0341 13.9146 13.877 13.9289 13.7711 13.8409L1.19311 3.40021C1.08659 3.31178 1.07221 3.15362 1.16104 3.04743L1.87752 2.19101C1.96588 2.0854 2.123 2.07112 2.22895 2.15906L14.8069 12.5998C14.9134 12.6882 14.9278 12.8464 14.839 12.9526L14.1225 13.809Z" fill="black"/>"#;

const COPILOT_LOGO_AND_CROSS: &str = r#"<path fill-rule="evenodd" clip-rule="evenodd" d="M9.03994 7.04962C8.00934 7.67635 7.30394 8.63219 7.30394 10.0149C7.30394 11.6908 7.72423 12.5893 8.20468 13.0744C8.68379 13.5581 9.40524 13.8149 10.4054 13.8149C11.815 13.8149 13.0291 13.5336 13.8802 12.9464C14.6756 12.3977 15.2708 11.5042 15.3438 9.96182C15.3991 8.79382 15.3678 8.01341 15.0568 7.45711C14.8094 7.01449 14.2326 6.47436 12.4901 6.27416C11.4684 6.15678 10.1114 6.39804 9.03994 7.04962ZM7.8731 5.13084C9.39145 4.2075 11.2531 3.87155 12.7464 4.04312C14.8843 4.28874 16.2844 5.05049 17.0171 6.36142C17.6863 7.55867 17.6384 8.98348 17.587 10.068C17.484 12.2439 16.5804 13.8118 15.1554 14.7949C13.7861 15.7396 12.0582 16.0606 10.4054 16.0606C9.04199 16.0606 7.65126 15.7069 6.60911 14.6547C5.5683 13.6038 5.05823 12.0408 5.05823 10.0149C5.05823 7.6958 6.31388 6.07903 7.8731 5.13084Z" fill="black"/>
<path fill-rule="evenodd" clip-rule="evenodd" d="M13.983 18.2811C14.6595 18.2811 15.2079 18.8295 15.2079 19.506V22.16C15.2079 22.8365 14.6595 23.385 13.983 23.385C13.3065 23.385 12.758 22.8365 12.758 22.16V19.506C12.758 18.8295 13.3065 18.2811 13.983 18.2811Z" fill="black"/>
<path fill-rule="evenodd" clip-rule="evenodd" d="M5.25567 14.8903C5.48362 14.039 5.99219 12.9059 7.20735 12.5361L7.92069 14.8798L7.92376 14.8785C7.92375 14.8785 7.92323 14.8778 7.92069 14.8798C7.91365 14.8852 7.89134 14.9052 7.85879 14.951C7.78817 15.0505 7.70058 15.2311 7.62217 15.524C7.46355 16.1164 7.40615 16.9168 7.40617 17.7813V22.1675C7.43457 22.3571 7.45083 22.438 7.48149 22.5189C7.50698 22.5862 7.55955 22.695 7.72277 22.8617C8.0938 23.2406 8.94056 23.8264 10.9275 24.7081C12.5399 25.4236 13.2749 25.7456 13.9586 25.9166C14.6053 26.0784 15.2382 26.1115 16.6666 26.1115V28.5613C15.2536 28.5613 14.3395 28.5372 13.3641 28.2932C12.452 28.0651 11.5174 27.6502 10.0614 27.004C10.0193 26.9853 9.9768 26.9664 9.93383 26.9474C7.89761 26.0438 6.6867 25.3053 5.97233 24.5757C5.59285 24.1882 5.34579 23.7967 5.19056 23.387C5.05419 23.0271 5.00551 22.6875 4.9751 22.4754C4.97301 22.4608 4.971 22.4468 4.96906 22.4335L4.9563 22.3458V17.7814C4.95628 16.8606 5.01201 15.8003 5.25567 14.8903Z" fill="black"/>
<path fill-rule="evenodd" clip-rule="evenodd" d="M3.98306 15.3589C4.62844 14.6695 5.39976 13.9884 5.99652 13.6348L7.24552 15.7424C6.93706 15.9252 6.34938 16.4159 5.77155 17.0332C5.21994 17.6224 4.85695 18.1377 4.73071 18.4254C4.70277 18.6674 4.71195 19.116 4.78977 19.648C4.87834 20.2536 5.01731 20.7033 5.10492 20.8628L5.19261 21.0224L5.23006 21.2007C5.28661 21.4698 5.55952 21.8651 6.30146 22.3907C6.88143 22.8015 7.56502 23.1703 8.29605 23.5648C8.45794 23.6521 8.62215 23.7407 8.78809 23.8313C9.5952 24.2156 10.848 24.7773 12.0191 25.2425C12.613 25.4784 13.1698 25.6831 13.6288 25.8268C13.8584 25.8987 14.0505 25.9512 14.2021 25.9847C14.3503 26.0175 14.4185 26.0227 14.4277 26.0234C14.4288 26.0234 14.4281 26.0234 14.4277 26.0234L14.4287 28.4733C13.9646 28.4733 13.3889 28.3188 12.8968 28.1648C12.3562 27.9955 11.7367 27.7664 11.1147 27.5193C9.86871 27.0244 8.54832 26.4314 7.70298 26.028L7.67249 26.0135L7.64284 25.9973C7.49779 25.918 7.34502 25.8357 7.18702 25.7506C6.4505 25.354 5.60004 24.896 4.88539 24.3898C4.08363 23.8219 3.18153 23.0135 2.87437 21.8785C2.61828 21.3365 2.4557 20.618 2.3657 20.0026C2.26537 19.3167 2.2169 18.4951 2.33888 17.8727L2.35434 17.7938L2.37996 17.7176C2.6522 16.9085 3.36219 16.0221 3.98306 15.3589Z" fill="black"/>
<path fill-rule="evenodd" clip-rule="evenodd" d="M3.48529 21.264V17.1402H5.93516V21.264H3.48529Z" fill="black"/>
<path fill-rule="evenodd" clip-rule="evenodd" d="M24.2932 7.04962C25.3238 7.67635 26.0292 8.63219 26.0292 10.0149C26.0292 11.6908 25.609 12.5893 25.1285 13.0744C24.6494 13.5581 23.9279 13.8149 22.9278 13.8149C21.5182 13.8149 20.3041 13.5336 19.453 12.9464C18.6576 12.3977 18.0624 11.5042 17.9894 9.96182C17.9341 8.79382 17.9654 8.01341 18.2764 7.45711C18.5238 7.01449 19.1006 6.47436 20.8431 6.27416C21.8648 6.15678 23.2218 6.39804 24.2932 7.04962ZM25.4601 5.13084C23.9417 4.2075 22.0801 3.87155 20.5868 4.04312C18.4489 4.28874 17.0488 5.05049 16.3161 6.36142C15.6469 7.55867 15.6948 8.98348 15.7462 10.068C15.8492 12.2439 16.7528 13.8118 18.1778 14.7949C19.5471 15.7396 21.275 16.0606 22.9278 16.0606C24.2912 16.0606 25.6819 15.7069 26.7241 14.6547C27.7649 13.6038 28.275 12.0408 28.275 10.0149C28.275 7.6958 27.0193 6.07903 25.4601 5.13084Z" fill="black"/>
<path fill-rule="evenodd" clip-rule="evenodd" d="M19.3502 18.2811C18.6737 18.2811 18.1253 18.8295 18.1253 19.506V22.16C18.1253 22.8365 18.6737 23.385 19.3502 23.385C20.0267 23.385 20.5752 22.8365 20.5752 22.16V19.506C20.5752 18.8295 20.0267 18.2811 19.3502 18.2811Z" fill="black"/>
<path fill-rule="evenodd" clip-rule="evenodd" d="M28.0775 14.8903C27.8496 14.039 27.341 12.9059 26.1259 12.5361L25.4125 14.8798L25.4095 14.8785C25.4095 14.8785 25.41 14.8778 25.4125 14.8798C25.4196 14.8852 25.4419 14.9052 25.4744 14.951C25.545 15.0505 25.6326 15.2311 25.711 15.524C25.8697 16.1164 25.9271 16.9168 25.927 17.7813V22.1675C25.8986 22.3571 25.8824 22.438 25.8517 22.5189C25.8262 22.5862 25.7737 22.695 25.6104 22.8617C25.2394 23.2406 24.3927 23.8264 22.4057 24.7081C20.7933 25.4236 20.0583 25.7456 19.3746 25.9166C18.7279 26.0784 18.0949 26.1115 16.6666 26.1115V28.5613C18.0796 28.5613 18.9937 28.5372 19.9691 28.2932C20.8812 28.0651 21.8158 27.6502 23.2718 27.004C23.3139 26.9853 23.3564 26.9664 23.3994 26.9474C25.4356 26.0438 26.6465 25.3053 27.3609 24.5757C27.7404 24.1882 27.9874 23.7967 28.1427 23.387C28.279 23.0271 28.3277 22.6875 28.3581 22.4754C28.3602 22.4608 28.3622 22.4468 28.3642 22.4335L28.3769 22.3458V17.7814C28.3769 16.8606 28.3212 15.8003 28.0775 14.8903Z" fill="black"/>
<path fill-rule="evenodd" clip-rule="evenodd" d="M29.3501 15.3589C28.7048 14.6695 27.9334 13.9884 27.3367 13.6348L26.0877 15.7424C26.3961 15.9252 26.9838 16.4159 27.5616 17.0332C28.1133 17.6224 28.4763 18.1377 28.6025 18.4254C28.6304 18.6674 28.6213 19.116 28.5434 19.648C28.4549 20.2536 28.3159 20.7033 28.2283 20.8628L28.1406 21.0224L28.1031 21.2007C28.0466 21.4698 27.7737 21.8651 27.0317 22.3907C26.4518 22.8015 25.7682 23.1703 25.0372 23.5648C24.8753 23.6521 24.711 23.7407 24.5451 23.8313C23.738 24.2156 22.4852 24.7773 21.3141 25.2425C20.7202 25.4784 20.1634 25.6831 19.7044 25.8268C19.4748 25.8987 19.2827 25.9512 19.1311 25.9847C18.9829 26.0175 18.9147 26.0227 18.9055 26.0234C18.9051 26.0234 18.9044 26.0234 18.9055 26.0234L18.9045 28.4733C19.3686 28.4733 19.9443 28.3188 20.4364 28.1648C20.977 27.9955 21.5965 27.7664 22.2185 27.5193C23.4645 27.0244 24.7849 26.4314 25.6302 26.028L25.6607 26.0135L25.6904 25.9973C25.8354 25.918 25.9882 25.8357 26.1462 25.7506C26.8827 25.354 27.7332 24.896 28.4478 24.3898C29.2496 23.8219 30.1517 23.0135 30.4588 21.8785C30.7149 21.3365 30.8775 20.618 30.9675 20.0026C31.0678 19.3167 31.1163 18.4951 30.9943 17.8727L30.9789 17.7938L30.9532 17.7176C30.681 16.9085 29.971 16.0221 29.3501 15.3589Z" fill="black"/>
<path fill-rule="evenodd" clip-rule="evenodd" d="M29.8479 21.264V17.1402H27.398V21.264H29.8479Z" fill="black"/>
<path fill-rule="evenodd" clip-rule="evenodd" d="M48.6666 11C49.2189 11 49.6666 11.4477 49.6666 12V15H52.6666C53.2189 15 53.6666 15.4477 53.6666 16C53.6666 16.5523 53.2189 17 52.6666 17H49.6666V20C49.6666 20.5523 49.2189 21 48.6666 21C48.1143 21 47.6666 20.5523 47.6666 20V17H44.6666C44.1143 17 43.6666 16.5523 43.6666 16C43.6666 15.4477 44.1143 15 44.6666 15H47.6666V12C47.6666 11.4477 48.1143 11 48.6666 11Z" fill="black" fill-opacity="0.5"/>"#;

fn monochrome_svg(icon: &MonochromeIcon, artwork: &Artwork) -> Result<String> {
    let source = match icon.mark {
        MarkVariant::Full => &artwork.mark,
        MarkVariant::Compact => &artwork.compact_mark,
    };
    let (x, y, width, height) = icon.frame;
    let frame = Rect::from_xywh(x, y, width, height)
        .ok_or_else(|| anyhow!("{} has an empty mark frame", icon.relative_path))?;
    let shapes = fit_into(source, frame)?;

    let width = format_number(icon.width);
    let height = format_number(icon.height);
    let mut document = format!(
        "<svg width=\"{width}\" height=\"{height}\" viewBox=\"0 0 {width} {height}\" \
         fill=\"none\" xmlns=\"http://www.w3.org/2000/svg\">\n"
    );
    if !icon.before_mark.is_empty() {
        document.push_str(icon.before_mark);
        document.push('\n');
    }
    let opacity = icon
        .mark_opacity
        .map(|opacity| format!(" opacity=\"{}\"", format_number(opacity)))
        .unwrap_or_default();
    let clip = if icon.mark_clip.is_some() {
        " clip-path=\"url(#mark-clip)\""
    } else {
        ""
    };
    for shape in &shapes {
        writeln!(
            document,
            "<path{opacity} d=\"{}\" fill=\"black\"{}{clip}/>",
            path_data(&shape.path),
            fill_rule_attribute(shape.rule),
        )
        .ok();
    }
    if !icon.after_mark.is_empty() {
        document.push_str(icon.after_mark);
        document.push('\n');
    }
    if let Some(clip) = icon.mark_clip {
        writeln!(
            document,
            "<defs>\n<clipPath id=\"mark-clip\">\n<path d=\"{clip}\" clip-rule=\"evenodd\"/>\n</clipPath>\n</defs>"
        )
        .ok();
    }
    document.push_str("</svg>\n");
    Ok(document)
}

pub fn generate(artwork: &Artwork) -> Result<Vec<GeneratedFile>> {
    let mut files = Vec::new();
    for channel in Channel::ALL {
        let suffix = channel.file_suffix();
        files.push(GeneratedFile {
            relative_path: format!("crates/zed/resources/app-icon{suffix}.png"),
            contents: encode_png(&render_icon(artwork, PNG_SIZE, channel)?)?,
        });
        files.push(GeneratedFile {
            relative_path: format!("crates/zed/resources/app-icon{suffix}@2x.png"),
            contents: encode_png(&render_icon(artwork, PNG_SIZE_2X, channel)?)?,
        });
        let frames = ICO_SIZES
            .iter()
            .map(|size| render_icon(artwork, *size, channel))
            .collect::<Result<Vec<_>>>()?;
        files.push(GeneratedFile {
            relative_path: format!("crates/zed/resources/windows/app-icon{suffix}.ico"),
            contents: encode_ico(&frames)?,
        });
    }
    for icon in MONOCHROME_ICONS {
        files.push(GeneratedFile {
            relative_path: icon.relative_path.to_string(),
            contents: monochrome_svg(icon, artwork)?.into_bytes(),
        });
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    fn repository_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    /// A deliberately non-square mark so centering on the square canvas is
    /// exercised, with a gradient fill like the real color mark.
    fn test_artwork() -> Artwork {
        let color = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 40 80">
            <defs><linearGradient id="g" x1="0" y1="0" x2="0" y2="1">
                <stop offset="0" stop-color="#0082FF"/><stop offset="1" stop-color="#581AD6"/>
            </linearGradient></defs>
            <path d="M10 10 H30 V70 H10 Z" fill="url(#g)"/>
        </svg>"##;
        let mono = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 40 80">
            <path d="M10 10 H30 V70 H10 Z M15 15 H25 V30 H15 Z" fill="currentColor" fill-rule="evenodd"/>
        </svg>"#;
        Artwork::from_shapes(
            shapes_from_svg(color).expect("color test mark parses"),
            shapes_from_svg(mono).expect("mono test mark parses"),
            shapes_from_svg(mono).expect("compact test mark parses"),
        )
        .expect("test artwork is valid")
    }

    struct IcoFrame {
        declared_size: u32,
        width: u32,
        height: u32,
        corner_alpha: u8,
        center_alpha: u8,
    }

    fn read_u16(bytes: &[u8], offset: usize) -> u16 {
        let slice = bytes.get(offset..offset + 2).expect("ICO is truncated");
        u16::from_le_bytes([slice[0], slice[1]])
    }

    fn read_u32(bytes: &[u8], offset: usize) -> u32 {
        let slice = bytes.get(offset..offset + 4).expect("ICO is truncated");
        u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]])
    }

    fn read_ico(bytes: &[u8]) -> Vec<IcoFrame> {
        assert_eq!(read_u16(bytes, 0), 0, "ICO reserved field");
        assert_eq!(read_u16(bytes, 2), 1, "ICO type must be icon");
        let count = read_u16(bytes, 4) as usize;
        (0..count)
            .map(|index| {
                let entry = 6 + index * 16;
                let declared = |value: u8| if value == 0 { 256 } else { u32::from(value) };
                let declared_width = declared(bytes[entry]);
                assert_eq!(declared_width, declared(bytes[entry + 1]), "square entry");
                let length = read_u32(bytes, entry + 8) as usize;
                let offset = read_u32(bytes, entry + 12) as usize;
                let image = bytes
                    .get(offset..offset + length)
                    .expect("ICO frame lies inside the file");
                if image.starts_with(b"\x89PNG") {
                    let decoded = image::load_from_memory(image)
                        .expect("PNG frame decodes")
                        .to_rgba8();
                    let center = decoded.width() / 2;
                    IcoFrame {
                        declared_size: declared_width,
                        width: decoded.width(),
                        height: decoded.height(),
                        corner_alpha: decoded.get_pixel(0, 0)[3],
                        center_alpha: decoded.get_pixel(center, center)[3],
                    }
                } else {
                    assert_eq!(read_u32(image, 0), 40, "BITMAPINFOHEADER");
                    let width = read_u32(image, 4);
                    let doubled_height = read_u32(image, 8);
                    assert_eq!(read_u16(image, 14), 32, "32-bit DIB");
                    let height = doubled_height / 2;
                    let pixel_alpha = |x: u32, y_from_top: u32| {
                        let row = height - 1 - y_from_top;
                        image[40 + ((row * width + x) * 4) as usize + 3]
                    };
                    IcoFrame {
                        declared_size: declared_width,
                        width,
                        height,
                        corner_alpha: pixel_alpha(0, 0),
                        center_alpha: pixel_alpha(width / 2, height / 2),
                    }
                }
            })
            .collect()
    }

    fn png_dimensions(bytes: &[u8]) -> (u32, u32) {
        let decoded = image::load_from_memory(bytes).expect("PNG decodes");
        (decoded.width(), decoded.height())
    }

    fn generated<'a>(files: &'a [GeneratedFile], path: &str) -> &'a [u8] {
        &files
            .iter()
            .find(|file| file.relative_path == path)
            .unwrap_or_else(|| panic!("{path} is generated"))
            .contents
    }

    #[test]
    fn generated_icons_are_square_transparent_and_complete() {
        let files = generate(&test_artwork()).expect("generation succeeds");
        for channel in Channel::ALL {
            let suffix = channel.file_suffix();
            for (name, size) in [
                (format!("crates/zed/resources/app-icon{suffix}.png"), 512),
                (
                    format!("crates/zed/resources/app-icon{suffix}@2x.png"),
                    1024,
                ),
            ] {
                let decoded = image::load_from_memory(generated(&files, &name))
                    .expect("PNG decodes")
                    .to_rgba8();
                assert_eq!((decoded.width(), decoded.height()), (size, size), "{name}");
                assert_eq!(
                    decoded.get_pixel(0, 0)[3],
                    0,
                    "{name} has a transparent corner"
                );
                assert!(
                    decoded.get_pixel(size / 2, size / 2)[3] > 0,
                    "{name} draws the mark"
                );
            }

            let ico_path = format!("crates/zed/resources/windows/app-icon{suffix}.ico");
            let frames = read_ico(generated(&files, &ico_path));
            let sizes: BTreeSet<u32> = frames.iter().map(|frame| frame.declared_size).collect();
            assert_eq!(
                sizes,
                BTreeSet::from([16, 24, 32, 48, 64, 128, 256]),
                "{ico_path} frame sizes"
            );
            for frame in &frames {
                assert_eq!(
                    (frame.width, frame.height),
                    (frame.declared_size, frame.declared_size),
                    "{ico_path} frame dimensions match the directory"
                );
                assert_eq!(frame.corner_alpha, 0, "{ico_path} has transparent corners");
                assert!(frame.center_alpha > 0, "{ico_path} draws the mark");
            }
        }
    }

    #[test]
    fn generation_is_deterministic() {
        let artwork = test_artwork();
        let first = generate(&artwork).expect("generation succeeds");
        let second = generate(&artwork).expect("generation succeeds");
        assert_eq!(first.len(), second.len());
        for (left, right) in first.iter().zip(&second) {
            assert_eq!(left.relative_path, right.relative_path);
            assert!(
                left.contents == right.contents,
                "{} differs between runs",
                left.relative_path
            );
        }
    }

    #[test]
    fn channel_icons_carry_a_badge_the_stable_icon_does_not() {
        let artwork = test_artwork();
        let corner = |channel| {
            let pixmap = render_icon(&artwork, 64, channel).expect("renders");
            let pixels = straight_rgba(&pixmap);
            let badge_center = BadgeGeometry::for_edge(64).center as usize;
            pixels[badge_center * 64 + badge_center]
        };
        assert_eq!(corner(Channel::Stable)[3], 0);
        for channel in [Channel::Dev, Channel::Nightly, Channel::Preview] {
            let pixel = corner(channel);
            let expected = channel.badge_color().expect("channel has a badge");
            assert_eq!(
                [pixel[0], pixel[1], pixel[2], pixel[3]],
                [expected[0], expected[1], expected[2], 255]
            );
        }
    }

    #[test]
    fn repository_icons_have_the_expected_dimensions() {
        let root = repository_root();
        for channel in Channel::ALL {
            let suffix = channel.file_suffix();
            for (name, size) in [
                (format!("crates/zed/resources/app-icon{suffix}.png"), 512),
                (
                    format!("crates/zed/resources/app-icon{suffix}@2x.png"),
                    1024,
                ),
            ] {
                let bytes = std::fs::read(root.join(&name)).expect("icon exists");
                assert_eq!(png_dimensions(&bytes), (size, size), "{name}");
            }
            let name = format!("crates/zed/resources/windows/app-icon{suffix}.ico");
            let bytes = std::fs::read(root.join(&name)).expect("icon exists");
            let frames = read_ico(&bytes);
            let sizes: BTreeSet<u32> = frames.iter().map(|frame| frame.declared_size).collect();
            assert_eq!(
                sizes,
                ICO_SIZES.iter().copied().collect::<BTreeSet<_>>(),
                "{name}"
            );
            for frame in frames {
                assert_eq!(
                    (frame.width, frame.height),
                    (frame.declared_size, frame.declared_size)
                );
                assert_eq!(frame.corner_alpha, 0, "{name} has transparent corners");
            }
        }
    }

    #[test]
    fn in_app_vectors_are_single_color() {
        let root = repository_root();
        let paint =
            regex::Regex::new(r#"\b(fill|stroke|stop-color)="([^"]*)""#).expect("valid regex");
        for icon in MONOCHROME_ICONS {
            let text =
                std::fs::read_to_string(root.join(icon.relative_path)).expect("vector exists");
            for captures in paint.captures_iter(&text) {
                let value = &captures[2];
                assert!(
                    matches!(value, "black" | "currentColor" | "none"),
                    "{} paints with {value}",
                    icon.relative_path
                );
            }
            for forbidden in ["<linearGradient", "<radialGradient", "<image", "<pattern"] {
                assert!(
                    !text.contains(forbidden),
                    "{} contains {forbidden}",
                    icon.relative_path
                );
            }
        }
    }

    #[test]
    fn in_app_vectors_no_longer_draw_the_zed_glyph() {
        let root = repository_root();
        let zed_glyph_paths = [
            (
                "assets/images/zed_logo.svg",
                "M9 6C7.34315 6 6 7.34315 6 9V75",
            ),
            (
                "assets/images/zed_x_copilot.svg",
                "M67.1666 4.33329C66.7064",
            ),
            ("assets/icons/zed_agent.svg", "M11 8.75V10.5C8.93097"),
            ("assets/icons/zed_predict.svg", "M7.5 8.9V11C5.43097"),
            ("assets/icons/zed_predict_down.svg", "M7.5 8.9V11C5.43097"),
            ("assets/icons/zed_predict_up.svg", "M7.5 8.9V11C5.43097"),
            ("assets/icons/zed_predict_error.svg", "M7.5 8.9V11C5.43097"),
            (
                "assets/icons/zed_predict_disabled.svg",
                "M6.75 9.31247L8.25 10.5576",
            ),
        ];
        for (path, glyph) in zed_glyph_paths {
            let text = std::fs::read_to_string(root.join(path)).expect("vector exists");
            assert!(!text.contains(glyph), "{path} still draws the Zed glyph");
        }
    }

    /// Catches artwork edited without rerunning `cargo xtask brand-icons`.
    /// Rasters are compared with a small tolerance so a different CPU's
    /// rasterizer rounding does not fail the check.
    #[test]
    fn repository_icons_match_the_artwork() {
        let root = repository_root();
        let artwork = Artwork::load(&root.join(SOURCE_DIRECTORY)).expect("artwork loads");
        for file in generate(&artwork).expect("generation succeeds") {
            let on_disk = std::fs::read(root.join(&file.relative_path))
                .unwrap_or_else(|_| panic!("{} exists", file.relative_path));
            if file.relative_path.ends_with(".svg") {
                assert!(
                    on_disk == file.contents,
                    "{} is stale; run `cargo xtask brand-icons`",
                    file.relative_path
                );
            } else if file.relative_path.ends_with(".png") {
                let expected = image::load_from_memory(&file.contents)
                    .expect("PNG decodes")
                    .to_rgba8();
                let actual = image::load_from_memory(&on_disk)
                    .expect("PNG decodes")
                    .to_rgba8();
                assert_eq!(expected.dimensions(), actual.dimensions());
                let largest_difference = expected
                    .as_raw()
                    .iter()
                    .zip(actual.as_raw())
                    .map(|(left, right)| left.abs_diff(*right))
                    .max()
                    .unwrap_or(0);
                assert!(
                    largest_difference <= 2,
                    "{} is stale; run `cargo xtask brand-icons`",
                    file.relative_path
                );
            } else {
                let expected = read_ico(&file.contents);
                let actual = read_ico(&on_disk);
                assert_eq!(
                    expected
                        .iter()
                        .map(|frame| frame.declared_size)
                        .collect::<Vec<_>>(),
                    actual
                        .iter()
                        .map(|frame| frame.declared_size)
                        .collect::<Vec<_>>(),
                    "{} is stale; run `cargo xtask brand-icons`",
                    file.relative_path
                );
            }
        }
    }
}
