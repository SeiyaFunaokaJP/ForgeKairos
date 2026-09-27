use collections::HashMap;
use gpui::{
    Animation, AnimationExt, AnyElement, App, BorderStyle, Bounds, ClipboardItem, Context, Entity,
    ImageSource, Img, MouseButton, MouseDownEvent, MouseMoveEvent, ParsedSvg, Point, RenderImage,
    SMOOTH_SVG_SCALE_FACTOR, ScrollDelta, ScrollHandle, ScrollWheelEvent, Size, Stateful,
    StyledText, Task, Window, canvas, img, point, pulsating_between, quad, size,
};
use mermaid_render::{DiagramGeometry, LabelItem};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::ops::Range;
use std::path::Path;
use std::rc::Rc;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use ui::{CopyButton, ScrollAxes, Scrollbars, TintColor, Tooltip, WithScrollbar, prelude::*};

use crate::parser::{CodeBlockKind, MarkdownEvent, MarkdownTag};
use settings::Settings as _;
use theme_settings::ThemeSettings;

use crate::mermaid_source::{self, MermaidLabelQuoting};

use super::{
    CopyButtonVisibility, Markdown, MarkdownEditCallback, MarkdownStyle, MermaidLabelEditCallback,
    MermaidLabelEditRequest, MermaidZoomCallback, ParsedMarkdown,
};

type MermaidDiagramCache = HashMap<ParsedMarkdownMermaidDiagramContents, Arc<CachedMermaidDiagram>>;

/// Per scroll tick, zoom changes by 10 percentage points, regardless of how
/// large a delta the platform reports for the tick.
const MERMAID_ZOOM_STEP: f32 = 0.1;
/// How many pixels of precise (trackpad) scroll make up one zoom tick.
const PIXELS_PER_ZOOM_TICK: f32 = 20.0;

#[derive(Clone, Debug)]
pub(crate) struct ParsedMarkdownMermaidDiagram {
    pub(crate) content_range: Range<usize>,
    pub(crate) contents: ParsedMarkdownMermaidDiagramContents,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ParsedMarkdownMermaidDiagramContents {
    pub(crate) contents: SharedString,
    pub(crate) scale: u32,
}

#[derive(Default, Clone)]
pub(crate) struct MermaidState {
    cache: MermaidDiagramCache,
    order: Vec<ParsedMarkdownMermaidDiagramContents>,
}

struct CachedMermaidDiagram {
    render_image: Arc<OnceLock<anyhow::Result<Arc<RenderImage>>>>,
    parsed_svg: Arc<OnceLock<Arc<ParsedSvg>>>,
    /// Where the diagram's nodes and edge labels landed, filled in alongside
    /// `parsed_svg` and reused across re-rasters since zooming does not change
    /// the layout.
    geometry: Arc<OnceLock<Arc<DiagramGeometry>>>,
    /// The previous raster shown while `render_image` is pending, along with
    /// the scale it was rasterized at, so it can be displayed at the same
    /// logical size as the raster that will replace it.
    fallback_image: Option<(Arc<RenderImage>, f32)>,
    /// The scale, relative to the diagram's natural size, that `render_image`
    /// was (or is being) rasterized at.
    rasterized_scale: f32,
    _task: Task<()>,
}

impl MermaidState {
    pub(crate) fn clear(&mut self, cx: &mut App) {
        for cached in self.cache.values() {
            cached.drop_images(cx);
        }
        self.cache.clear();
        self.order.clear();
    }

    fn get_fallback_image(
        idx: usize,
        old_order: &[ParsedMarkdownMermaidDiagramContents],
        new_order_len: usize,
        cache: &MermaidDiagramCache,
    ) -> Option<(Arc<RenderImage>, f32)> {
        if old_order.len() != new_order_len {
            return None;
        }

        old_order.get(idx).and_then(|old_content| {
            cache.get(old_content).and_then(|old_cached| {
                old_cached
                    .render_image
                    .get()
                    .and_then(|result| result.as_ref().ok().cloned())
                    .map(|image| (image, old_cached.rasterized_scale))
                    .or_else(|| old_cached.fallback_image.clone())
            })
        })
    }

    pub(crate) fn update(
        &mut self,
        parsed: &ParsedMarkdown,
        zoom_for_offset: impl Fn(usize) -> f32,
        cx: &mut Context<Markdown>,
    ) {
        let mut new_order = Vec::new();
        let mut source_offsets = Vec::new();
        for (source_offset, mermaid_diagram) in parsed.mermaid_diagrams.iter() {
            new_order.push(mermaid_diagram.contents.clone());
            source_offsets.push(*source_offset);
        }

        for (idx, new_content) in new_order.iter().enumerate() {
            if !self.cache.contains_key(new_content) {
                let fallback =
                    Self::get_fallback_image(idx, &self.order, new_order.len(), &self.cache);
                // The cache is keyed by contents, so duplicate diagrams share
                // one entry; if duplicates have different zooms, the zoom of
                // the first occurrence wins.
                let zoom = source_offsets
                    .get(idx)
                    .copied()
                    .map_or(1.0, &zoom_for_offset);
                self.cache.insert(
                    new_content.clone(),
                    Arc::new(CachedMermaidDiagram::new(
                        new_content.clone(),
                        zoom,
                        fallback,
                        cx,
                    )),
                );
            }
        }

        let new_order_set: std::collections::HashSet<_> = new_order.iter().cloned().collect();
        self.cache.retain(|content, cached| {
            let keep = new_order_set.contains(content);
            if !keep {
                cached.drop_images(cx);
            }
            keep
        });
        self.order = new_order;
    }

    /// The natural (zoom 1.0) logical size of a diagram, if any raster of it
    /// is available to recover the size from.
    pub(crate) fn natural_size(
        &self,
        contents: &ParsedMarkdownMermaidDiagramContents,
    ) -> Option<Size<Pixels>> {
        let cached = self.cache.get(contents)?;
        let (image, rasterized_scale) = cached
            .render_image
            .get()
            .and_then(|result| result.as_ref().ok())
            .map(|image| (image.clone(), cached.rasterized_scale))
            .or_else(|| cached.fallback_image.clone())?;
        Some(mermaid_base_size(&image, rasterized_scale))
    }

    /// Re-rasterizes a single cached diagram at exactly the scale it is
    /// displayed at (`zoom` times its natural size), reusing the cached
    /// parsed SVG so that neither mermaid layout nor SVG parsing is re-run.
    /// No-ops if the entry is already rasterized at the target scale, or if
    /// it's still mid-render or failed to render.
    pub(crate) fn rerasterize_diagram(
        &mut self,
        contents: &ParsedMarkdownMermaidDiagramContents,
        zoom: f32,
        cx: &mut Context<Markdown>,
    ) {
        let Some(cached) = self.cache.get_mut(contents) else {
            return;
        };
        let Some(parsed_svg) = cached.parsed_svg.get().cloned() else {
            return;
        };
        let target_scale = zoom;
        if (cached.rasterized_scale - target_scale).abs() < 0.001 {
            return;
        }
        // The old render image lives on as the new entry's fallback, so it
        // must not be dropped here; the old fallback is no longer painted.
        // If the old raster is still pending, keep showing the previous
        // fallback so the diagram doesn't disappear mid-zoom.
        let new_fallback = match cached
            .render_image
            .get()
            .and_then(|result| result.as_ref().ok())
        {
            Some(image) => {
                if let Some((old_fallback, _)) = cached.fallback_image.clone() {
                    cx.drop_image(old_fallback, None);
                }
                Some((image.clone(), cached.rasterized_scale))
            }
            None => cached.fallback_image.clone(),
        };
        let scale_factor = contents.scale as f32 / 100.0 * target_scale;
        *cached = Arc::new(CachedMermaidDiagram::new_from_parsed(
            parsed_svg,
            cached.geometry.clone(),
            scale_factor,
            target_scale,
            new_fallback,
            cx,
        ));
    }
}

impl CachedMermaidDiagram {
    fn new(
        contents: ParsedMarkdownMermaidDiagramContents,
        zoom: f32,
        fallback_image: Option<(Arc<RenderImage>, f32)>,
        cx: &mut Context<Markdown>,
    ) -> Self {
        let render_image = Arc::new(OnceLock::<anyhow::Result<Arc<RenderImage>>>::new());
        let parsed_svg = Arc::new(OnceLock::<Arc<ParsedSvg>>::new());
        let geometry = Arc::new(OnceLock::<Arc<DiagramGeometry>>::new());
        let svg_renderer = cx.svg_renderer();
        let mermaid_theme = build_mermaid_theme(cx);

        let task = cx.spawn({
            let render_image = render_image.clone();
            let parsed_svg = parsed_svg.clone();
            let geometry = geometry.clone();
            let fallback_image = fallback_image.clone();
            async move |this, cx| {
                let value = cx
                    .background_spawn(async move {
                        let (svg_string, diagram_geometry) =
                            mermaid_render::render_to_svg_with_geometry(
                                &contents.contents,
                                &mermaid_theme,
                            )?;
                        let tree = svg_renderer
                            .parse_svg(svg_string.as_bytes())
                            .map_err(|error| anyhow::anyhow!("{error}"))?;
                        let tree = Arc::new(tree);
                        let scale = contents.scale as f32 / 100.0 * zoom;
                        let image = svg_renderer
                            .render_parsed(&tree, scale)
                            .map_err(|error| anyhow::anyhow!("{error}"))?;
                        anyhow::Ok((tree, Arc::new(diagram_geometry), image))
                    })
                    .await;
                let value = match value {
                    Ok((tree, diagram_geometry, image)) => {
                        parsed_svg.set(tree).ok();
                        geometry.set(diagram_geometry).ok();
                        Ok(image)
                    }
                    Err(error) => Err(error),
                };
                render_image.set(value).ok();
                Self::on_render_complete(this, fallback_image, cx);
            }
        });

        Self {
            render_image,
            parsed_svg,
            geometry,
            fallback_image,
            rasterized_scale: zoom,
            _task: task,
        }
    }

    fn new_from_parsed(
        parsed_svg: Arc<ParsedSvg>,
        geometry: Arc<OnceLock<Arc<DiagramGeometry>>>,
        scale_factor: f32,
        rasterized_scale: f32,
        fallback_image: Option<(Arc<RenderImage>, f32)>,
        cx: &mut Context<Markdown>,
    ) -> Self {
        let render_image = Arc::new(OnceLock::<anyhow::Result<Arc<RenderImage>>>::new());
        let parsed_svg_cell = Arc::new(OnceLock::new());
        parsed_svg_cell.set(parsed_svg.clone()).ok();
        let svg_renderer = cx.svg_renderer();

        let task = cx.spawn({
            let render_image = render_image.clone();
            let fallback_image = fallback_image.clone();
            async move |this, cx| {
                let value = cx
                    .background_spawn(async move {
                        svg_renderer
                            .render_parsed(&parsed_svg, scale_factor)
                            .map_err(|error| anyhow::anyhow!("{error}"))
                    })
                    .await;
                render_image.set(value).ok();
                Self::on_render_complete(this, fallback_image, cx);
            }
        });

        Self {
            render_image,
            parsed_svg: parsed_svg_cell,
            geometry,
            fallback_image,
            rasterized_scale,
            _task: task,
        }
    }

    fn on_render_complete(
        this: gpui::WeakEntity<Markdown>,
        fallback_image: Option<(Arc<RenderImage>, f32)>,
        cx: &mut gpui::AsyncApp,
    ) {
        this.update(cx, |_, cx| {
            // The fallback will no longer be painted now that the real render
            // is available, so its GPU texture can be released.
            if let Some((fallback_image, _)) = fallback_image {
                cx.drop_image(fallback_image, None);
            }
            cx.notify();
        })
        .ok();
    }

    fn drop_images(&self, cx: &mut App) {
        if let Some(Ok(render_image)) = self.render_image.get() {
            cx.drop_image(render_image.clone(), None);
        }
        if let Some((fallback_image, _)) = self.fallback_image.clone() {
            cx.drop_image(fallback_image, None);
        }
    }

    #[cfg(test)]
    fn new_for_test(
        render_image: Option<Arc<RenderImage>>,
        fallback_image: Option<Arc<RenderImage>>,
        parsed_svg: Option<Arc<ParsedSvg>>,
        geometry: Option<Arc<DiagramGeometry>>,
    ) -> Self {
        let result = Arc::new(OnceLock::new());
        if let Some(render_image) = render_image {
            let _ = result.set(Ok(render_image));
        }
        let parsed_svg_cell = Arc::new(OnceLock::new());
        if let Some(parsed_svg) = parsed_svg {
            let _ = parsed_svg_cell.set(parsed_svg);
        }
        let geometry_cell = Arc::new(OnceLock::new());
        if let Some(geometry) = geometry {
            let _ = geometry_cell.set(geometry);
        }
        Self {
            render_image: result,
            parsed_svg: parsed_svg_cell,
            geometry: geometry_cell,
            fallback_image: fallback_image.map(|image| (image, 1.0)),
            rasterized_scale: 1.0,
            _task: Task::ready(()),
        }
    }
}

/// Merman has somewhat limited text measurement capabilities.
///
/// When it doesn't have metrics for any of the specified fonts, it chooses a
/// fairly narrow width, which causes visible overflow. Adding `sans-serif`
/// allows it to fall back to a more conservative (i.e. wider) measurement.
///
/// This isn't perfect - very wide fonts will likely still cause overflow. A
/// proper fix would involve somehow piping `resvg`'s actual measurements into
/// `merman`, but that is a lot of work for a fairly uncommon edge case.
fn mermaid_font_family(font_family: &str) -> String {
    let font_family = gpui::font_name_with_fallbacks(font_family, "system-ui");
    if font_family
        .split(',')
        .any(|family| family.trim().eq_ignore_ascii_case("sans-serif"))
    {
        font_family.to_string()
    } else {
        format!("{font_family}, sans-serif")
    }
}

/// The Mermaid palette derived from the user's Zed theme.
///
/// Public so that the structured flowchart editor's live preview draws with the
/// same colors as the preview it will write back into.
pub fn build_mermaid_theme(cx: &App) -> mermaid_render::MermaidTheme {
    let colors = cx.theme().colors();
    let theme_settings = ThemeSettings::get_global(cx);
    let is_dark = !cx.theme().appearance.is_light();

    let players = cx.theme().players();
    let git_branch_colors = std::array::from_fn(|i| players.0[i % players.0.len()].cursor);
    let git_branch_label_colors = git_branch_colors.map(mermaid_render::text_color_for_background);

    mermaid_render::MermaidTheme {
        dark_mode: is_dark,
        font_family: mermaid_font_family(theme_settings.ui_font.family.as_ref()),
        background: colors.editor_background,
        primary_color: colors.surface_background,
        primary_text_color: colors.text,
        primary_border_color: colors.border,
        secondary_color: colors.element_background,
        tertiary_color: colors.ghost_element_hover,
        line_color: colors.border,
        text_color: colors.text,
        edge_label_background: colors.editor_background,
        cluster_background: colors.panel_background,
        cluster_border: colors.border_variant,
        note_background: colors.surface_background,
        note_border: colors.border_variant,
        actor_background: colors.element_background,
        actor_border: colors.border,
        activation_background: colors.ghost_element_hover,
        activation_border: colors.border,
        git_branch_colors,
        git_branch_label_colors,
        er_attr_bg_odd: colors.surface_background,
        er_attr_bg_even: colors.element_background,
        error_color: cx.theme().status().error,
        warning_color: cx.theme().status().warning,
        accent_colors: players
            .0
            .iter()
            .map(|player| mermaid_render::AccentColor {
                foreground: player.cursor,
                background: player.background,
            })
            .collect(),
    }
}

/// The scale percentage a fenced code block's info string asks for, or `None`
/// when the block is not a mermaid diagram.
pub fn parse_mermaid_info(info: &str) -> Option<u32> {
    let mut parts = info.split_whitespace();
    if parts.next()? != "mermaid" {
        return None;
    }

    Some(
        parts
            .next()
            .and_then(|scale| scale.parse().ok())
            .unwrap_or(100)
            .clamp(10, 500),
    )
}

/// We deliberately block rendering of some diagram types, even though `merman`
/// supports them, because we have not yet written custom CSS to ensure text is
/// readable.
pub fn is_supported_diagram_type(source: &str) -> bool {
    /// If updating this list, also update the system prompt!
    const SUPPORTED_PREFIXES: &[&str] = &[
        "flowchart",
        "graph",
        "sequenceDiagram",
        "classDiagram",
        "classDiagram-v2",
        "stateDiagram",
        "stateDiagram-v2",
        "erDiagram",
        "gantt",
        "pie",
        "gitGraph",
        "mindmap",
        "timeline",
        "quadrantChart",
        "xychart-beta",
        "xychart",
        "journey",
        "radar-beta",
        "radar-beta:",
        "requirementDiagram",
        "C4Context",
        "C4Container",
        "C4Component",
        "C4Dynamic",
        "C4Deployment",
        "block-beta",
        "block",
        "sankey-beta",
        "sankey",
        "packet-beta",
        "packet",
        "venn-beta",
        "kanban",
        "treemap-beta",
        "treemap",
        "architecture-beta",
        "treeView-beta",
        "ishikawa-beta",
        "ishikawa",
        "eventmodeling",
    ];
    let first_token = source
        .trim_start()
        .split(|c: char| c.is_whitespace() || c == '\n')
        .next()
        .unwrap_or("");
    SUPPORTED_PREFIXES
        .iter()
        .any(|prefix| first_token.eq_ignore_ascii_case(prefix))
}

pub(crate) fn extract_mermaid_diagrams(
    source: &str,
    events: &[(Range<usize>, MarkdownEvent)],
) -> BTreeMap<usize, ParsedMarkdownMermaidDiagram> {
    let mut mermaid_diagrams = BTreeMap::default();

    for (source_range, event) in events {
        let MarkdownEvent::Start(MarkdownTag::CodeBlock { kind, metadata }) = event else {
            continue;
        };
        if !metadata.is_fenced_closed {
            continue;
        }
        let scale = match kind {
            CodeBlockKind::FencedLang(info) => match parse_mermaid_info(info.as_ref()) {
                Some(scale) => scale,
                None => continue,
            },
            CodeBlockKind::FencedSrc(path_range) => {
                let path = Path::new(path_range.path.as_ref());
                match path.extension().and_then(|ext| ext.to_str()) {
                    Some("mermaid" | "mmd") => 100,
                    _ => continue,
                }
            }
            _ => continue,
        };

        let contents = source[metadata.content_range.clone()]
            .strip_suffix('\n')
            .unwrap_or(&source[metadata.content_range.clone()])
            .to_string();
        if !is_supported_diagram_type(&contents) {
            continue;
        }
        mermaid_diagrams.insert(
            source_range.start,
            ParsedMarkdownMermaidDiagram {
                content_range: metadata.content_range.clone(),
                contents: ParsedMarkdownMermaidDiagramContents {
                    contents: contents.into(),
                    scale,
                },
            },
        );
    }

    mermaid_diagrams
}

/// The natural (zoom 1.0, unfitted) logical size of a diagram, recovered from
/// one of its rasters.
///
/// Mermaid rasters always come from `SvgRenderer::render_parsed`, whose images
/// have a device scale of [`SMOOTH_SVG_SCALE_FACTOR`]; dividing by it and by
/// the scale the raster was made at recovers the natural size, regardless of
/// which raster is currently cached.
fn mermaid_base_size(image: &RenderImage, rasterized_scale: f32) -> Size<Pixels> {
    let device_size = image.size(0);
    let device_scale = SMOOTH_SVG_SCALE_FACTOR * rasterized_scale;
    size(
        px(device_size.width.0 as f32 / device_scale),
        px(device_size.height.0 as f32 / device_scale),
    )
}

fn mermaid_display_size(base_size: Size<Pixels>, display_scale: f32) -> Size<Pixels> {
    size(
        base_size.width * display_scale,
        base_size.height * display_scale,
    )
}

/// The number of zoom ticks represented by a scroll-wheel event.
///
/// A discrete wheel notch arrives as one `Lines` event whose magnitude varies
/// by platform and device (1 or 3 lines are both common), so the delta is
/// clamped to a single tick: every notch is exactly one step. Precise (pixel)
/// deltas from trackpads arrive as many small events, each contributing a
/// fractional tick, so a gesture zooms smoothly at the same overall rate.
fn mermaid_zoom_ticks(delta: ScrollDelta) -> f32 {
    match delta {
        ScrollDelta::Lines(lines) => lines.y,
        ScrollDelta::Pixels(pixels) => f32::from(pixels.y) / PIXELS_PER_ZOOM_TICK,
    }
    .clamp(-1.0, 1.0)
}

fn on_mermaid_zoom_scroll(
    markdown: Entity<Markdown>,
    source_offset: usize,
    on_zoom: Option<MermaidZoomCallback>,
) -> impl Fn(&ScrollWheelEvent, &mut Window, &mut App) + 'static {
    move |event, window, cx| {
        if !(event.modifiers.control || event.modifiers.platform) {
            return;
        }
        let scroll_ticks = mermaid_zoom_ticks(event.delta);
        if scroll_ticks != 0.0 {
            let zoom_changed = markdown.update(cx, |markdown, cx| {
                let current_zoom = markdown.mermaid_zoom_level(source_offset);
                let new_zoom = current_zoom + scroll_ticks * MERMAID_ZOOM_STEP;
                markdown.set_mermaid_zoom_level(source_offset, new_zoom, cx);
                markdown.mermaid_zoom_level(source_offset) != current_zoom
            });
            // Only notify when the zoom actually changed. A no-op zoom (e.g.
            // clamped at the min/max) must not pause tail-following, since that
            // would disable following while still at the bottom.
            if zoom_changed && let Some(on_zoom) = &on_zoom {
                on_zoom(window, cx);
            }
        }
        cx.stop_propagation();
    }
}

pub(crate) fn render_mermaid_diagram(
    parsed: &ParsedMarkdownMermaidDiagram,
    mermaid_state: &MermaidState,
    style: &MarkdownStyle,
    markdown: Entity<Markdown>,
    source_offset: usize,
    showing_code: bool,
    zoom: f32,
    copy_button_visibility: CopyButtonVisibility,
    on_zoom: Option<MermaidZoomCallback>,
    on_edit: Option<MarkdownEditCallback>,
    on_label_edit: Option<MermaidLabelEditCallback>,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let cached = mermaid_state.cache.get(&parsed.contents);
    let content_range = parsed.content_range.clone();
    let geometry = cached.and_then(|cached| cached.geometry.get().cloned());
    let render_result = cached.and_then(|cached| cached.render_image.get());
    let show_interactive = copy_button_visibility != CopyButtonVisibility::Hidden;

    let code = parsed.contents.contents.clone();

    let mut container = div().group("code_block").relative().w_full().rounded_lg();
    container.style().refine(&style.code_block);

    match render_result {
        Some(Ok(render_image)) => {
            let body = if showing_code {
                render_mermaid_code_view(&parsed.contents.contents)
            } else {
                let rasterized_scale = cached.map_or(1.0, |cached| cached.rasterized_scale);
                let image_element =
                    img(ImageSource::Render(render_image.clone())).with_fallback(|| {
                        Label::new("Mermaid 図を読み込めませんでした。").into_any_element()
                    });
                let scroll_handle = markdown.update(cx, |markdown, _| {
                    markdown.mermaid_scroll_handle(source_offset)
                });
                let base_size = mermaid_base_size(render_image, rasterized_scale);
                let display_size = mermaid_display_size(base_size, zoom);
                let body = mermaid_scroll_container(
                    markdown.clone(),
                    source_offset,
                    &scroll_handle,
                    on_zoom.clone(),
                )
                .child(render_mermaid_image(
                    image_element,
                    display_size,
                    geometry,
                    parsed,
                    markdown.clone(),
                    source_offset,
                    on_label_edit,
                    cx,
                ))
                .into_any_element();
                with_mermaid_horizontal_scrollbar(source_offset, &scroll_handle, body, window, cx)
            };

            container
                .when(show_interactive, |container| {
                    container.child(render_mermaid_tab_header(
                        source_offset,
                        showing_code,
                        markdown.clone(),
                    ))
                })
                .child(body)
                .when(show_interactive, |container| {
                    container.child(render_mermaid_overlay_controls(
                        source_offset,
                        content_range,
                        code.to_string(),
                        (!showing_code && zoom != 1.0).then_some(zoom),
                        markdown,
                        on_zoom,
                        on_edit,
                    ))
                })
                .into_any_element()
        }
        Some(Err(error)) => {
            // Render failed — show the source code without tabs, prefixed by
            // the reason. Without it the block is indistinguishable from a
            // diagram that is merely still rendering.
            container
                .child(
                    div().w_full().pb_1().child(
                        Label::new(format!("Mermaid 図を描画できませんでした: {error}"))
                            .size(LabelSize::XSmall)
                            .color(Color::Error),
                    ),
                )
                .child(render_mermaid_code_view(&parsed.contents.contents))
                .when(show_interactive, |container| {
                    container.child(render_mermaid_overlay_controls(
                        source_offset,
                        content_range,
                        code.to_string(),
                        None,
                        markdown,
                        on_zoom,
                        on_edit,
                    ))
                })
                .into_any_element()
        }
        None => {
            // Still rendering
            if let Some((fallback, fallback_scale)) =
                cached.and_then(|cached| cached.fallback_image.clone())
            {
                let fallback_element =
                    img(ImageSource::Render(fallback.clone())).with_fallback(|| {
                        div()
                            .child(Label::new("Mermaid 図を読み込めませんでした。"))
                            .into_any_element()
                    });
                let scroll_handle = markdown.update(cx, |markdown, _| {
                    markdown.mermaid_scroll_handle(source_offset)
                });
                let base_size = mermaid_base_size(&fallback, fallback_scale);
                let display_size = mermaid_display_size(base_size, zoom);
                // The fallback is the prior raster shown at the new size while
                // the sharper one renders; keep it static so swapping rasters
                // (e.g. on zoom) is seamless rather than flashing.
                let inner = mermaid_scroll_container(
                    markdown.clone(),
                    source_offset,
                    &scroll_handle,
                    on_zoom.clone(),
                )
                .child(render_mermaid_image(
                    fallback_element,
                    display_size,
                    geometry,
                    parsed,
                    markdown.clone(),
                    source_offset,
                    on_label_edit,
                    cx,
                ))
                .into_any_element();
                let body = with_mermaid_horizontal_scrollbar(
                    source_offset,
                    &scroll_handle,
                    inner,
                    window,
                    cx,
                );
                container
                    .when(show_interactive, |container| {
                        container.child(render_mermaid_tab_header(
                            source_offset,
                            showing_code,
                            markdown.clone(),
                        ))
                    })
                    .child(body)
                    .when(show_interactive, |container| {
                        container.child(render_mermaid_overlay_controls(
                            source_offset,
                            content_range,
                            code.to_string(),
                            (zoom != 1.0).then_some(zoom),
                            markdown,
                            on_zoom,
                            on_edit,
                        ))
                    })
                    .into_any_element()
            } else {
                // No fallback — show the code so the user has something to look at
                container
                    .child(render_mermaid_code_view(&parsed.contents.contents))
                    .child(
                        div().absolute().top_1().right_2().child(
                            Label::new("レンダリング中...")
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .with_animation(
                                    "mermaid-loading-pulse",
                                    Animation::new(Duration::from_secs(2))
                                        .repeat()
                                        .with_easing(pulsating_between(0.4, 0.8)),
                                    |label, delta| label.alpha(delta),
                                ),
                        ),
                    )
                    .when(show_interactive, |container| {
                        container.child(render_mermaid_overlay_controls(
                            source_offset,
                            content_range,
                            code.to_string(),
                            None,
                            markdown,
                            on_zoom,
                            on_edit,
                        ))
                    })
                    .into_any_element()
            }
        }
    }
}

/// The horizontal scroll container wrapping a mermaid raster. The element id
/// and the [`ScrollHandle`] identity are both stable per source offset so the
/// scroll position survives raster swaps and re-rasters. Diagrams keep their
/// natural (zoomed) size and scroll instead of being crushed via max_w_full
/// (#61051).
fn mermaid_scroll_container(
    markdown: Entity<Markdown>,
    source_offset: usize,
    scroll_handle: &ScrollHandle,
    on_zoom: Option<MermaidZoomCallback>,
) -> Stateful<Div> {
    let mut container = div()
        .id(ElementId::named_usize(
            "mermaid-diagram-body",
            source_offset,
        ))
        .w_full()
        .overflow_x_scroll()
        .track_scroll(scroll_handle)
        .on_scroll_wheel(on_mermaid_zoom_scroll(markdown, source_offset, on_zoom));
    // Without this, gpui maps vertical wheel deltas onto the x axis for
    // x-only scroll containers (see `paint_scroll_listener` in gpui's div),
    // hijacking plain vertical scrolls. Restricting to the actual axis lets
    // vertical deltas propagate to the surrounding view, and means a
    // ctrl+vertical-scroll (zoom gesture) never moves this container either,
    // since its delta.x is zero.
    container.style().restrict_scroll_to_axis = Some(true);
    container
}

fn with_mermaid_horizontal_scrollbar(
    source_offset: usize,
    scroll_handle: &ScrollHandle,
    body: AnyElement,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    // Always show the scrollbar (rather than autohiding) so it's clear a
    // zoomed diagram can be scrolled. It only appears when the content
    // actually overflows, since the thumb isn't drawn when there's nothing to
    // scroll.
    let scrollbars = Scrollbars::always_visible(ScrollAxes::Horizontal)
        .id(("mermaid-diagram-scrollbar", source_offset))
        .tracked_scroll_handle(scroll_handle)
        .with_track_along(
            ScrollAxes::Horizontal,
            cx.theme().colors().editor_background,
        )
        .notify_content();

    div()
        .w_full()
        .custom_scrollbars(scrollbars, window, cx)
        .child(body)
        .into_any_element()
}

fn render_mermaid_tab_header(
    source_offset: usize,
    showing_code: bool,
    markdown: Entity<Markdown>,
) -> impl IntoElement {
    let preview_id = ElementId::NamedChild(
        Arc::new(ElementId::from((
            "mermaid-tab-preview",
            markdown.entity_id(),
        ))),
        source_offset.to_string().into(),
    );
    let code_id = ElementId::NamedChild(
        Arc::new(ElementId::from(("mermaid-tab-code", markdown.entity_id()))),
        source_offset.to_string().into(),
    );
    let preview_markdown = markdown.clone();
    let code_markdown = markdown;

    h_flex()
        .gap_0p5()
        .mb_2p5()
        .child(
            Button::new(preview_id, "プレビュー")
                .label_size(LabelSize::Small)
                .selected_style(ButtonStyle::Tinted(TintColor::Accent))
                .toggle_state(!showing_code)
                .on_click(move |_event, _window, cx| {
                    preview_markdown.update(cx, |md, cx| {
                        if md.is_mermaid_showing_code(source_offset) {
                            md.toggle_mermaid_tab(source_offset);
                            cx.notify();
                        }
                    });
                }),
        )
        .child(
            Button::new(code_id, "コード")
                .label_size(LabelSize::Small)
                .selected_style(ButtonStyle::Tinted(TintColor::Accent))
                .toggle_state(showing_code)
                .on_click(move |_event, _window, cx| {
                    code_markdown.update(cx, |md, cx| {
                        if !md.is_mermaid_showing_code(source_offset) {
                            md.toggle_mermaid_tab(source_offset);
                            cx.notify();
                        }
                    });
                }),
        )
}

/// The overlay controls anchored to the top-right corner of a diagram: an
/// optional "Zoom NNN%" readout with a reset button (shown only while zoomed
/// away from the natural size) followed by the hover-revealed copy button.
fn render_mermaid_overlay_controls(
    source_offset: usize,
    content_range: Range<usize>,
    code: String,
    zoom: Option<f32>,
    markdown: Entity<Markdown>,
    on_zoom: Option<MermaidZoomCallback>,
    on_edit: Option<MarkdownEditCallback>,
) -> impl IntoElement {
    h_flex()
        .absolute()
        .top_1()
        .right_1()
        .gap_0p5()
        .when_some(zoom, |this, zoom| {
            this.child(render_mermaid_zoom_indicator(
                source_offset,
                zoom,
                markdown.clone(),
                on_zoom,
            ))
        })
        .when_some(on_edit, |this, on_edit| {
            this.child(render_mermaid_edit_button(
                source_offset,
                content_range,
                on_edit,
            ))
        })
        .child(render_mermaid_copy_button(source_offset, code, markdown))
}

/// Opens the structured editor on the fence this diagram was drawn from. Only
/// offered when the preview knows which editor holds the source, which is what
/// the callback carries. DESIGN.md §5.7
fn render_mermaid_edit_button(
    source_offset: usize,
    content_range: Range<usize>,
    on_edit: MarkdownEditCallback,
) -> impl IntoElement {
    IconButton::new(
        ElementId::named_usize("mermaid-edit", source_offset),
        IconName::Pencil,
    )
    .icon_size(IconSize::Small)
    .icon_color(Color::Muted)
    .visible_on_hover("code_block")
    .tooltip(Tooltip::text("Mermaid 図を編集"))
    .on_click(move |_event, window, cx| {
        on_edit(content_range.clone(), window, cx);
    })
}

/// A "Zoom NNN%" readout paired with a reset button, styled like the adjacent
/// copy button. Shown only while the diagram is zoomed away from its natural
/// size, so it doubles as an affordance that the zoom can be reset.
fn render_mermaid_zoom_indicator(
    source_offset: usize,
    zoom: f32,
    markdown: Entity<Markdown>,
    on_zoom: Option<MermaidZoomCallback>,
) -> impl IntoElement {
    let percentage = (zoom * 100.0).round() as i32;

    h_flex()
        .gap_0p5()
        .child(
            Label::new(format!("ズーム {percentage}%"))
                .size(LabelSize::Small)
                .color(Color::Muted),
        )
        .child(
            IconButton::new(
                ElementId::named_usize("mermaid-zoom-reset", source_offset),
                IconName::RotateCcw,
            )
            .icon_size(IconSize::Small)
            .icon_color(Color::Muted)
            .tooltip(Tooltip::text("ズームをリセット"))
            .on_click(move |_event, window, cx| {
                let zoom_changed = markdown.update(cx, |markdown, cx| {
                    let current_zoom = markdown.mermaid_zoom_level(source_offset);
                    markdown.set_mermaid_zoom_level(source_offset, 1.0, cx);
                    markdown.mermaid_zoom_level(source_offset) != current_zoom
                });
                if zoom_changed && let Some(on_zoom) = &on_zoom {
                    on_zoom(window, cx);
                }
            }),
        )
}

fn render_mermaid_copy_button(
    source_offset: usize,
    code: String,
    markdown: Entity<Markdown>,
) -> impl IntoElement {
    let id = ElementId::NamedChild(
        Arc::new(ElementId::from(("copy-mermaid-code", markdown.entity_id()))),
        source_offset.to_string().into(),
    );

    CopyButton::new(id.clone(), code.clone())
        .visible_on_hover("code_block")
        .custom_on_click({
            move |_window, cx| {
                let id = id.clone();
                markdown.update(cx, |this, cx| {
                    this.copied_code_blocks.insert(id.clone());
                    cx.write_to_clipboard(ClipboardItem::new_string(code.clone()));
                    cx.spawn(async move |this, cx| {
                        cx.background_executor().timer(Duration::from_secs(2)).await;
                        cx.update(|cx| {
                            this.update(cx, |this, cx| {
                                this.copied_code_blocks.remove(&id);
                                cx.notify();
                            })
                        })
                        .ok();
                    })
                    .detach();
                });
            }
        })
}

fn render_mermaid_code_view(contents: &SharedString) -> AnyElement {
    div()
        .w_full()
        .child(StyledText::new(contents.clone()))
        .into_any_element()
}

/// A label in a rendered diagram that the preview can edit in place.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum MermaidLabelTarget {
    Node {
        node_id: SharedString,
    },
    Edge {
        from: SharedString,
        to: SharedString,
        /// Which edge this is among those connecting the same two nodes, in
        /// source order.
        occurrence: usize,
    },
    /// A connection of a diagram whose SVG numbers its connections instead of
    /// naming their ends (a state diagram's transitions).
    Transition {
        index: usize,
    },
    /// A label that belongs to neither a node nor an edge (a chart's title, a
    /// pie slice, a gantt task or section, an ER attribute), with the text it
    /// is drawn with, which the source scan checks its token against.
    Label {
        item: LabelItem,
        label: SharedString,
    },
    /// One of several boxes drawn for the same node, counting from the first
    /// in document order (a sequence diagram draws each participant at the top
    /// and again at the bottom). The first box is a plain [`Self::Node`].
    NodeCopy {
        node_id: SharedString,
        copy: usize,
    },
}

/// Maps a point in window coordinates onto the diagram's own `viewBox`.
///
/// `bounds` is where the raster sits on screen, so it already carries both the
/// zoom (it is the displayed size, not the natural one) and the horizontal
/// scroll position (the origin moves with the scroll container). Neither needs
/// a term of its own here.
fn mermaid_view_box_point(
    position: Point<Pixels>,
    bounds: Bounds<Pixels>,
    view_box: [f32; 4],
) -> Option<Point<f32>> {
    let scale = mermaid_view_box_scale(bounds, view_box)?;
    Some(point(
        view_box[0] + f32::from(position.x - bounds.origin.x) / scale,
        view_box[1] + f32::from(position.y - bounds.origin.y) / scale,
    ))
}

/// The inverse of [`mermaid_view_box_point`] for a whole rectangle.
fn mermaid_window_rect(
    rect: [f32; 4],
    bounds: Bounds<Pixels>,
    view_box: [f32; 4],
) -> Option<Bounds<Pixels>> {
    let scale = mermaid_view_box_scale(bounds, view_box)?;
    Some(Bounds {
        origin: gpui::point(
            bounds.origin.x + px((rect[0] - view_box[0]) * scale),
            bounds.origin.y + px((rect[1] - view_box[1]) * scale),
        ),
        size: size(px(rect[2] * scale), px(rect[3] * scale)),
    })
}

/// Displayed pixels per `viewBox` unit. The rasterizer preserves the aspect
/// ratio, so one factor covers both axes.
fn mermaid_view_box_scale(bounds: Bounds<Pixels>, view_box: [f32; 4]) -> Option<f32> {
    if view_box[2] <= 0.0 {
        return None;
    }
    let scale = f32::from(bounds.size.width) / view_box[2];
    (scale.is_finite() && scale > 0.0).then_some(scale)
}

fn rect_contains(rect: [f32; 4], point: Point<f32>) -> bool {
    point.x >= rect[0]
        && point.y >= rect[1]
        && point.x <= rect[0] + rect[2]
        && point.y <= rect[1] + rect[3]
}

/// The label under `point`, if any.
///
/// Edge labels are tested first: they are small, they are drawn over the edges
/// rather than over a node, and preferring them keeps a label that happens to
/// touch a node box reachable.
fn mermaid_label_at(geometry: &DiagramGeometry, point: Point<f32>) -> Option<MermaidLabelTarget> {
    for edge in &geometry.edges {
        if edge
            .label_rect
            .is_some_and(|rect| rect_contains(rect, point))
        {
            return Some(match edge.statement_index {
                Some(index) => MermaidLabelTarget::Transition { index },
                None => MermaidLabelTarget::Edge {
                    from: edge.from.clone().into(),
                    to: edge.to.clone().into(),
                    occurrence: edge.occurrence,
                },
            });
        }
    }
    // An ER attribute's cell lies inside its entity's box, so the smaller
    // target has to win.
    if let Some(label) = geometry
        .labels
        .iter()
        .find(|label| rect_contains(label.rect, point))
    {
        return Some(MermaidLabelTarget::Label {
            item: label.item.clone(),
            label: label.label.clone().into(),
        });
    }
    let (index, node) = geometry
        .nodes
        .iter()
        .enumerate()
        .find(|(_, node)| rect_contains(node.rect, point))?;
    let copy = geometry.nodes[..index]
        .iter()
        .filter(|earlier| earlier.node_id == node.node_id)
        .count();
    Some(match copy {
        0 => MermaidLabelTarget::Node {
            node_id: node.node_id.clone().into(),
        },
        copy => MermaidLabelTarget::NodeCopy {
            node_id: node.node_id.clone().into(),
            copy,
        },
    })
}

/// The `copy`-th box drawn for `node_id`, counting from zero.
fn mermaid_node_copy<'a>(
    geometry: &'a DiagramGeometry,
    node_id: &str,
    copy: usize,
) -> Option<&'a mermaid_render::NodeGeometry> {
    geometry
        .nodes
        .iter()
        .filter(|node| node.node_id == node_id)
        .nth(copy)
}

fn mermaid_label_rect(geometry: &DiagramGeometry, target: &MermaidLabelTarget) -> Option<[f32; 4]> {
    match target {
        MermaidLabelTarget::Node { node_id } => geometry
            .nodes
            .iter()
            .find(|node| node.node_id == node_id.as_ref())
            .map(|node| node.rect),
        MermaidLabelTarget::Edge {
            from,
            to,
            occurrence,
        } => geometry
            .edges
            .iter()
            .find(|edge| {
                edge.from == from.as_ref()
                    && edge.to == to.as_ref()
                    && edge.occurrence == *occurrence
            })
            .and_then(|edge| edge.label_rect),
        MermaidLabelTarget::Transition { index } => geometry
            .edges
            .iter()
            .find(|edge| edge.statement_index == Some(*index))
            .and_then(|edge| edge.label_rect),
        MermaidLabelTarget::Label { item, .. } => geometry
            .labels
            .iter()
            .find(|label| &label.item == item)
            .map(|label| label.rect),
        MermaidLabelTarget::NodeCopy { node_id, copy } => {
            mermaid_node_copy(geometry, node_id, *copy).map(|node| node.rect)
        }
    }
}

/// The label as the diagram draws it, used only when the matching token cannot
/// be found in the source.
fn mermaid_rendered_label(geometry: &DiagramGeometry, target: &MermaidLabelTarget) -> String {
    match target {
        MermaidLabelTarget::Node { node_id } => geometry
            .nodes
            .iter()
            .find(|node| node.node_id == node_id.as_ref())
            .map(|node| node.label.clone())
            .unwrap_or_default(),
        MermaidLabelTarget::Edge {
            from,
            to,
            occurrence,
        } => geometry
            .edges
            .iter()
            .find(|edge| {
                edge.from == from.as_ref()
                    && edge.to == to.as_ref()
                    && edge.occurrence == *occurrence
            })
            .and_then(|edge| edge.label.clone())
            .unwrap_or_default(),
        MermaidLabelTarget::Transition { index } => geometry
            .edges
            .iter()
            .find(|edge| edge.statement_index == Some(*index))
            .and_then(|edge| edge.label.clone())
            .unwrap_or_default(),
        MermaidLabelTarget::Label { label, .. } => label.to_string(),
        MermaidLabelTarget::NodeCopy { node_id, copy } => {
            mermaid_node_copy(geometry, node_id, *copy)
                .map(|node| node.label.clone())
                .unwrap_or_default()
        }
    }
}

fn mermaid_source_span(
    fence_body: &str,
    target: &MermaidLabelTarget,
) -> mermaid_source::MermaidLabelLookup {
    match target {
        MermaidLabelTarget::Node { node_id } => {
            mermaid_source::find_node_label(fence_body, node_id)
        }
        MermaidLabelTarget::Edge {
            from,
            to,
            occurrence,
        } => mermaid_source::find_edge_label(fence_body, from, to, *occurrence),
        MermaidLabelTarget::Transition { index } => {
            mermaid_source::find_transition_label(fence_body, *index)
        }
        MermaidLabelTarget::Label { item, label } => {
            mermaid_source::find_item_label(fence_body, item, label)
        }
        // Every box of a node is drawn from the same token.
        MermaidLabelTarget::NodeCopy { node_id, .. } => {
            mermaid_source::find_node_label(fence_body, node_id)
        }
    }
}

/// The diagram's raster, with a hit-testing layer on top when the consumer can
/// write label edits back to a source file.
///
/// Without that callback (the agent panel, hover popups, a read-only buffer)
/// this is just the image, so those surfaces keep behaving exactly as before.
fn render_mermaid_image(
    image: Img,
    display_size: Size<Pixels>,
    geometry: Option<Arc<DiagramGeometry>>,
    parsed: &ParsedMarkdownMermaidDiagram,
    markdown: Entity<Markdown>,
    source_offset: usize,
    on_label_edit: Option<MermaidLabelEditCallback>,
    cx: &mut App,
) -> AnyElement {
    let image = image.w(display_size.width).h(display_size.height);
    let (Some(geometry), Some(on_label_edit)) = (geometry, on_label_edit) else {
        return image.into_any_element();
    };
    if geometry.is_empty() {
        return image.into_any_element();
    }

    let view_box = geometry.view_box;
    let hovered = markdown
        .read(cx)
        .mermaid_hovered_label(source_offset)
        .cloned();
    let highlight = hovered
        .as_ref()
        .and_then(|target| mermaid_label_rect(&geometry, target));
    let highlight_color = cx.theme().colors().text_accent;
    // Written during prepaint, read by the mouse handlers of the next event.
    let image_bounds: Rc<Cell<Option<Bounds<Pixels>>>> = Rc::default();

    let overlay = div()
        .id(ElementId::named_usize(
            "mermaid-label-hit-layer",
            source_offset,
        ))
        // The layer covers exactly the raster, so a test can map a label's
        // `viewBox` rectangle onto the window without knowing the layout above
        // it. Outside a debug build the selector compiles to nothing.
        .debug_selector(|| format!("mermaid-label-hit-layer-{source_offset}"))
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .when(hovered.is_some(), |this| {
            this.cursor_pointer()
                .tooltip(Tooltip::text("ダブルクリックで文字を編集"))
        })
        .child(
            canvas(
                {
                    let image_bounds = image_bounds.clone();
                    move |bounds, _window, _cx| image_bounds.set(Some(bounds))
                },
                move |bounds, _prepaint, window, _cx| {
                    let Some(rect) =
                        highlight.and_then(|rect| mermaid_window_rect(rect, bounds, view_box))
                    else {
                        return;
                    };
                    window.paint_quad(quad(
                        rect,
                        4.0,
                        gpui::transparent_black(),
                        2.0,
                        highlight_color,
                        BorderStyle::Solid,
                    ));
                },
            )
            .absolute()
            .top_0()
            .left_0()
            .size_full(),
        )
        .on_mouse_move({
            let markdown = markdown.clone();
            let geometry = geometry.clone();
            let image_bounds = image_bounds.clone();
            move |event: &MouseMoveEvent, _window, cx| {
                let target = image_bounds
                    .get()
                    .and_then(|bounds| mermaid_view_box_point(event.position, bounds, view_box))
                    .and_then(|position| mermaid_label_at(&geometry, position));
                markdown.update(cx, |markdown, cx| {
                    markdown.set_mermaid_hovered_label(source_offset, target, cx);
                });
            }
        })
        .on_mouse_exit({
            move |_event, _window, cx| {
                markdown.update(cx, |markdown, cx| {
                    markdown.set_mermaid_hovered_label(source_offset, None, cx);
                });
            }
        })
        .on_mouse_down(MouseButton::Left, {
            let fence_body = parsed.contents.contents.clone();
            let content_start = parsed.content_range.start;
            move |event: &MouseDownEvent, window, cx| {
                if event.click_count < 2 {
                    return;
                }
                let Some(bounds) = image_bounds.get() else {
                    return;
                };
                let Some(target) = mermaid_view_box_point(event.position, bounds, view_box)
                    .and_then(|position| mermaid_label_at(&geometry, position))
                else {
                    return;
                };
                let Some(label_bounds) = mermaid_label_rect(&geometry, &target)
                    .and_then(|rect| mermaid_window_rect(rect, bounds, view_box))
                else {
                    return;
                };
                let span = mermaid_source_span(&fence_body, &target);
                let request = MermaidLabelEditRequest {
                    source_range: span.as_ref().ok().map(|span| {
                        (content_start + span.range.start)..(content_start + span.range.end)
                    }),
                    source_text: span
                        .as_ref()
                        .ok()
                        .and_then(|span| fence_body.get(span.range.clone()))
                        .unwrap_or_default()
                        .to_string()
                        .into(),
                    quoting: span
                        .as_ref()
                        .map_or(MermaidLabelQuoting::Delimited, |span| span.quoting),
                    refusal: span.as_ref().err().copied(),
                    reserved_labels: match &target {
                        MermaidLabelTarget::Label { item, .. } => {
                            mermaid_source::reserved_labels(&fence_body, item)
                        }
                        _ => Vec::new(),
                    },
                    current_label: match &span {
                        Ok(span) => span.label.clone().into(),
                        Err(_) => mermaid_rendered_label(&geometry, &target).into(),
                    },
                    bounds: label_bounds,
                };
                // Without this the preview would also move the source cursor to
                // the fence, stealing focus from the field about to open.
                cx.stop_propagation();
                on_label_edit(request, window, cx);
            }
        });

    div()
        .relative()
        .w(display_size.width)
        .h(display_size.height)
        .child(image)
        .child(overlay)
        .into_any_element()
}

#[cfg(test)]
mod chart_label_tests;

#[cfg(test)]
mod tests {
    use super::{
        CachedMermaidDiagram, MermaidDiagramCache, MermaidState, ParsedMarkdownMermaidDiagram,
        ParsedMarkdownMermaidDiagramContents, extract_mermaid_diagrams, parse_mermaid_info,
    };
    use crate::{
        CodeBlockRenderer, CopyButtonVisibility, MERMAID_ZOOM_DEBOUNCE, Markdown, MarkdownElement,
        MarkdownOptions, MarkdownStyle, WrapButtonVisibility,
    };
    use collections::HashMap;
    use gpui::{
        Context, Entity, IntoElement, Render, RenderImage, TestAppContext, Window, point, size,
    };
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::Arc;
    use std::time::Duration;
    use ui::prelude::*;

    #[test]
    fn spellings_merman_accepts_are_previewed() {
        for source in [
            "classDiagram-v2\n",
            "requirementDiagram\n",
            "requirementdiagram\n",
            "ishikawa\n",
            "Ishikawa\n",
            "ISHIKAWA-BETA\n",
            "architecture-beta\n",
            "treeView-beta\n",
            "eventmodeling\n",
            "venn-beta\n",
            "C4Deployment\n",
        ] {
            assert!(
                super::is_supported_diagram_type(source),
                "{source:?} should be previewed"
            );
        }
    }

    mod label_editing {
        use super::super::{
            MermaidLabelTarget, mermaid_label_at, mermaid_label_rect, mermaid_source_span,
            mermaid_view_box_point, mermaid_window_rect,
        };
        use crate::mermaid_source::MermaidLabelQuoting;
        use gpui::{Bounds, point, px, size};
        use mermaid_render::{DiagramGeometry, EdgeGeometry, NodeGeometry};

        /// A two-node diagram whose `viewBox` is 200x100, with one node in each
        /// half and an edge label between them.
        fn geometry() -> DiagramGeometry {
            DiagramGeometry {
                view_box: [0.0, 0.0, 200.0, 100.0],
                nodes: vec![
                    NodeGeometry {
                        node_id: "A".into(),
                        label: "\u{958b}\u{59cb}".into(),
                        rect: [10.0, 20.0, 40.0, 30.0],
                    },
                    NodeGeometry {
                        node_id: "B".into(),
                        label: "\u{7d42}\u{4e86}".into(),
                        rect: [150.0, 20.0, 40.0, 30.0],
                    },
                ],
                edges: vec![EdgeGeometry {
                    edge_id: "L_A_B_0".into(),
                    from: "A".into(),
                    to: "B".into(),
                    occurrence: 0,
                    statement_index: None,
                    label: Some("\u{306f}\u{3044}".into()),
                    label_rect: Some([90.0, 30.0, 20.0, 10.0]),
                }],
                labels: Vec::new(),
            }
        }

        fn bounds(origin_x: f32, origin_y: f32, zoom: f32) -> Bounds<gpui::Pixels> {
            Bounds {
                origin: point(px(origin_x), px(origin_y)),
                size: size(px(200.0 * zoom), px(100.0 * zoom)),
            }
        }

        /// The mapping has to account for where the raster is on screen and for
        /// the zoom it is displayed at. Both are carried by the raster's own
        /// bounds: scrolling moves the origin, zooming changes the size.
        #[test]
        fn window_points_map_into_view_box_coordinates() {
            // Unzoomed, at the window origin: one displayed pixel is one unit.
            let unzoomed = bounds(0.0, 0.0, 1.0);
            assert_eq!(
                mermaid_view_box_point(
                    point(px(30.0), px(35.0)),
                    unzoomed,
                    [0.0, 0.0, 200.0, 100.0]
                ),
                Some(point(30.0, 35.0))
            );

            // Zoomed to 2x, the same unit is two displayed pixels.
            let zoomed = bounds(0.0, 0.0, 2.0);
            assert_eq!(
                mermaid_view_box_point(point(px(60.0), px(70.0)), zoomed, [0.0, 0.0, 200.0, 100.0]),
                Some(point(30.0, 35.0))
            );

            // Scrolled right by 120 displayed pixels while zoomed: the raster's
            // origin has moved off the left edge of the viewport.
            let scrolled = bounds(-120.0, 8.0, 2.0);
            assert_eq!(
                mermaid_view_box_point(
                    point(px(-60.0), px(78.0)),
                    scrolled,
                    [0.0, 0.0, 200.0, 100.0]
                ),
                Some(point(30.0, 35.0))
            );

            // A `viewBox` that does not start at the origin shifts the result.
            assert_eq!(
                mermaid_view_box_point(
                    point(px(30.0), px(35.0)),
                    unzoomed,
                    [-50.0, -10.0, 200.0, 100.0]
                ),
                Some(point(-20.0, 25.0))
            );

            // A degenerate diagram has no usable mapping rather than a bogus one.
            assert_eq!(
                mermaid_view_box_point(point(px(1.0), px(1.0)), unzoomed, [0.0, 0.0, 0.0, 0.0]),
                None
            );
        }

        #[test]
        fn label_boxes_map_back_out_to_the_window() {
            let view_box = [0.0, 0.0, 200.0, 100.0];
            let mapped =
                mermaid_window_rect([10.0, 20.0, 40.0, 30.0], bounds(-120.0, 8.0, 2.0), view_box)
                    .expect("the rect should map");
            assert_eq!(mapped.origin, point(px(-100.0), px(48.0)));
            assert_eq!(mapped.size, size(px(80.0), px(60.0)));

            // Round-tripping a corner through both directions lands back where
            // it started, which is what keeps the highlight on the label.
            let round_tripped =
                mermaid_view_box_point(mapped.origin, bounds(-120.0, 8.0, 2.0), view_box);
            assert_eq!(round_tripped, Some(point(10.0, 20.0)));
        }

        #[test]
        fn hit_testing_picks_the_label_under_the_pointer() {
            let geometry = geometry();
            assert_eq!(
                mermaid_label_at(&geometry, point(30.0, 35.0)),
                Some(MermaidLabelTarget::Node {
                    node_id: "A".into()
                })
            );
            assert_eq!(
                mermaid_label_at(&geometry, point(170.0, 35.0)),
                Some(MermaidLabelTarget::Node {
                    node_id: "B".into()
                })
            );
            assert_eq!(
                mermaid_label_at(&geometry, point(95.0, 35.0)),
                Some(MermaidLabelTarget::Edge {
                    from: "A".into(),
                    to: "B".into(),
                    occurrence: 0,
                })
            );
            // Between the boxes there is nothing to edit.
            assert_eq!(mermaid_label_at(&geometry, point(70.0, 90.0)), None);
            assert_eq!(mermaid_label_at(&geometry, point(9.0, 35.0)), None);
        }

        #[test]
        fn a_target_reports_the_box_it_was_found_in() {
            let geometry = geometry();
            assert_eq!(
                mermaid_label_rect(
                    &geometry,
                    &MermaidLabelTarget::Node {
                        node_id: "B".into()
                    }
                ),
                Some([150.0, 20.0, 40.0, 30.0])
            );
            assert_eq!(
                mermaid_label_rect(
                    &geometry,
                    &MermaidLabelTarget::Edge {
                        from: "A".into(),
                        to: "B".into(),
                        occurrence: 0,
                    }
                ),
                Some([90.0, 30.0, 20.0, 10.0])
            );
            // A diagram that has been re-rendered since the pointer moved may no
            // longer contain the target.
            assert_eq!(
                mermaid_label_rect(
                    &geometry,
                    &MermaidLabelTarget::Node {
                        node_id: "gone".into()
                    }
                ),
                None
            );
        }

        /// A hit-tested target has to resolve to the token in the fence body,
        /// which is what the edit actually replaces.
        #[test]
        fn targets_resolve_to_the_token_in_the_fence() {
            let fence_body =
                "flowchart LR\n  A[\u{958b}\u{59cb}] -->|\u{306f}\u{3044}| B[\u{7d42}\u{4e86}]";
            let node = mermaid_source_span(
                fence_body,
                &MermaidLabelTarget::Node {
                    node_id: "B".into(),
                },
            )
            .expect("the node label should be found");
            assert_eq!(&fence_body[node.range.clone()], "\u{7d42}\u{4e86}");
            assert_eq!(node.quoting, MermaidLabelQuoting::Delimited);

            let edge = mermaid_source_span(
                fence_body,
                &MermaidLabelTarget::Edge {
                    from: "A".into(),
                    to: "B".into(),
                    occurrence: 0,
                },
            )
            .expect("the edge label should be found");
            assert_eq!(&fence_body[edge.range], "\u{306f}\u{3044}");

            assert_eq!(
                mermaid_source_span(
                    fence_body,
                    &MermaidLabelTarget::Node {
                        node_id: "missing".into()
                    }
                ),
                Err(crate::mermaid_source::MermaidLabelRefusal::NotLocated)
            );
        }
    }

    /// Hovering has to repaint only when the highlighted label actually
    /// changes, since the pointer moving across one node produces an event per
    /// frame.
    #[gpui::test]
    fn test_mermaid_hover_state_only_changes_on_a_new_label(cx: &mut TestAppContext) {
        use super::MermaidLabelTarget;

        ensure_theme_initialized(cx);
        let markdown = cx.new(|cx| {
            Markdown::new(
                "```mermaid\nflowchart LR\n  A[one] --> B[two]\n```".into(),
                None,
                None,
                cx,
            )
        });
        let node_a = MermaidLabelTarget::Node {
            node_id: "A".into(),
        };
        let node_b = MermaidLabelTarget::Node {
            node_id: "B".into(),
        };

        // Counting the notifications is the point: the return value alone
        // would not catch a `cx.notify()` that escaped the guard.
        let repaints = std::rc::Rc::new(std::cell::Cell::new(0usize));
        let subscription = cx.update(|cx| {
            cx.observe(&markdown, {
                let repaints = repaints.clone();
                move |_, _| repaints.set(repaints.get() + 1)
            })
        });

        let set = |markdown: &Entity<Markdown>,
                   cx: &mut TestAppContext,
                   target: Option<MermaidLabelTarget>| {
            markdown.update(cx, |markdown, cx| {
                markdown.set_mermaid_hovered_label(0, target, cx)
            })
        };

        assert!(markdown.read_with(cx, |markdown, _| {
            markdown.mermaid_hovered_label(0).is_none()
        }));
        // The background parse notifies once it lands; start counting after it.
        cx.run_until_parked();
        repaints.set(0);
        let mut expected_repaints = 0;

        assert!(set(&markdown, cx, Some(node_a.clone())));
        expected_repaints += 1;
        cx.run_until_parked();
        assert_eq!(repaints.get(), expected_repaints);

        // The pointer moving within the same node produces an event per frame,
        // and none of them may repaint.
        for _ in 0..5 {
            assert!(!set(&markdown, cx, Some(node_a.clone())));
        }
        cx.run_until_parked();
        assert_eq!(repaints.get(), expected_repaints);
        assert_eq!(
            markdown.read_with(cx, |markdown, _| markdown.mermaid_hovered_label(0).cloned()),
            Some(node_a)
        );

        assert!(set(&markdown, cx, Some(node_b)));
        expected_repaints += 1;
        assert!(set(&markdown, cx, None));
        expected_repaints += 1;
        assert!(!set(&markdown, cx, None));
        cx.run_until_parked();
        assert_eq!(repaints.get(), expected_repaints);

        // Diagrams are keyed by their own source offset.
        assert!(markdown.read_with(cx, |markdown, _| {
            markdown.mermaid_hovered_label(999).is_none()
        }));
        drop(subscription);
    }

    /// The one test that drives the whole path: a double-click on the raster
    /// has to land on the label under the pointer and reach the consumer with
    /// the token the fence spells it with. Every piece below it is tested on
    /// its own, but nothing else checks that the geometry, the hit layer's
    /// bounds and the source scan agree about which label was clicked.
    #[gpui::test]
    fn test_double_clicking_a_label_reaches_the_callback(cx: &mut TestAppContext) {
        struct TestWindow;

        impl Render for TestWindow {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div()
            }
        }

        struct EditableMermaid {
            markdown: Entity<Markdown>,
            requests: Rc<RefCell<Vec<Recorded>>>,
        }

        impl Render for EditableMermaid {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                let requests = self.requests.clone();
                div().child(
                    MarkdownElement::new(self.markdown.clone(), MarkdownStyle::default())
                        .code_block_renderer(CodeBlockRenderer::Default {
                            copy_button_visibility: CopyButtonVisibility::Hidden,
                            wrap_button_visibility: WrapButtonVisibility::Hidden,
                            border: false,
                        })
                        .on_mermaid_label_edit(move |request, _window, _cx| {
                            requests.borrow_mut().push((
                                request.source_range(),
                                request.source_text().to_string(),
                                request.current_label.to_string(),
                                request.refusal_message(),
                            ));
                        }),
                )
            }
        }

        /// What the field opens on when the token could not be found at all.
        const NOT_LOCATED: &str = "\u{3053}\u{306e}\u{30e9}\u{30d9}\u{30eb}\u{306f}\u{30bd}\u{30fc}\u{30b9}\u{306e}\u{4f4d}\u{7f6e}\u{3092}\u{7279}\u{5b9a}\u{3067}\u{304d}\u{307e}\u{305b}\u{3093}\u{3067}\u{3057}\u{305f}\u{3002}";
        type Recorded = (Option<std::ops::Range<usize>>, String, String, &'static str);

        ensure_theme_initialized(cx);
        let source = concat!(
            "```mermaid\n",
            "flowchart LR\n",
            "  A[\u{958b}\u{59cb}] -->|\u{306f}\u{3044}| B[\u{7d42}\u{4e86}]\n",
            "  A & B -->|\u{5171}\u{6709}| C[\u{6b21}]\n",
            "```",
        );

        let (_, cx) = cx.add_window_view(|_, _| TestWindow);
        let markdown = cx.new(|cx| {
            Markdown::new_with_options(
                source.into(),
                None,
                None,
                MarkdownOptions {
                    render_mermaid_diagrams: true,
                    ..Default::default()
                },
                cx,
            )
        });
        cx.run_until_parked();

        let (source_offset, contents) = markdown.read_with(cx, |markdown, _| {
            let (offset, diagram) = markdown
                .parsed_markdown
                .mermaid_diagrams
                .iter()
                .next()
                .expect("the fence should have been extracted");
            (*offset, diagram.contents.clone())
        });
        assert_eq!(source_offset, 0, "the fence starts the document");

        // The raster and the geometry are the ones merman produces, so the
        // boxes the test clicks in are the ones the user sees.
        let (image, geometry) = markdown.update(cx, |_, cx| {
            let (svg, geometry) = mermaid_render::render_to_svg_with_geometry(
                &contents.contents,
                &super::build_mermaid_theme(cx),
            )
            .expect("the diagram should render");
            let parsed = Arc::new(
                cx.svg_renderer()
                    .parse_svg(svg.as_bytes())
                    .expect("the SVG should parse"),
            );
            let image = cx
                .svg_renderer()
                .render_parsed(&parsed, 1.0)
                .expect("the SVG should rasterize");
            (image, Arc::new(geometry))
        });
        markdown.update(cx, |markdown, _| {
            markdown.mermaid_state.cache.insert(
                contents.clone(),
                Arc::new(CachedMermaidDiagram::new_for_test(
                    Some(image),
                    None,
                    None,
                    Some(geometry.clone()),
                )),
            );
            markdown.mermaid_state.order = vec![contents.clone()];
        });

        let requests = Rc::new(RefCell::new(Vec::new()));
        let center_of = |rect: [f32; 4], bounds: gpui::Bounds<Pixels>| {
            let scale = f32::from(bounds.size.width) / geometry.view_box[2];
            point(
                bounds.origin.x + px((rect[0] + rect[2] / 2.0 - geometry.view_box[0]) * scale),
                bounds.origin.y + px((rect[1] + rect[3] / 2.0 - geometry.view_box[1]) * scale),
            )
        };
        let draw = |cx: &mut gpui::VisualTestContext| {
            let markdown = markdown.clone();
            let requests = requests.clone();
            cx.draw(
                Default::default(),
                size(px(600.0), px(600.0)),
                move |_, cx| {
                    cx.new(|_| EditableMermaid { markdown, requests })
                        .into_any_element()
                },
            );
            cx.debug_bounds("mermaid-label-hit-layer-0")
                .expect("the hit layer should have been painted over the raster")
        };

        let node = geometry
            .nodes
            .iter()
            .find(|node| node.node_id == "B")
            .expect("B should be mapped");
        let bounds = draw(cx);
        cx.simulate_event(gpui::MouseDownEvent {
            position: center_of(node.rect, bounds),
            button: gpui::MouseButton::Left,
            modifiers: Default::default(),
            click_count: 2,
            first_mouse: false,
        });
        let expected = source
            .find("\u{7d42}\u{4e86}")
            .expect("the label is in the source");
        assert_eq!(
            requests.borrow().as_slice(),
            [(
                Some(expected..expected + "\u{7d42}\u{4e86}".len()),
                "\u{7d42}\u{4e86}".to_string(),
                "\u{7d42}\u{4e86}".to_string(),
                NOT_LOCATED
            )],
            "double-clicking B should offer the token B is drawn with"
        );

        // An edge label is a target of its own, and a single click is not an
        // edit at all.
        requests.borrow_mut().clear();
        let edge_rect = geometry
            .edges
            .first()
            .and_then(|edge| edge.label_rect)
            .expect("the edge label should be mapped");
        let bounds = draw(cx);
        cx.simulate_event(gpui::MouseDownEvent {
            position: center_of(edge_rect, bounds),
            button: gpui::MouseButton::Left,
            modifiers: Default::default(),
            click_count: 1,
            first_mouse: false,
        });
        assert!(
            requests.borrow().is_empty(),
            "a single click must not open the field"
        );
        let bounds = draw(cx);
        cx.simulate_event(gpui::MouseDownEvent {
            position: center_of(edge_rect, bounds),
            button: gpui::MouseButton::Left,
            modifiers: Default::default(),
            click_count: 2,
            first_mouse: false,
        });
        let expected = source
            .find("\u{306f}\u{3044}")
            .expect("the label is in the source");
        assert_eq!(
            requests.borrow().as_slice(),
            [(
                Some(expected..expected + "\u{306f}\u{3044}".len()),
                "\u{306f}\u{3044}".to_string(),
                "\u{306f}\u{3044}".to_string(),
                NOT_LOCATED
            )]
        );

        // The two connections an `&` group expands to are drawn with one token,
        // so the request carries the reason instead of a range to overwrite.
        requests.borrow_mut().clear();
        let shared_rect = geometry
            .edges
            .iter()
            .find(|edge| edge.label.as_deref() == Some("\u{5171}\u{6709}"))
            .and_then(|edge| edge.label_rect)
            .expect("the shared label should be mapped");
        let bounds = draw(cx);
        cx.simulate_event(gpui::MouseDownEvent {
            position: center_of(shared_rect, bounds),
            button: gpui::MouseButton::Left,
            modifiers: Default::default(),
            click_count: 2,
            first_mouse: false,
        });
        assert_eq!(
            requests.borrow().as_slice(),
            [(
                None,
                String::new(),
                "\u{5171}\u{6709}".to_string(),
                "\u{3053}\u{306e}\u{7dda}\u{306e}\u{30e9}\u{30d9}\u{30eb}\u{306f}\u{8907}\u{6570}\u{306e}\u{3064}\u{306a}\u{304c}\u{308a}\u{3067}\u{5171}\u{6709}\u{3055}\u{308c}\u{3066}\u{3044}\u{308b}\u{305f}\u{3081}\u{3001}\u{3053}\u{3053}\u{3067}\u{306f}\u{7de8}\u{96c6}\u{3067}\u{304d}\u{307e}\u{305b}\u{3093}\u{3002}"
            )]
        );

        // Nothing is offered where no label is drawn.
        requests.borrow_mut().clear();
        let bounds = draw(cx);
        cx.simulate_event(gpui::MouseDownEvent {
            position: point(bounds.origin.x + px(1.0), bounds.bottom() - px(1.0)),
            button: gpui::MouseButton::Left,
            modifiers: Default::default(),
            click_count: 2,
            first_mouse: false,
        });
        assert!(requests.borrow().is_empty(), "the corner holds no label");
    }

    fn ensure_theme_initialized(cx: &mut TestAppContext) {
        cx.update(|cx| {
            if !cx.has_global::<settings::SettingsStore>() {
                settings::init(cx);
            }
            if !cx.has_global::<theme::GlobalTheme>() {
                theme_settings::init(theme::LoadThemes::JustBase, cx);
            }
        });
    }

    /// Renders a [`MarkdownElement`] beneath a throwaway view (mirroring how
    /// elements are always rendered in production) and captures the
    /// [`crate::RenderedText`] produced by its layout. Mermaid diagram bodies
    /// are scroll containers, whose paint requires a current view, so the
    /// element can't be drawn bare with `cx.draw`.
    fn draw_markdown_element(
        markdown: Entity<Markdown>,
        cx: &mut gpui::VisualTestContext,
    ) -> crate::RenderedText {
        struct CaptureRenderedText {
            markdown: Entity<Markdown>,
            rendered_text: Rc<RefCell<Option<crate::RenderedText>>>,
        }

        impl Render for CaptureRenderedText {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                let element = MarkdownElement::new(self.markdown.clone(), MarkdownStyle::default())
                    .code_block_renderer(CodeBlockRenderer::Default {
                        copy_button_visibility: CopyButtonVisibility::Hidden,
                        wrap_button_visibility: WrapButtonVisibility::Hidden,
                        border: false,
                    })
                    .on_render({
                        let rendered_text = self.rendered_text.clone();
                        move |text| *rendered_text.borrow_mut() = Some(text)
                    });
                div().child(element)
            }
        }

        let rendered_text = Rc::new(RefCell::new(None));
        cx.draw(Default::default(), size(px(600.0), px(600.0)), {
            let rendered_text = rendered_text.clone();
            |_window, cx| {
                cx.new(|_| CaptureRenderedText {
                    markdown,
                    rendered_text,
                })
                .into_any_element()
            }
        });
        let rendered_text = rendered_text.borrow_mut().take();
        rendered_text.expect("markdown element should have been laid out")
    }

    fn render_markdown_with_options(
        markdown: &str,
        options: MarkdownOptions,
        cx: &mut TestAppContext,
    ) -> crate::RenderedText {
        struct TestWindow;

        impl Render for TestWindow {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div()
            }
        }

        ensure_theme_initialized(cx);

        let (_, cx) = cx.add_window_view(|_, _| TestWindow);
        let markdown = cx.new(|cx| {
            Markdown::new_with_options(markdown.to_string().into(), None, None, options, cx)
        });
        cx.run_until_parked();
        draw_markdown_element(markdown, cx)
    }

    fn mock_render_image(cx: &mut TestAppContext) -> Arc<RenderImage> {
        cx.update(|cx| {
            cx.svg_renderer()
                .render_single_frame(
                    br#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"></svg>"#,
                    1.0,
                )
                .unwrap()
        })
    }

    fn mermaid_contents(contents: &str) -> ParsedMarkdownMermaidDiagramContents {
        ParsedMarkdownMermaidDiagramContents {
            contents: contents.to_string().into(),
            scale: 100,
        }
    }

    fn mermaid_sequence(diagrams: &[&str]) -> Vec<ParsedMarkdownMermaidDiagramContents> {
        diagrams
            .iter()
            .map(|diagram| mermaid_contents(diagram))
            .collect()
    }

    fn mermaid_fallback(
        new_diagram: &str,
        new_full_order: &[ParsedMarkdownMermaidDiagramContents],
        old_full_order: &[ParsedMarkdownMermaidDiagramContents],
        cache: &MermaidDiagramCache,
    ) -> Option<Arc<RenderImage>> {
        let new_content = mermaid_contents(new_diagram);
        let idx = new_full_order
            .iter()
            .position(|diagram| diagram == &new_content)?;
        MermaidState::get_fallback_image(idx, old_full_order, new_full_order.len(), cache)
            .map(|(image, _)| image)
    }

    #[test]
    fn test_mermaid_font_family_resolves_zed_virtual_fonts() {
        assert_eq!(
            super::mermaid_font_family(".ZedSans"),
            "IBM Plex Sans, sans-serif"
        );
        assert_eq!(
            super::mermaid_font_family("Zed Plex Sans"),
            "IBM Plex Sans, sans-serif"
        );
        assert_eq!(super::mermaid_font_family(".ZedMono"), "Lilex, sans-serif");
        assert_eq!(
            super::mermaid_font_family(".SystemUIFont"),
            "system-ui, sans-serif"
        );
        assert_eq!(
            super::mermaid_font_family("Custom Font"),
            "Custom Font, sans-serif"
        );
        assert_eq!(
            super::mermaid_font_family("Custom Font, sans-serif"),
            "Custom Font, sans-serif"
        );
    }

    #[test]
    fn test_parse_mermaid_info() {
        assert_eq!(parse_mermaid_info("mermaid"), Some(100));
        assert_eq!(parse_mermaid_info("mermaid 150"), Some(150));
        assert_eq!(parse_mermaid_info("mermaid 5"), Some(10));
        assert_eq!(parse_mermaid_info("mermaid 999"), Some(500));
        assert_eq!(parse_mermaid_info("rust"), None);
    }

    #[test]
    fn test_extract_mermaid_diagrams_parses_scale() {
        let markdown = "```mermaid 150\ngraph TD;\n```\n\n```rust\nfn main() {}\n```";
        let events =
            crate::parser::parse_markdown_with_options(markdown, false, false, false).events;
        let diagrams = extract_mermaid_diagrams(markdown, &events);

        assert_eq!(diagrams.len(), 1);
        let diagram = diagrams.values().next().unwrap();
        assert_eq!(diagram.contents.contents, "graph TD;");
        assert_eq!(diagram.contents.scale, 150);
    }

    #[test]
    fn test_extract_mermaid_diagrams_with_tilde_fence() {
        let markdown = "~~~mermaid\ngraph TD;\n~~~";
        let events =
            crate::parser::parse_markdown_with_options(markdown, false, false, false).events;
        let diagrams = extract_mermaid_diagrams(markdown, &events);

        assert_eq!(diagrams.len(), 1);
        assert_eq!(
            diagrams
                .values()
                .next()
                .map(|diagram| diagram.contents.contents.as_ref()),
            Some("graph TD;")
        );
    }

    #[test]
    fn test_unsupported_diagram_types_are_skipped() {
        let markdown = concat!(
            "```mermaid\nzenuml\n```\n\n",
            "```mermaid\ninfo\n```\n\n",
            "```mermaid\nflowchart TD\n    A --> B\n```",
        );
        let events =
            crate::parser::parse_markdown_with_options(markdown, false, false, false).events;
        let diagrams = extract_mermaid_diagrams(markdown, &events);
        assert_eq!(
            diagrams.len(),
            1,
            "Only the flowchart should be extracted; zenuml and info should be skipped"
        );
        let diagram = diagrams.values().next().unwrap();
        assert!(
            diagram.contents.contents.contains("flowchart"),
            "The extracted diagram should be the flowchart"
        );
    }

    #[gpui::test]
    fn test_mermaid_fallback_on_edit(cx: &mut TestAppContext) {
        let old_full_order = mermaid_sequence(&["graph A", "graph B", "graph C"]);
        let new_full_order = mermaid_sequence(&["graph A", "graph B modified", "graph C"]);

        let svg_b = mock_render_image(cx);

        let mut cache: MermaidDiagramCache = HashMap::default();
        cache.insert(
            mermaid_contents("graph A"),
            Arc::new(CachedMermaidDiagram::new_for_test(
                Some(mock_render_image(cx)),
                None,
                None,
                None,
            )),
        );
        cache.insert(
            mermaid_contents("graph B"),
            Arc::new(CachedMermaidDiagram::new_for_test(
                Some(svg_b.clone()),
                None,
                None,
                None,
            )),
        );
        cache.insert(
            mermaid_contents("graph C"),
            Arc::new(CachedMermaidDiagram::new_for_test(
                Some(mock_render_image(cx)),
                None,
                None,
                None,
            )),
        );

        let fallback =
            mermaid_fallback("graph B modified", &new_full_order, &old_full_order, &cache);

        assert_eq!(fallback.as_ref().map(|image| image.id), Some(svg_b.id));
    }

    #[gpui::test]
    fn test_mermaid_no_fallback_on_add_in_middle(cx: &mut TestAppContext) {
        let old_full_order = mermaid_sequence(&["graph A", "graph C"]);
        let new_full_order = mermaid_sequence(&["graph A", "graph NEW", "graph C"]);

        let mut cache: MermaidDiagramCache = HashMap::default();
        cache.insert(
            mermaid_contents("graph A"),
            Arc::new(CachedMermaidDiagram::new_for_test(
                Some(mock_render_image(cx)),
                None,
                None,
                None,
            )),
        );
        cache.insert(
            mermaid_contents("graph C"),
            Arc::new(CachedMermaidDiagram::new_for_test(
                Some(mock_render_image(cx)),
                None,
                None,
                None,
            )),
        );

        let fallback = mermaid_fallback("graph NEW", &new_full_order, &old_full_order, &cache);

        assert!(fallback.is_none());
    }

    #[gpui::test]
    fn test_mermaid_fallback_chains_on_rapid_edits(cx: &mut TestAppContext) {
        let old_full_order = mermaid_sequence(&["graph A", "graph B modified", "graph C"]);
        let new_full_order = mermaid_sequence(&["graph A", "graph B modified again", "graph C"]);

        let original_svg = mock_render_image(cx);

        let mut cache: MermaidDiagramCache = HashMap::default();
        cache.insert(
            mermaid_contents("graph A"),
            Arc::new(CachedMermaidDiagram::new_for_test(
                Some(mock_render_image(cx)),
                None,
                None,
                None,
            )),
        );
        cache.insert(
            mermaid_contents("graph B modified"),
            Arc::new(CachedMermaidDiagram::new_for_test(
                None,
                Some(original_svg.clone()),
                None,
                None,
            )),
        );
        cache.insert(
            mermaid_contents("graph C"),
            Arc::new(CachedMermaidDiagram::new_for_test(
                Some(mock_render_image(cx)),
                None,
                None,
                None,
            )),
        );

        let fallback = mermaid_fallback(
            "graph B modified again",
            &new_full_order,
            &old_full_order,
            &cache,
        );

        assert_eq!(
            fallback.as_ref().map(|image| image.id),
            Some(original_svg.id)
        );
    }

    #[gpui::test]
    fn test_mermaid_fallback_with_duplicate_blocks_edit_second(cx: &mut TestAppContext) {
        let old_full_order = mermaid_sequence(&["graph A", "graph A", "graph B"]);
        let new_full_order = mermaid_sequence(&["graph A", "graph A edited", "graph B"]);

        let svg_a = mock_render_image(cx);

        let mut cache: MermaidDiagramCache = HashMap::default();
        cache.insert(
            mermaid_contents("graph A"),
            Arc::new(CachedMermaidDiagram::new_for_test(
                Some(svg_a.clone()),
                None,
                None,
                None,
            )),
        );
        cache.insert(
            mermaid_contents("graph B"),
            Arc::new(CachedMermaidDiagram::new_for_test(
                Some(mock_render_image(cx)),
                None,
                None,
                None,
            )),
        );

        let fallback = mermaid_fallback("graph A edited", &new_full_order, &old_full_order, &cache);

        assert_eq!(fallback.as_ref().map(|image| image.id), Some(svg_a.id));
    }

    #[gpui::test]
    fn test_mermaid_rerasterize_reuses_parsed_svg(cx: &mut TestAppContext) {
        ensure_theme_initialized(cx);

        let markdown = cx.new(|cx| Markdown::new("".into(), None, None, cx));

        let (parsed_svg, original_image) = markdown.update(cx, |_, cx| {
            let svg_renderer = cx.svg_renderer();
            let parsed_svg = Arc::new(
                svg_renderer
                    .parse_svg(
                        br#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"></svg>"#,
                    )
                    .unwrap(),
            );
            let original_image = svg_renderer.render_parsed(&parsed_svg, 1.0).unwrap();
            (parsed_svg, original_image)
        });

        let contents = mermaid_contents("graph A");
        let mut state = MermaidState::default();
        state.cache.insert(
            contents.clone(),
            Arc::new(CachedMermaidDiagram::new_for_test(
                Some(original_image.clone()),
                None,
                Some(parsed_svg),
                None,
            )),
        );

        markdown.update(cx, |_, cx| state.rerasterize_diagram(&contents, 2.0, cx));

        let cached = state.cache.get(&contents).unwrap();
        assert!(
            cached.render_image.get().is_none(),
            "the new raster should still be pending"
        );
        assert_eq!(cached.rasterized_scale, 2.0);
        assert_eq!(
            cached
                .fallback_image
                .as_ref()
                .map(|(image, zoom)| (image.id, *zoom)),
            Some((original_image.id, 1.0)),
            "the old raster should keep being displayed while the new one is pending"
        );

        cx.run_until_parked();

        let cached = state.cache.get(&contents).unwrap();
        let new_image = cached
            .render_image
            .get()
            .expect("render should have completed")
            .as_ref()
            .expect("render should have succeeded");
        let original_size = original_image.size(0);
        let new_size = new_image.size(0);
        assert_eq!(new_size.width.0, original_size.width.0 * 2);
        assert_eq!(new_size.height.0, original_size.height.0 * 2);

        // Re-rasterizing at the zoom the entry is already rasterized at is a
        // no-op.
        let entry_before = Arc::as_ptr(state.cache.get(&contents).unwrap());
        markdown.update(cx, |_, cx| state.rerasterize_diagram(&contents, 2.0, cx));
        assert_eq!(
            Arc::as_ptr(state.cache.get(&contents).unwrap()),
            entry_before
        );
    }

    #[gpui::test]
    fn test_mermaid_render_panic_becomes_a_render_error(cx: &mut TestAppContext) {
        ensure_theme_initialized(cx);

        let markdown = cx.new(|cx| Markdown::new("".into(), None, None, cx));
        // A C4 boundary whose arguments are still being typed makes merman
        // panic; the preview has to show an error instead of crashing.
        let cached = markdown.update(cx, |_, cx| {
            CachedMermaidDiagram::new(
                mermaid_contents(
                    "C4Context
System_Boundary(b) {
}",
                ),
                1.0,
                None,
                cx,
            )
        });

        cx.run_until_parked();

        let error = cached
            .render_image
            .get()
            .expect("render should have completed")
            .as_ref()
            .expect_err("render should have failed");
        assert!(
            error
                .to_string()
                .contains("merman panicked while rendering"),
            "got: {error:#}"
        );
    }

    #[gpui::test]
    fn test_mermaid_zoom_rerasterize_is_debounced(cx: &mut TestAppContext) {
        ensure_theme_initialized(cx);

        let markdown = cx.new(|cx| Markdown::new("".into(), None, None, cx));

        let (parsed_svg, original_image) = markdown.update(cx, |_, cx| {
            let svg_renderer = cx.svg_renderer();
            let parsed_svg = Arc::new(
                svg_renderer
                    .parse_svg(
                        br#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"></svg>"#,
                    )
                    .unwrap(),
            );
            let original_image = svg_renderer.render_parsed(&parsed_svg, 1.0).unwrap();
            (parsed_svg, original_image)
        });

        let contents = mermaid_contents("graph A");
        let source_offset = 0;
        markdown.update(cx, |markdown, _| {
            markdown.parsed_markdown.mermaid_diagrams.insert(
                source_offset,
                ParsedMarkdownMermaidDiagram {
                    content_range: 0..contents.contents.len(),
                    contents: contents.clone(),
                },
            );
            markdown.mermaid_state.cache.insert(
                contents.clone(),
                Arc::new(CachedMermaidDiagram::new_for_test(
                    Some(original_image.clone()),
                    None,
                    Some(parsed_svg),
                    None,
                )),
            );
        });

        let half_debounce = MERMAID_ZOOM_DEBOUNCE / 2;

        markdown.update(cx, |markdown, cx| {
            markdown.set_mermaid_zoom_level(source_offset, 1.5, cx)
        });
        cx.executor().advance_clock(half_debounce);
        cx.run_until_parked();
        markdown.read_with(cx, |markdown, _| {
            let cached = markdown.mermaid_state.cache.get(&contents).unwrap();
            assert_eq!(
                cached.rasterized_scale, 1.0,
                "no re-raster before the debounce elapses"
            );
        });

        // A second zoom change within the debounce window restarts the timer.
        markdown.update(cx, |markdown, cx| {
            markdown.set_mermaid_zoom_level(source_offset, 2.0, cx)
        });
        cx.executor().advance_clock(half_debounce);
        cx.run_until_parked();
        markdown.read_with(cx, |markdown, _| {
            let cached = markdown.mermaid_state.cache.get(&contents).unwrap();
            assert_eq!(
                cached.rasterized_scale, 1.0,
                "the second zoom change should restart the debounce timer"
            );
        });

        cx.executor()
            .advance_clock(half_debounce + Duration::from_millis(1));
        cx.run_until_parked();
        markdown.read_with(cx, |markdown, _| {
            let cached = markdown.mermaid_state.cache.get(&contents).unwrap();
            assert_eq!(cached.rasterized_scale, 2.0);
            let new_image = cached
                .render_image
                .get()
                .expect("render should have completed")
                .as_ref()
                .expect("render should have succeeded");
            let original_size = original_image.size(0);
            let new_size = new_image.size(0);
            assert_eq!(new_size.width.0, original_size.width.0 * 2);
            assert_eq!(new_size.height.0, original_size.height.0 * 2);
        });
    }

    #[test]
    fn test_mermaid_zoom_ticks_is_one_step_per_notch() {
        use gpui::{ScrollDelta, point, px};

        use super::mermaid_zoom_ticks;

        // Discrete wheel notches report varying line counts per platform and
        // device; all must count as exactly one tick.
        for lines in [1.0, 3.0, 5.0] {
            assert_eq!(
                mermaid_zoom_ticks(ScrollDelta::Lines(point(0.0, lines))),
                1.0
            );
            assert_eq!(
                mermaid_zoom_ticks(ScrollDelta::Lines(point(0.0, -lines))),
                -1.0
            );
        }

        // Precise deltas contribute fractional ticks, capped at one tick.
        let half_tick = mermaid_zoom_ticks(ScrollDelta::Pixels(point(px(0.0), px(10.0))));
        assert!(half_tick > 0.0 && half_tick < 1.0);
        assert_eq!(
            mermaid_zoom_ticks(ScrollDelta::Pixels(point(px(0.0), px(500.0)))),
            1.0
        );

        // Horizontal-only scrolling must not zoom.
        assert_eq!(mermaid_zoom_ticks(ScrollDelta::Lines(point(2.0, 0.0))), 0.0);
    }

    #[gpui::test]
    fn test_mermaid_zoom_snap_and_clamp(cx: &mut TestAppContext) {
        let markdown = cx.new(|cx| Markdown::new("".into(), None, None, cx));

        markdown.update(cx, |markdown, cx| {
            // With no raster or layout to compute a fit-to-width scale from,
            // the minimum zoom is 1.0.
            markdown.set_mermaid_zoom_level(0, 0.05, cx);
            assert_eq!(markdown.mermaid_zoom_level(0), 1.0);

            markdown.set_mermaid_zoom_level(0, 10.0, cx);
            assert_eq!(markdown.mermaid_zoom_level(0), 2.0);

            // Values close to 1.0 snap back to the default.
            markdown.set_mermaid_zoom_level(0, 1.04, cx);
            assert_eq!(markdown.mermaid_zoom_level(0), 1.0);

            markdown.set_mermaid_zoom_level(0, 2.0, cx);
            markdown.set_mermaid_zoom_level(0, 0.96, cx);
            assert_eq!(markdown.mermaid_zoom_level(0), 1.0);

            markdown.set_mermaid_zoom_level(0, 1.06, cx);
            assert_eq!(markdown.mermaid_zoom_level(0), 1.06);
        });
    }

    /// A diagram wider than the preview keeps its natural size, so it overflows
    /// the scroll container ([`mermaid_scroll_container`]) rather than being
    /// shrunk to fit. Fitting the whole diagram into the width stays an
    /// explicit zoom gesture, because a wide diagram scaled down is unreadable.
    #[gpui::test]
    fn test_wide_diagram_starts_at_its_natural_size(cx: &mut TestAppContext) {
        ensure_theme_initialized(cx);

        let markdown = cx.new(|cx| Markdown::new("".into(), None, None, cx));
        let (parsed_svg, image) = markdown.update(cx, |_, cx| {
            let svg_renderer = cx.svg_renderer();
            let parsed_svg = Arc::new(
                svg_renderer
                    .parse_svg(
                        br#"<svg xmlns="http://www.w3.org/2000/svg" width="800" height="100"></svg>"#,
                    )
                    .unwrap(),
            );
            let image = svg_renderer.render_parsed(&parsed_svg, 1.0).unwrap();
            (parsed_svg, image)
        });

        let contents = mermaid_contents("graph A");
        let source_offset = 0;
        markdown.update(cx, |markdown, cx| {
            markdown.parsed_markdown.mermaid_diagrams.insert(
                source_offset,
                ParsedMarkdownMermaidDiagram {
                    content_range: 0..contents.contents.len(),
                    contents: contents.clone(),
                },
            );
            markdown.mermaid_state.cache.insert(
                contents.clone(),
                Arc::new(CachedMermaidDiagram::new_for_test(
                    Some(image),
                    None,
                    Some(parsed_svg),
                    None,
                )),
            );
            markdown
                .mermaid_views
                .entry(source_offset)
                .or_default()
                .container_width_for_test = Some(px(200.));

            // Without this the assertions below would hold for a diagram that
            // already fits, and would say nothing about the wide case.
            assert_eq!(
                markdown.mermaid_min_zoom_level(source_offset),
                0.25,
                "the diagram must be four times the container width"
            );
            assert_eq!(
                markdown.effective_mermaid_zoom_level(source_offset, cx),
                1.0,
                "a diagram four times the container width must not be shrunk to fit"
            );
            assert_eq!(markdown.mermaid_zoom_level(source_offset), 1.0);
        });
    }

    #[gpui::test]
    fn test_mermaid_zoom_to_fit_tracks_container_width(cx: &mut TestAppContext) {
        ensure_theme_initialized(cx);

        let markdown = cx.new(|cx| Markdown::new("".into(), None, None, cx));

        // A raster whose natural (zoom 1.0) width is 200px.
        let (parsed_svg, image) = markdown.update(cx, |_, cx| {
            let svg_renderer = cx.svg_renderer();
            let parsed_svg = Arc::new(
                svg_renderer
                    .parse_svg(
                        br#"<svg xmlns="http://www.w3.org/2000/svg" width="200" height="100"></svg>"#,
                    )
                    .unwrap(),
            );
            let image = svg_renderer.render_parsed(&parsed_svg, 1.0).unwrap();
            (parsed_svg, image)
        });

        let contents = mermaid_contents("graph A");
        let source_offset = 0;
        markdown.update(cx, |markdown, _| {
            markdown.parsed_markdown.mermaid_diagrams.insert(
                source_offset,
                ParsedMarkdownMermaidDiagram {
                    content_range: 0..contents.contents.len(),
                    contents: contents.clone(),
                },
            );
            markdown.mermaid_state.cache.insert(
                contents.clone(),
                Arc::new(CachedMermaidDiagram::new_for_test(
                    Some(image),
                    None,
                    Some(parsed_svg),
                    None,
                )),
            );
            markdown
                .mermaid_views
                .entry(source_offset)
                .or_default()
                .container_width_for_test = Some(px(100.));
        });

        // Zooming out past the floor clamps to fit-to-width (100 / 200).
        markdown.update(cx, |markdown, cx| {
            markdown.set_mermaid_zoom_level(source_offset, 0.05, cx);
            assert_eq!(markdown.mermaid_zoom_level(source_offset), 0.5);
        });

        // A fully zoomed-out diagram stays stuck to fit-to-width when the
        // container is resized.
        markdown.update(cx, |markdown, cx| {
            markdown
                .mermaid_views
                .get_mut(&source_offset)
                .unwrap()
                .container_width_for_test = Some(px(150.));
            assert_eq!(
                markdown.effective_mermaid_zoom_level(source_offset, cx),
                0.75
            );
            assert_eq!(markdown.mermaid_zoom_level(source_offset), 0.75);
        });

        // Zooming in one step starts from the tracked fit-to-width zoom
        // instead of jumping, and stops tracking the container width.
        markdown.update(cx, |markdown, cx| {
            let zoom = markdown.mermaid_zoom_level(source_offset);
            markdown.set_mermaid_zoom_level(source_offset, zoom + 0.1, cx);
            assert!((markdown.mermaid_zoom_level(source_offset) - 0.85).abs() < 1e-5);

            markdown
                .mermaid_views
                .get_mut(&source_offset)
                .unwrap()
                .container_width_for_test = Some(px(100.));
            assert!(
                (markdown.effective_mermaid_zoom_level(source_offset, cx) - 0.85).abs() < 1e-5,
                "a zoom above the floor must not track container resizes"
            );
        });

        // Resetting to the natural size never tracks the container width.
        markdown.update(cx, |markdown, cx| {
            markdown.set_mermaid_zoom_level(source_offset, 1.0, cx);
            markdown
                .mermaid_views
                .get_mut(&source_offset)
                .unwrap()
                .container_width_for_test = Some(px(150.));
            assert_eq!(
                markdown.effective_mermaid_zoom_level(source_offset, cx),
                1.0
            );
        });
    }

    #[gpui::test]
    fn test_mermaid_zoom_retained_across_reparse(cx: &mut TestAppContext) {
        ensure_theme_initialized(cx);

        let source = "```mermaid\ngraph TD;\n```";
        let markdown = cx.new(|cx| {
            Markdown::new_with_options(
                source.into(),
                None,
                None,
                MarkdownOptions {
                    render_mermaid_diagrams: true,
                    ..Default::default()
                },
                cx,
            )
        });
        cx.run_until_parked();

        let source_offset = markdown.read_with(cx, |markdown, _| {
            *markdown
                .parsed_markdown
                .mermaid_diagrams
                .keys()
                .next()
                .expect("the mermaid diagram should have been parsed")
        });
        markdown.update(cx, |markdown, cx| {
            markdown.set_mermaid_zoom_level(source_offset, 2.0, cx)
        });

        // Appending after the diagram keeps its offset, so the zoom is
        // retained across the reparse.
        markdown.update(cx, |markdown, cx| {
            markdown.replace(format!("{source}\n\nmore text"), cx);
        });
        cx.run_until_parked();
        markdown.read_with(cx, |markdown, _| {
            assert_eq!(markdown.mermaid_zoom_level(source_offset), 2.0);
        });

        // Removing the diagram drops its zoom state.
        markdown.update(cx, |markdown, cx| {
            markdown.replace("plain text", cx);
        });
        cx.run_until_parked();
        markdown.read_with(cx, |markdown, _| {
            assert_eq!(markdown.mermaid_zoom_level(source_offset), 1.0);
            assert!(markdown.mermaid_views.is_empty());
        });
    }

    #[gpui::test]
    fn test_mermaid_scroll_handle_retained_across_reparse(cx: &mut TestAppContext) {
        ensure_theme_initialized(cx);

        let source = "```mermaid\ngraph TD;\n```";
        let markdown = cx.new(|cx| {
            Markdown::new_with_options(
                source.into(),
                None,
                None,
                MarkdownOptions {
                    render_mermaid_diagrams: true,
                    ..Default::default()
                },
                cx,
            )
        });
        cx.run_until_parked();

        let source_offset = markdown.read_with(cx, |markdown, _| {
            *markdown
                .parsed_markdown
                .mermaid_diagrams
                .keys()
                .next()
                .expect("the mermaid diagram should have been parsed")
        });
        let scroll_handle = markdown.update(cx, |markdown, _| {
            markdown.mermaid_scroll_handle(source_offset)
        });
        scroll_handle.set_offset(point(px(-42.0), px(0.0)));

        // Appending after the diagram keeps its offset, so the same scroll
        // handle (and thus the scroll position) is retained across the
        // reparse.
        markdown.update(cx, |markdown, cx| {
            markdown.replace(format!("{source}\n\nmore text"), cx);
        });
        cx.run_until_parked();
        let retained_handle = markdown.update(cx, |markdown, _| {
            markdown.mermaid_scroll_handle(source_offset)
        });
        assert_eq!(retained_handle.offset(), point(px(-42.0), px(0.0)));

        // Removing the diagram drops its scroll state.
        markdown.update(cx, |markdown, cx| {
            markdown.replace("plain text", cx);
        });
        cx.run_until_parked();
        markdown.read_with(cx, |markdown, _| {
            assert!(markdown.mermaid_views.is_empty());
        });
    }

    #[gpui::test]
    fn test_mermaid_rendering_replaces_code_block_text(cx: &mut TestAppContext) {
        let rendered = render_markdown_with_options(
            "```mermaid\ngraph TD;\n```",
            MarkdownOptions {
                render_mermaid_diagrams: true,
                ..Default::default()
            },
            cx,
        );

        let text = rendered
            .lines
            .iter()
            .map(|line| line.layout.wrapped_text())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(!text.contains("graph TD;"));
    }

    #[gpui::test]
    fn test_mermaid_source_anchor_maps_inside_block(cx: &mut TestAppContext) {
        struct TestWindow;

        impl Render for TestWindow {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div()
            }
        }

        ensure_theme_initialized(cx);

        let (_, cx) = cx.add_window_view(|_, _| TestWindow);
        let markdown = cx.new(|cx| {
            Markdown::new_with_options(
                "```mermaid\ngraph TD;\n```".into(),
                None,
                None,
                MarkdownOptions {
                    render_mermaid_diagrams: true,
                    ..Default::default()
                },
                cx,
            )
        });
        cx.run_until_parked();
        let render_image = mock_render_image(cx);
        markdown.update(cx, |markdown, _| {
            let contents = markdown
                .parsed_markdown
                .mermaid_diagrams
                .values()
                .next()
                .unwrap()
                .contents
                .clone();
            markdown.mermaid_state.cache.insert(
                contents.clone(),
                Arc::new(CachedMermaidDiagram::new_for_test(
                    Some(render_image),
                    None,
                    None,
                    None,
                )),
            );
            markdown.mermaid_state.order = vec![contents];
        });

        let rendered_text = draw_markdown_element(markdown.clone(), cx);

        let mermaid_diagram = markdown.update(cx, |markdown, _| {
            markdown
                .parsed_markdown
                .mermaid_diagrams
                .values()
                .next()
                .unwrap()
                .clone()
        });
        assert!(
            rendered_text
                .position_for_source_index(mermaid_diagram.content_range.start)
                .is_some()
        );
        assert!(
            rendered_text
                .position_for_source_index(mermaid_diagram.content_range.end.saturating_sub(1))
                .is_some()
        );
    }
}

#[cfg(test)]
mod class_sequence_tests;
