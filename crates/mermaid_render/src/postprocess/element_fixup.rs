//! Fixes various issues in merman's SVG output.
//!
//! Replaces hardcoded white backgrounds with the theme background:
//! ```xml
//! <!-- before --> <svg style="background-color: white">
//! <!-- after  --> <svg style="background-color: #1e1e2e">
//! ```
//!
//! Removes `<rect>` elements with missing or invalid dimensions:
//! ```xml
//! <!-- before --> <rect width="NaN" height="10"/>
//! <!-- after  --> (removed)
//! ```
//!
//! Replaces hardcoded text colors with the theme text color:
//! ```xml
//! <!-- before --> <text fill="#333">Hello</text>
//! <!-- after  --> <text fill="#cdd6f4">Hello</text>
//! ```

use std::borrow::Cow;
use std::fmt::Write as _;

use anyhow::Result;
use quick_xml::events::{BytesStart, Event};

use super::unescape_attribute_value;
use crate::MermaidTheme;

struct ElementFixup<I> {
    inner: I,
    background_css: String,
    text_color_css: String,
    line_color_css: String,
    font_family_css: String,
    diagram_kind: DiagramKind,
    svg_seen: bool,
    skip_rect_depth: usize,
}

/// The diagram kinds whose colors are written as attributes rather than
/// through a stylesheet, so they need rewriting element by element.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiagramKind {
    C4,
    Sankey,
    Treemap,
    Other,
}

impl DiagramKind {
    fn of_root(e: &BytesStart<'_>) -> Result<Self> {
        let Some(role) = e.try_get_attribute("aria-roledescription")? else {
            return Ok(Self::Other);
        };
        Ok(match role.value.as_ref() {
            b"c4" => Self::C4,
            b"sankey" => Self::Sankey,
            b"treemap" => Self::Treemap,
            _ => Self::Other,
        })
    }
}

/// C4 draws boundaries, relations and their labels in this fixed gray unless
/// the diagram restyles them, which disappears against a dark background.
const C4_DEFAULT_INK: &str = "#444444";

fn rewrite_attr<'a>(
    e: &BytesStart<'_>,
    attr_name: &[u8],
    new_value: &str,
) -> Result<BytesStart<'a>> {
    let name = e.name();
    let tag = std::str::from_utf8(name.as_ref())?;
    let mut new_elem = BytesStart::new(tag.to_owned());
    for attr in e.attributes() {
        let attr = attr?;
        if attr.key.local_name().as_ref() == attr_name {
            let local_name = attr.key.local_name();
            let key = std::str::from_utf8(local_name.as_ref())?;
            new_elem.push_attribute((key, new_value));
        } else {
            new_elem.push_attribute(attr);
        }
    }
    Ok(new_elem)
}

fn rewrap<'a>(event: &Event<'_>, elem: BytesStart<'a>) -> Event<'a> {
    match event {
        Event::Start(_) => Event::Start(elem),
        _ => Event::Empty(elem),
    }
}

fn is_bad_rect(e: &BytesStart) -> Result<bool> {
    for attr_name in ["width", "height"] {
        match e.try_get_attribute(attr_name)? {
            None => return Ok(true),
            Some(attr) => {
                let val = attr.unescape_value()?;
                let trimmed = val.trim();
                if trimmed.is_empty() {
                    return Ok(true);
                }
                if let Ok(n) = trimmed.parse::<f64>() {
                    if !n.is_finite() || n <= 0.0 {
                        return Ok(true);
                    }
                }
            }
        }
    }
    Ok(false)
}

fn is_hardcoded_text_fill(val: &str) -> bool {
    matches!(
        val,
        "" | "#333" | "black" | "#000" | "#000000" | "white" | "#fff" | "#ffffff"
    )
}

fn style_declarations(style: &str) -> impl Iterator<Item = &str> {
    style
        .split(';')
        .map(str::trim)
        .filter(|declaration| !declaration.is_empty())
}

fn declaration_property(declaration: &str) -> Option<&str> {
    declaration
        .split_once(':')
        .map(|(property, _)| property.trim())
}

/// Replaces the first `fill` declaration only: merman writes its computed
/// default first and appends a `classDef` color after it, which must keep
/// winning.
fn replace_first_fill_declaration(style: &str, fill: &str) -> Option<String> {
    let mut replaced = false;
    let mut rewritten = String::with_capacity(style.len() + fill.len());
    for declaration in style_declarations(style) {
        if !rewritten.is_empty() {
            rewritten.push(' ');
        }
        let is_fill = declaration_property(declaration)
            .is_some_and(|property| property.eq_ignore_ascii_case("fill"));
        if is_fill && !replaced {
            replaced = true;
            write!(rewritten, "fill:{fill};").expect("write to String cannot fail");
        } else {
            rewritten.push_str(declaration);
            rewritten.push(';');
        }
    }
    replaced.then_some(rewritten)
}

fn push_font_style(style: &mut String, font_family: &str) {
    write!(style, "font-family: {font_family};").expect("write to String cannot fail");
}

fn font_style(font_family: &str) -> String {
    let mut style = String::with_capacity(font_family.len() + "font-family: ;".len());
    push_font_style(&mut style, font_family);
    style
}

fn rewrite_background_style<'a>(style: &'a str, background_css: &str) -> Cow<'a, str> {
    const PROPERTY: &str = "background-color:";

    let Some(property_start) = style.find(PROPERTY) else {
        return Cow::Borrowed(style);
    };
    let value_start = property_start + PROPERTY.len();
    let value_end = style[value_start..]
        .find(';')
        .map_or(style.len(), |offset| value_start + offset);
    let value = style[value_start..value_end].trim();

    let is_white = value.eq_ignore_ascii_case("white")
        || value.eq_ignore_ascii_case("#fff")
        || value.eq_ignore_ascii_case("#ffffff");
    if !is_white {
        return Cow::Borrowed(style);
    }

    let value_len = value_end.saturating_sub(value_start);
    let mut rewritten = String::with_capacity(
        style
            .len()
            .saturating_sub(value_len)
            .saturating_add(background_css.len()),
    );
    rewritten.push_str(&style[..value_start]);
    rewritten.push_str(background_css);
    rewritten.push_str(&style[value_end..]);
    Cow::Owned(rewritten)
}

fn font_family_declaration_value(declaration: &str) -> Option<&str> {
    let (property, value) = declaration.split_once(':')?;
    property
        .trim()
        .eq_ignore_ascii_case("font-family")
        .then(|| value.trim())
}

fn rewrite_font_style<'a>(style: &'a str, font_family: &str) -> Cow<'a, str> {
    let mut font_family_declaration_count = 0;
    let mut has_target_font_family = false;
    for declaration in style
        .split(';')
        .map(str::trim)
        .filter(|declaration| !declaration.is_empty())
    {
        if let Some(value) = font_family_declaration_value(declaration) {
            font_family_declaration_count += 1;
            has_target_font_family = value == font_family;
        }
    }

    if font_family_declaration_count == 1 && has_target_font_family {
        return Cow::Borrowed(style);
    }

    let mut rewritten =
        String::with_capacity(style.len() + font_family.len() + " font-family: ;".len());
    for declaration in style.split(';') {
        let declaration = declaration.trim();
        if declaration.is_empty() || font_family_declaration_value(declaration).is_some() {
            continue;
        }
        if !rewritten.is_empty() {
            rewritten.push(' ');
        }
        rewritten.push_str(declaration);
        rewritten.push(';');
    }
    if !rewritten.is_empty() {
        rewritten.push(' ');
    }
    push_font_style(&mut rewritten, font_family);
    Cow::Owned(rewritten)
}

impl<'a, I: Iterator<Item = Result<Event<'a>>>> ElementFixup<I> {
    fn rewrite_svg_style(&self, e: &BytesStart<'_>) -> Result<Option<BytesStart<'a>>> {
        let Some(style) = e
            .try_get_attribute("style")?
            .as_ref()
            .map(unescape_attribute_value)
            .transpose()?
        else {
            return Ok(None);
        };
        let new_style = rewrite_background_style(&style, &self.background_css);
        if matches!(new_style, Cow::Borrowed(_)) {
            return Ok(None);
        }

        Ok(Some(rewrite_attr(e, b"style", &new_style)?))
    }

    fn rewrite_text_element(&self, e: &BytesStart<'_>, fix_fill: bool) -> Result<BytesStart<'a>> {
        let name = e.name();
        let tag = std::str::from_utf8(name.as_ref())?;
        let mut new_elem = BytesStart::new(tag.to_owned());
        let mut has_font_family = false;
        let mut has_style = false;
        // Treemap labels get a fill picked for the full-strength section color,
        // but the box behind them is drawn translucent over the background.
        let themed_style_fill = self.diagram_kind == DiagramKind::Treemap
            && e.try_get_attribute("class")?.is_some_and(|class| {
                class
                    .value
                    .split(|byte| byte.is_ascii_whitespace())
                    .any(|token| token.starts_with(b"treemap"))
            });

        for attr in e.attributes() {
            let attr = attr?;
            match attr.key.local_name().as_ref() {
                b"fill" if fix_fill => {
                    let val = attr.unescape_value()?;
                    if is_hardcoded_text_fill(&val)
                        || (self.diagram_kind == DiagramKind::C4 && val == C4_DEFAULT_INK)
                    {
                        new_elem.push_attribute(("fill", self.text_color_css.as_str()));
                    } else {
                        new_elem.push_attribute(attr);
                    }
                }
                b"font-family" => {
                    has_font_family = true;
                    new_elem.push_attribute(("font-family", self.font_family_css.as_str()));
                }
                b"style" => {
                    has_style = true;
                    let style = unescape_attribute_value(&attr)?;
                    let themed = themed_style_fill
                        .then(|| replace_first_fill_declaration(&style, &self.text_color_css))
                        .flatten();
                    let style = themed.as_deref().unwrap_or(&style);
                    let style = rewrite_font_style(style, &self.font_family_css);
                    new_elem.push_attribute(("style", style.as_ref()));
                }
                _ => new_elem.push_attribute(attr),
            }
        }

        if !has_font_family {
            new_elem.push_attribute(("font-family", self.font_family_css.as_str()));
        }
        if !has_style {
            let style = font_style(&self.font_family_css);
            new_elem.push_attribute(("style", style.as_str()));
        }

        Ok(new_elem)
    }

    fn rewrite_kind_specific_shape(&self, event: Event<'a>) -> Result<Option<Event<'a>>> {
        let (Event::Start(e) | Event::Empty(e)) = &event else {
            return Ok(Some(event));
        };
        match self.diagram_kind {
            DiagramKind::C4 => {
                let default_stroke = e
                    .try_get_attribute("stroke")?
                    .is_some_and(|stroke| stroke.value.as_ref() == C4_DEFAULT_INK.as_bytes());
                if default_stroke {
                    let new_elem = rewrite_attr(e, b"stroke", &self.line_color_css)?;
                    return Ok(Some(rewrap(&event, new_elem)));
                }
            }
            DiagramKind::Sankey if e.name().as_ref() == b"rect" => {
                // The generic `.node rect` theme rule would otherwise paint
                // every sankey node the same surface color; an inline style
                // outranks it while keeping merman's per-node color.
                let fill = e
                    .try_get_attribute("fill")?
                    .map(|fill| fill.unescape_value().map(|value| value.into_owned()))
                    .transpose()?;
                if let Some(fill) = fill
                    && e.try_get_attribute("style")?.is_none()
                {
                    let mut new_elem = e.to_owned();
                    new_elem.push_attribute(("style", format!("fill: {fill};").as_str()));
                    return Ok(Some(rewrap(&event, new_elem)));
                }
            }
            DiagramKind::Sankey | DiagramKind::Treemap | DiagramKind::Other => {}
        }
        Ok(Some(event))
    }

    fn process_event(&mut self, event: Event<'a>) -> Result<Option<Event<'a>>> {
        match &event {
            Event::Start(e) | Event::Empty(e) if e.name().as_ref() == b"svg" && !self.svg_seen => {
                self.svg_seen = true;
                self.diagram_kind = DiagramKind::of_root(e)?;
                if let Some(new_elem) = self.rewrite_svg_style(e)? {
                    Ok(Some(rewrap(&event, new_elem)))
                } else {
                    Ok(Some(event))
                }
            }

            Event::Start(e) | Event::Empty(e) if e.name().as_ref() == b"rect" => {
                if is_bad_rect(e)? {
                    if matches!(event, Event::Start(_)) {
                        self.skip_rect_depth = 1;
                    }
                    Ok(None)
                } else {
                    self.rewrite_kind_specific_shape(event)
                }
            }

            Event::Start(e) | Event::Empty(e) if matches!(e.name().as_ref(), b"line" | b"path") => {
                self.rewrite_kind_specific_shape(event)
            }

            Event::Start(e) | Event::Empty(e) if e.name().as_ref() == b"text" => {
                Ok(Some(rewrap(&event, self.rewrite_text_element(e, true)?)))
            }

            Event::Start(e) | Event::Empty(e) if e.name().as_ref() == b"tspan" => {
                Ok(Some(rewrap(&event, self.rewrite_text_element(e, false)?)))
            }

            _ => Ok(Some(event)),
        }
    }
}

impl<'a, I: Iterator<Item = Result<Event<'a>>>> Iterator for ElementFixup<I> {
    type Item = Result<Event<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let event = match self.inner.next()? {
                Ok(ev) => ev,
                Err(e) => return Some(Err(e)),
            };

            if self.skip_rect_depth > 0 {
                match &event {
                    Event::Start(_) => self.skip_rect_depth += 1,
                    Event::End(_) => self.skip_rect_depth -= 1,
                    _ => {}
                }
                continue;
            }

            match self.process_event(event) {
                Ok(Some(ev)) => return Some(Ok(ev)),
                Ok(None) => continue,
                Err(e) => return Some(Err(e)),
            }
        }
    }
}

pub(super) fn process<'a>(
    events: impl Iterator<Item = Result<Event<'a>>>,
    theme: &MermaidTheme,
) -> impl Iterator<Item = Result<Event<'a>>> {
    ElementFixup {
        inner: events,
        background_css: crate::css_color(theme.background),
        text_color_css: crate::css_color(theme.text_color),
        line_color_css: crate::css_color(theme.line_color),
        font_family_css: theme.font_family.clone(),
        diagram_kind: DiagramKind::Other,
        svg_seen: false,
        skip_rect_depth: 0,
    }
}

#[cfg(test)]
mod tests {
    use quick_xml::Reader;
    use quick_xml::events::Event;

    use crate::{MermaidTheme, render_to_svg};

    /// State and ER diagrams are the ones that carry the font family in the
    /// `style` attribute of every `<text>`, which is where `merman` loses the
    /// `;` of a trailing `&quot;`. The production theme's font family ends with
    /// a quoted name, so before this was repaired these two diagram kinds did
    /// not render at all with the settings Zed ships.
    #[test]
    fn a_font_family_that_ends_with_a_quoted_name_still_renders() {
        for source in [
            "stateDiagram-v2\n    [*] --> Idle\n    Idle --> [*]",
            "erDiagram\n    CUSTOMER ||--o{ ORDER : places",
            "flowchart TD\n    A[開始] --> B[終了]",
        ] {
            for font_family in [
                MermaidTheme::default().font_family,
                "\"Segoe UI\"".to_string(),
                "Zed Plex Sans, sans-serif".to_string(),
                "Inter, \"Noto Sans JP\"".to_string(),
            ] {
                let theme = MermaidTheme {
                    font_family: font_family.clone(),
                    ..MermaidTheme::default()
                };
                let svg = render_to_svg(source, &theme).unwrap_or_else(|error| {
                    panic!("`{source}` should render with {font_family:?}: {error}")
                });

                let mut reader = Reader::from_str(&svg);
                reader.config_mut().check_end_names = false;
                loop {
                    match reader.read_event() {
                        Ok(Event::Eof) => break,
                        Ok(_) => {}
                        Err(error) => {
                            panic!("the SVG should parse back with {font_family:?}: {error}\n{svg}")
                        }
                    }
                }
                assert!(
                    svg.contains(&font_family.replace('"', "&quot;")),
                    "the font family should reach the SVG intact with {font_family:?}"
                );
            }
        }
    }
}
