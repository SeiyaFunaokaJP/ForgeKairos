//! The class menu of the state and ER forms, in the same shape as the
//! flowchart's: the names on offer are the ones a `classDef` declares, the ones
//! already assigned, and one typed into the form's own class field, since the
//! form has no way to write a `classDef` line.

use std::rc::Rc;

use editor::Editor;
use gpui::{AnyElement, App, ElementId, Entity, Window};
use ui::{ContextMenu, DropdownMenu, DropdownStyle, IconPosition, prelude::*};

use super::{ID_FIELD_WIDTH, MermaidEditor, class_menu_label, field_frame, section_title};

/// What picking a class in the menu does: add it when it is not assigned, and
/// take it away when it is.
pub(super) type ToggleClass = Rc<dyn Fn(String, &mut Window, &mut App)>;

/// The class names `classDef` statements among `lines` declare, in the order
/// they are written. A statement may declare several at once (`classDef a,b
/// fill:#f96`), and a name `is_valid` refuses is one the form could not write
/// back, so it is not offered.
pub(super) fn declared_class_names<'a>(
    lines: impl IntoIterator<Item = &'a str>,
    is_valid: impl Fn(&str) -> bool,
) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for text in lines {
        for line in text.lines() {
            let Some(rest) = line.trim().strip_prefix("classDef") else {
                continue;
            };
            if !rest.starts_with(char::is_whitespace) {
                continue;
            }
            let Some(list) = rest.split_whitespace().next() else {
                continue;
            };
            for name in list.trim_end_matches(';').split(',') {
                let name = name.trim();
                if is_valid(name) && !names.iter().any(|known| known == name) {
                    names.push(name.to_string());
                }
            }
        }
    }
    names
}

/// `declared` followed by every name in `assigned` and the one `typed`, each
/// once.
pub(super) fn class_candidates<'a>(
    mut declared: Vec<String>,
    assigned: impl IntoIterator<Item = &'a String>,
    typed: &str,
    is_valid: impl Fn(&str) -> bool,
) -> Vec<String> {
    for name in assigned {
        if !declared.contains(name) {
            declared.push(name.clone());
        }
    }
    let typed = typed.trim();
    if is_valid(typed) && !declared.iter().any(|known| known == typed) {
        declared.push(typed.to_string());
    }
    declared
}

pub(super) fn render_class_menu(
    id: impl Into<ElementId>,
    assigned: &[String],
    candidates: &[String],
    on_toggle: ToggleClass,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let label = class_menu_label(assigned);
    let assigned = assigned.to_vec();
    let candidates = candidates.to_vec();
    let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
        if candidates.is_empty() {
            return menu.label(i18n::t!(
                "Write a classDef, or type a name in \"Class Name\"."
            ));
        }
        for name in candidates.clone() {
            let on_toggle = on_toggle.clone();
            let is_selected = assigned.contains(&name);
            let chosen = name.clone();
            menu = menu.toggleable_entry(
                name,
                is_selected,
                IconPosition::Start,
                None,
                move |window, cx| on_toggle(chosen.clone(), window, cx),
            );
        }
        menu
    });

    DropdownMenu::new(id, label, menu)
        .style(DropdownStyle::Outlined)
        .trigger_size(ButtonSize::Compact)
        .into_any_element()
}

/// The field a class name with no `classDef` yet is typed into, so that the
/// class menus can offer it.
pub(super) fn render_class_input(
    field: &Entity<Editor>,
    is_valid: impl Fn(&str) -> bool,
    window: &mut Window,
    cx: &mut gpui::Context<MermaidEditor>,
) -> AnyElement {
    let typed = field.read(cx).text(cx);
    let typed_is_valid = typed.trim().is_empty() || is_valid(typed.trim());
    h_flex()
        .w_full()
        .gap_1()
        .child(section_title(i18n::t!("Class Name")))
        .child(field_frame(field, ID_FIELD_WIDTH, typed_is_valid, window, cx).child(field.clone()))
        .child(section_title(i18n::t!(
            "The classDef names and the name typed here are offered under \"Class\"."
        )))
        .into_any_element()
}

#[cfg(test)]
#[path = "class_menu_tests.rs"]
mod tests;
