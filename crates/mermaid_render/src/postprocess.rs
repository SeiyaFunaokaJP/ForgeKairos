//! Zed-specific post-processing of [`merman`]-produced SVGs.
//!
//! Each submodule is a specific pass that tweaks the SVG event iterator in a particular way.
//!
//! We always produce and consume [`Event`]s with a short lifetime.
//! [`Event<'a>`] is backed internally by a [`Cow<'a, [u8]>`](std::borrow::Cow),
//! so we don't have lifetime issues when we need to mutate the text in an
//! [`Event`], but also don't force allocating a new [`String`] each time.
//!
//! Many modules contain internal structs that implement [`Iterator`] to make
//! reasoning about lifetimes simpler, but these are private implementation
//! details.

mod accent_colors;
mod center_edge_labels;
mod collect_geometry;
#[cfg(test)]
mod collect_geometry_class_tests;
mod element_fixup;
mod inject_css;
mod strip_foreignobject;
pub(crate) mod util;

use std::borrow::Cow;
use std::cell::RefCell;
use std::rc::Rc;

use anyhow::{Context as _, Result};
use quick_xml::Reader;
use quick_xml::events::Event;
use quick_xml::events::attributes::Attribute;

use crate::{DiagramGeometry, MermaidTheme};

/// [`Attribute::unescape_value`] for a value `merman` may have cut short.
///
/// `merman` strips `!important` out of a `style` attribute after it has escaped
/// the attribute, by splitting the escaped text on `;` and joining the pieces
/// back without the empty last one. A value whose last character is an escaped
/// quote — which is what a `font-family` whose last entry is a quoted font name
/// produces — therefore arrives as `…&quot;Segoe UI Emoji&quot`, and
/// `quick_xml` refuses the whole attribute. A missing semicolon must not cost
/// the diagram, so it is put back.
pub(crate) fn unescape_attribute_value<'a>(attribute: &Attribute<'a>) -> Result<Cow<'a, str>> {
    match attribute.unescape_value() {
        Ok(value) => Ok(value),
        Err(error) => {
            let escaped = std::str::from_utf8(attribute.value.as_ref())?;
            let incomplete_entity = escaped
                .rfind('&')
                .and_then(|start| escaped.get(start + 1..))
                .is_some_and(|name| {
                    !name.is_empty()
                        && name
                            .chars()
                            .all(|character| character.is_ascii_alphanumeric() || character == '#')
                });
            if !incomplete_entity {
                return Err(error.into());
            }
            let completed = format!("{escaped};");
            let Ok(repaired) = quick_xml::escape::unescape(&completed) else {
                return Err(error.into());
            };
            Ok(Cow::Owned(repaired.into_owned()))
        }
    }
}

pub(super) fn postprocess(svg: &str, theme: &MermaidTheme) -> Result<(String, DiagramGeometry)> {
    // merman 0.6 already applies the generic resvg-safe cleanup before this point.
    // The remaining passes are Zed-specific theme and accent adjustments.
    let svg_id = extract_svg_id(svg);

    let mut reader = Reader::from_str(svg);
    reader.config_mut().check_end_names = false;
    let events = ReaderIter::new(reader);
    // merman's resvg-safe pipeline already removes foreignObject elements and
    // replaces their labels with native <text> fallback groups. This pass keeps
    // those fallback labels, but drops any that merely duplicate a native
    // <text> (e.g. user journey renders some labels both ways).
    let events = strip_foreignobject::process(events, svg);
    let events = center_edge_labels::process(events);
    // Geometry is read after the edge labels have been recentered, so the boxes
    // it reports are the ones that actually get painted, and before the passes
    // below, which only rewrite styles and classes.
    let geometry = Rc::new(RefCell::new(DiagramGeometry::default()));
    let events = collect_geometry::process(events, &svg_id, geometry.clone());
    let events = element_fixup::process(events, theme);

    let events = accent_colors::process(events, theme);
    let events = inject_css::process(events, theme, &svg_id);

    let mut writer = quick_xml::Writer::new(Vec::with_capacity(svg.len()));
    for event in events {
        writer.write_event(event?)?;
    }
    let svg = String::from_utf8(writer.into_inner()).context("SVG output is not valid UTF-8")?;
    Ok((svg, geometry.take()))
}

fn extract_svg_id(svg: &str) -> String {
    let mut reader = Reader::from_str(svg);
    reader.config_mut().check_end_names = false;
    for event in ReaderIter::new(reader) {
        let Ok(Event::Start(e) | Event::Empty(e)) = event else {
            continue;
        };
        if e.name().as_ref() == b"svg" {
            return e
                .try_get_attribute("id")
                .ok()
                .flatten()
                .and_then(|a| a.unescape_value().ok())
                .map(|v| v.into_owned())
                .unwrap_or_default();
        }
    }
    String::new()
}

struct ReaderIter<'a> {
    reader: Reader<&'a [u8]>,
    done: bool,
}

impl<'a> ReaderIter<'a> {
    fn new(reader: Reader<&'a [u8]>) -> Self {
        Self {
            reader,
            done: false,
        }
    }
}

impl<'a> Iterator for ReaderIter<'a> {
    type Item = Result<Event<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        match self.reader.read_event() {
            Ok(Event::Eof) => {
                self.done = true;
                None
            }
            Ok(event) => Some(Ok(event)),
            Err(e) => {
                self.done = true;
                Some(Err(e.into()))
            }
        }
    }
}
