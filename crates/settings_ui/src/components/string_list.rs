use std::rc::Rc;

use gpui::ElementId;
use ui::{Tooltip, prelude::*};

use crate::components::SettingsInputField;

/// Edits a list of strings: a row per entry with a button that removes it, and
/// a field at the end that adds what is typed when Enter is pressed.
#[derive(IntoElement)]
pub struct StringListEditor {
    id: SharedString,
    entries: Vec<String>,
    aria_label: Option<SharedString>,
    on_add: Rc<dyn Fn(String, &mut Window, &mut App)>,
    on_remove: Rc<dyn Fn(String, &mut Window, &mut App)>,
}

impl StringListEditor {
    /// The `id` must be unique in the window; derive it from the setting's
    /// JSON path.
    pub fn new(
        id: impl Into<SharedString>,
        entries: Vec<String>,
        on_add: impl Fn(String, &mut Window, &mut App) + 'static,
        on_remove: impl Fn(String, &mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            id: id.into(),
            entries,
            aria_label: None,
            on_add: Rc::new(on_add),
            on_remove: Rc::new(on_remove),
        }
    }

    /// Sets the label assistive technology announces for the field that adds
    /// an entry.
    pub fn aria_label(mut self, label: impl Into<SharedString>) -> Self {
        self.aria_label = Some(label.into());
        self
    }
}

/// `entries` with `text` added at the end, or `None` when `text` is blank or
/// already in the list. Surrounding whitespace is not part of an entry.
pub fn string_list_with_added(entries: &[String], text: &str) -> Option<Vec<String>> {
    let text = text.trim();
    if text.is_empty() || entries.iter().any(|entry| entry == text) {
        return None;
    }
    let mut entries = entries.to_vec();
    entries.push(text.to_string());
    Some(entries)
}

/// `entries` without `removed`.
pub fn string_list_without(entries: &[String], removed: &str) -> Vec<String> {
    entries
        .iter()
        .filter(|entry| entry.as_str() != removed)
        .cloned()
        .collect()
}

impl RenderOnce for StringListEditor {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let on_add = self.on_add;
        let on_remove = self.on_remove;
        let is_empty = self.entries.is_empty();

        v_flex()
            .id(ElementId::Name(self.id.clone()))
            .w_64()
            .gap_1()
            .when(is_empty, |this| {
                this.child(
                    Label::new(i18n::t!("No entries"))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
            .children(self.entries.into_iter().enumerate().map(|(index, entry)| {
                let on_remove = on_remove.clone();
                h_flex()
                    .w_full()
                    .min_w_0()
                    .gap_2()
                    .justify_between()
                    .child(div().min_w_0().child(Label::new(entry.clone()).truncate()))
                    .child(
                        IconButton::new(("remove-entry", index), IconName::Trash)
                            .icon_size(IconSize::Small)
                            .icon_color(Color::Muted)
                            .tab_index(0_isize)
                            .aria_label(i18n::t!("Remove {entry}", entry = entry.as_str()))
                            .tooltip(Tooltip::text(i18n::t!("Remove")))
                            .on_click(move |_, window, cx| {
                                on_remove(entry.clone(), window, cx);
                            }),
                    )
            }))
            .child(
                SettingsInputField::new(ElementId::Name(format!("{}-add-entry", self.id).into()))
                    .tab_index(0)
                    .with_placeholder(i18n::t!("Add an entry"))
                    .when_some(self.aria_label, |field, label| field.aria_label(label))
                    .display_confirm_button()
                    .clear_on_confirm()
                    .on_confirm(move |text, window, cx| {
                        if let Some(text) = text {
                            on_add(text, window, cx);
                        }
                    }),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{string_list_with_added, string_list_without};

    fn list(entries: &[&str]) -> Vec<String> {
        entries.iter().map(|entry| entry.to_string()).collect()
    }

    #[test]
    fn adding_appends_trimmed_text_once() {
        let entries = list(&["printk", "assert"]);
        assert_eq!(
            string_list_with_added(&entries, "  likely "),
            Some(list(&["printk", "assert", "likely"]))
        );
        assert_eq!(string_list_with_added(&entries, "assert"), None);
        assert_eq!(string_list_with_added(&entries, " assert\t"), None);
        assert_eq!(string_list_with_added(&entries, ""), None);
        assert_eq!(string_list_with_added(&entries, "   "), None);
        assert_eq!(
            string_list_with_added(&[], "printf"),
            Some(list(&["printf"]))
        );
    }

    #[test]
    fn removing_keeps_the_other_entries_in_order() {
        let entries = list(&["printk", "assert", "likely"]);
        assert_eq!(
            string_list_without(&entries, "assert"),
            list(&["printk", "likely"])
        );
        assert_eq!(string_list_without(&entries, "missing"), entries);
        assert_eq!(string_list_without(&list(&["only"]), "only"), list(&[]));
    }
}
