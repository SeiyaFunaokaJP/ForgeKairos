use std::{collections::HashMap, rc::Rc};

use gpui::{ElementId, Entity};
use ui::{Tooltip, prelude::*};

use crate::components::SettingsInputField;

/// Edits a list of strings: a row per entry with a button that removes it, and
/// a field at the end that adds what is typed when Enter is pressed. With
/// `on_replace`, clicking an entry (or its pencil) turns the row into a field
/// that replaces the entry on Enter and leaves it as it was on Escape.
#[derive(IntoElement)]
pub struct StringListEditor {
    id: SharedString,
    entries: Vec<String>,
    aria_label: Option<SharedString>,
    on_add: Rc<dyn Fn(String, &mut Window, &mut App)>,
    on_remove: Rc<dyn Fn(String, &mut Window, &mut App)>,
    on_replace: Option<Rc<dyn Fn(EditedEntry, String, &mut Window, &mut App)>>,
}

/// An entry of the list, told apart from equal entries a hand-edited file may
/// hold by how many of them come before it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditedEntry {
    pub text: String,
    pub occurrence: usize,
}

/// The entry being edited in place. Each edit gets a field of its own
/// (`generation`), so a new one never starts with the text an earlier one
/// left behind.
#[derive(Clone, Default)]
struct InPlaceEdit {
    entry: Option<EditedEntry>,
    generation: usize,
    /// The text last confirmed is another entry's, so the field stays open
    /// and says why instead of closing as if it had been saved.
    duplicate: Option<String>,
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
            on_replace: None,
        }
    }

    /// Lets an entry be edited in place: `on_replace` gets the entry and the
    /// text it was changed to.
    pub fn on_replace(
        mut self,
        on_replace: impl Fn(EditedEntry, String, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_replace = Some(Rc::new(on_replace));
        self
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

/// `entries` with `original` changed to `text` where it stands, or `None` when
/// nothing would change or the change is one adding would refuse: blank text,
/// or text another entry already has. Emptying an entry does not remove it.
pub fn string_list_with_replaced(
    entries: &[String],
    original: &EditedEntry,
    text: &str,
) -> Option<Vec<String>> {
    let text = text.trim();
    if text.is_empty() || text == original.text || entries.iter().any(|entry| entry == text) {
        return None;
    }
    let (position, _) = entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| **entry == original.text)
        .nth(original.occurrence)?;
    let mut entries = entries.to_vec();
    if let Some(entry) = entries.get_mut(position) {
        *entry = text.to_string();
    }
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
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let on_add = self.on_add;
        let on_remove = self.on_remove;
        let on_replace = self.on_replace;
        let is_empty = self.entries.is_empty();
        let in_place_edit = window.use_keyed_state(
            ElementId::Name(format!("{}-in-place-edit", self.id).into()),
            cx,
            |_, _| InPlaceEdit::default(),
        );
        let InPlaceEdit {
            entry,
            generation,
            duplicate,
        } = in_place_edit.read(cx).clone();
        let mut seen: HashMap<&str, usize> = HashMap::new();
        let entries: Vec<EditedEntry> = self
            .entries
            .iter()
            .map(|text| {
                let count = seen.entry(text.as_str()).or_default();
                let occurrence = *count;
                *count += 1;
                EditedEntry {
                    text: text.clone(),
                    occurrence,
                }
            })
            .collect();
        let texts: Rc<[String]> = self.entries.iter().cloned().collect();
        // An entry that was removed or changed in the file meanwhile has
        // nothing left to edit.
        let edited_entry = entry.filter(|entry| on_replace.is_some() && entries.contains(entry));
        let id = self.id.clone();

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
            .children(entries.into_iter().enumerate().map(|(index, edited)| {
                let entry = edited.text.clone();
                if let Some(on_replace) = on_replace
                    .clone()
                    .filter(|_| edited_entry.as_ref() == Some(&edited))
                {
                    let stop_on_confirm = in_place_edit.clone();
                    let stop_on_cancel = in_place_edit.clone();
                    let texts = texts.clone();
                    let field = SettingsInputField::new(ElementId::Name(
                        format!("{id}-edit-entry-{generation}").into(),
                    ))
                    .tab_index(0)
                    .with_initial_text(entry.clone())
                    .aria_label(i18n::t!("Edit {entry}", entry = entry.as_str()))
                    .focus_when_shown()
                    .display_confirm_button()
                    .on_cancel(move |_, cx| {
                        stop_editing(&stop_on_cancel, cx);
                    })
                    .on_confirm(move |text, window, cx| {
                        if let Some(text) = text.as_deref()
                            && let Some(duplicate) = duplicate_of_another(&texts, &edited, text)
                        {
                            stop_on_confirm.update(cx, |state, cx| {
                                state.duplicate = Some(duplicate);
                                cx.notify();
                            });
                            return;
                        }
                        stop_editing(&stop_on_confirm, cx);
                        if let Some(text) = text {
                            on_replace(edited.clone(), text, window, cx);
                        }
                    });
                    return v_flex()
                        .w_full()
                        .gap_0p5()
                        .child(field)
                        .when_some(duplicate.clone(), |this, duplicate| {
                            let selector = format!("{id}-duplicate");
                            this.child(
                                div().debug_selector(move || selector).child(
                                    Label::new(i18n::t!(
                                        "{entry} is already in the list.",
                                        entry = duplicate.as_str()
                                    ))
                                    .size(LabelSize::Small)
                                    .color(Color::Error),
                                ),
                            )
                        })
                        .into_any_element();
                }
                let on_remove = on_remove.clone();
                let can_edit = on_replace.is_some();
                let start_from_label = in_place_edit.clone();
                let start_from_button = in_place_edit.clone();
                let entry_for_label = edited.clone();
                let entry_for_button = edited;
                let entry_selector = format!("{id}-entry-{index}");
                h_flex()
                    .w_full()
                    .min_w_0()
                    .gap_2()
                    .justify_between()
                    .child(
                        div()
                            .id(("entry", index))
                            .debug_selector(move || entry_selector)
                            .min_w_0()
                            .tooltip(Tooltip::text(entry.clone()))
                            .when(can_edit, |this| {
                                this.cursor_pointer().on_click(move |_, _, cx| {
                                    start_editing(&start_from_label, entry_for_label.clone(), cx);
                                })
                            })
                            .child(Label::new(entry.clone()).truncate()),
                    )
                    .child(
                        h_flex()
                            .flex_none()
                            .when(can_edit, |this| {
                                this.child(
                                    IconButton::new(("edit-entry", index), IconName::Pencil)
                                        .icon_size(IconSize::Small)
                                        .icon_color(Color::Muted)
                                        .tab_index(0_isize)
                                        .aria_label(i18n::t!(
                                            "Edit {entry}",
                                            entry = entry.as_str()
                                        ))
                                        .tooltip(Tooltip::text(i18n::t!("Edit")))
                                        .on_click(move |_, _, cx| {
                                            start_editing(
                                                &start_from_button,
                                                entry_for_button.clone(),
                                                cx,
                                            );
                                        }),
                                )
                            })
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
                            ),
                    )
                    .into_any_element()
            }))
            .child(
                SettingsInputField::new(ElementId::Name(format!("{}-add-entry", self.id).into()))
                    .tab_index(0)
                    .with_placeholder(i18n::t!("Add an entry"))
                    .when_some(self.aria_label, |field, label| field.aria_label(label))
                    .display_confirm_button()
                    .clear_on_confirm()
                    .clear_on_escape()
                    .on_confirm(move |text, window, cx| {
                        if let Some(text) = text {
                            on_add(text, window, cx);
                        }
                    }),
            )
    }
}

fn start_editing(in_place_edit: &Entity<InPlaceEdit>, entry: EditedEntry, cx: &mut App) {
    in_place_edit.update(cx, |state, cx| {
        state.entry = Some(entry);
        state.generation += 1;
        state.duplicate = None;
        cx.notify();
    });
}

fn stop_editing(in_place_edit: &Entity<InPlaceEdit>, cx: &mut App) {
    in_place_edit.update(cx, |state, cx| {
        state.entry = None;
        state.duplicate = None;
        cx.notify();
    });
}

/// The trimmed `text` when another entry of the list already has it, which
/// `string_list_with_replaced` refuses.
fn duplicate_of_another(entries: &[String], edited: &EditedEntry, text: &str) -> Option<String> {
    let text = text.trim();
    (text != edited.text && entries.iter().any(|entry| entry == text)).then(|| text.to_string())
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use editor::Editor;
    use gpui::{
        Entity, Focusable as _, KeyBinding, Modifiers, TestAppContext, VisualTestContext,
        WeakEntity,
    };
    use ui::prelude::*;

    use crate::components::SettingsInputField;

    use super::{
        EditedEntry, StringListEditor, duplicate_of_another, string_list_with_added,
        string_list_with_replaced, string_list_without,
    };

    fn list(entries: &[&str]) -> Vec<String> {
        entries.iter().map(|entry| entry.to_string()).collect()
    }

    gpui::actions!(string_list_test, [CloseTestWindow]);

    #[derive(Default)]
    struct Calls {
        added: Vec<String>,
        replaced: Vec<(String, usize, String)>,
        closed: usize,
    }

    /// The list inside a stand-in for the settings window, whose Escape
    /// binding closes it. Without entries, an ordinary field of the window
    /// stands in for the list.
    struct ListInWindow {
        entries: Option<Vec<String>>,
        calls: Rc<RefCell<Calls>>,
    }

    impl Render for ListInWindow {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let on_add = self.calls.clone();
            let on_replace = self.calls.clone();
            let on_close = self.calls.clone();
            div()
                .key_context("TestSettingsWindow")
                .on_action(move |_: &CloseTestWindow, _, _| {
                    on_close.borrow_mut().closed += 1;
                })
                .size_full()
                .map(|this| match self.entries.clone() {
                    Some(entries) => this.child(
                        StringListEditor::new(
                            "list",
                            entries,
                            move |text, _, _| on_add.borrow_mut().added.push(text),
                            |_, _, _| {},
                        )
                        .on_replace(move |original, text, _, _| {
                            on_replace.borrow_mut().replaced.push((
                                original.text,
                                original.occurrence,
                                text,
                            ));
                        }),
                    ),
                    None => this.child(
                        SettingsInputField::new("plain")
                            .with_initial_text("printk".to_string())
                            .display_confirm_button()
                            .on_confirm(move |text, _, _| {
                                on_add.borrow_mut().added.extend(text);
                            }),
                    ),
                })
        }
    }

    fn list_in_window(
        cx: &mut TestAppContext,
    ) -> (
        Rc<RefCell<Calls>>,
        Rc<RefCell<Vec<WeakEntity<Editor>>>>,
        &mut VisualTestContext,
    ) {
        list_of_entries_in_window(Some(list(&["printk", "assert"])), cx)
    }

    fn list_of_entries_in_window(
        entries: Option<Vec<String>>,
        cx: &mut TestAppContext,
    ) -> (
        Rc<RefCell<Calls>>,
        Rc<RefCell<Vec<WeakEntity<Editor>>>>,
        &mut VisualTestContext,
    ) {
        cx.update(|cx| {
            crate::test::register_settings(cx);
            cx.bind_keys([
                KeyBinding::new("escape", editor::actions::Cancel, Some("Editor")),
                KeyBinding::new("enter", menu::Confirm, Some("Editor")),
                KeyBinding::new("escape", CloseTestWindow, Some("TestSettingsWindow")),
            ]);
        });
        let editors: Rc<RefCell<Vec<WeakEntity<Editor>>>> = Rc::default();
        cx.update(|cx| {
            let editors = editors.clone();
            cx.observe_new(move |_: &mut Editor, _, cx| {
                editors.borrow_mut().push(cx.entity().downgrade());
            })
            .detach();
        });
        let calls: Rc<RefCell<Calls>> = Rc::default();
        let (_view, cx) = cx.add_window_view({
            let calls = calls.clone();
            |_, _| ListInWindow { entries, calls }
        });
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        (calls, editors, cx)
    }

    fn editor_at(editors: &Rc<RefCell<Vec<WeakEntity<Editor>>>>, index: usize) -> Entity<Editor> {
        editors
            .borrow()
            .get(index)
            .and_then(|editor| editor.upgrade())
            .unwrap_or_else(|| panic!("no live editor number {index}"))
    }

    fn text(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> String {
        editor.read_with(cx, |editor, cx| editor.text(cx))
    }

    #[gpui::test]
    fn escape_in_the_add_field_discards_the_text_instead_of_closing_the_window(
        cx: &mut TestAppContext,
    ) {
        let (calls, editors, cx) = list_in_window(cx);
        let add_field = editor_at(&editors, 0);
        cx.update(|window, cx| window.focus(&add_field.focus_handle(cx), cx));
        add_field.update_in(cx, |editor, window, cx| {
            editor.set_text("trace", window, cx)
        });
        cx.run_until_parked();

        cx.simulate_keystrokes("escape");
        assert_eq!(text(&add_field, cx), "");
        assert_eq!(
            calls.borrow().closed,
            0,
            "Escape with text typed closed the window"
        );
        assert!(calls.borrow().added.is_empty());

        // With nothing to discard, Escape closes the window as in any other
        // field of it.
        cx.simulate_keystrokes("escape");
        assert_eq!(calls.borrow().closed, 1);

        add_field.update_in(cx, |editor, window, cx| {
            editor.set_text("likely", window, cx)
        });
        cx.simulate_keystrokes("enter");
        assert_eq!(calls.borrow().added, ["likely"]);
    }

    #[gpui::test]
    fn escape_in_another_field_of_the_window_still_closes_it(cx: &mut TestAppContext) {
        let (calls, editors, cx) = list_of_entries_in_window(None, cx);
        let field = editor_at(&editors, 0);
        cx.update(|window, cx| window.focus(&field.focus_handle(cx), cx));
        cx.run_until_parked();
        cx.simulate_keystrokes("escape");
        assert_eq!(text(&field, cx), "printk");
        assert_eq!(calls.borrow().closed, 1);
    }

    #[gpui::test]
    fn an_entry_is_edited_in_place_and_escape_leaves_it_as_it_was(cx: &mut TestAppContext) {
        let (calls, editors, cx) = list_in_window(cx);
        let click_entry = |index: usize, cx: &mut VisualTestContext| {
            let selector: &'static str = if index == 0 {
                "list-entry-0"
            } else {
                "list-entry-1"
            };
            let bounds = cx
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("{selector} should be drawn"));
            cx.simulate_click(bounds.center(), Modifiers::none());
            cx.run_until_parked();
        };

        click_entry(1, cx);
        let edit_field = editor_at(&editors, 1);
        assert_eq!(text(&edit_field, cx), "assert");
        assert!(
            cx.update(|window, cx| edit_field.focus_handle(cx).is_focused(window)),
            "the field should take the focus"
        );
        assert!(cx.debug_bounds("list-entry-1").is_none());
        edit_field.update_in(cx, |editor, window, cx| {
            editor.set_text("BUG_ON", window, cx)
        });
        cx.simulate_keystrokes("enter");
        assert_eq!(
            calls.borrow().replaced,
            [("assert".to_string(), 0, "BUG_ON".to_string())]
        );
        assert!(cx.debug_bounds("list-entry-1").is_some());

        click_entry(0, cx);
        let edit_field = editor_at(&editors, 2);
        assert_eq!(text(&edit_field, cx), "printk");
        edit_field.update_in(cx, |editor, window, cx| {
            editor.set_text("trace", window, cx)
        });
        cx.simulate_keystrokes("escape");
        assert_eq!(
            calls.borrow().replaced.len(),
            1,
            "Escape replaced the entry"
        );
        assert_eq!(calls.borrow().closed, 0, "Escape closed the window");
        assert!(cx.debug_bounds("list-entry-0").is_some());

        // Emptying the entry and confirming leaves it in place.
        click_entry(0, cx);
        let edit_field = editor_at(&editors, 3);
        edit_field.update_in(cx, |editor, window, cx| editor.set_text("", window, cx));
        cx.simulate_keystrokes("enter");
        assert_eq!(calls.borrow().replaced.len(), 1);
        assert!(cx.debug_bounds("list-entry-0").is_some());
    }

    #[gpui::test]
    fn an_edit_to_another_entrys_text_keeps_the_field_open(cx: &mut TestAppContext) {
        let (calls, editors, cx) = list_in_window(cx);
        let bounds = cx
            .debug_bounds("list-entry-1")
            .unwrap_or_else(|| panic!("list-entry-1 should be drawn"));
        cx.simulate_click(bounds.center(), Modifiers::none());
        cx.run_until_parked();
        let edit_field = editor_at(&editors, 1);
        edit_field.update_in(cx, |editor, window, cx| {
            editor.set_text(" printk ", window, cx)
        });
        cx.simulate_keystrokes("enter");

        assert!(calls.borrow().replaced.is_empty());
        assert!(
            cx.debug_bounds("list-entry-1").is_none(),
            "the field closed as if the edit had been saved"
        );
        assert!(
            cx.debug_bounds("list-duplicate").is_some(),
            "no reason shown"
        );
        assert_eq!(text(&edit_field, cx), " printk ", "the typed text is kept");

        edit_field.update_in(cx, |editor, window, cx| {
            editor.set_text("trace", window, cx)
        });
        cx.simulate_keystrokes("enter");
        assert_eq!(
            calls.borrow().replaced,
            [("assert".to_string(), 0, "trace".to_string())]
        );
        assert!(cx.debug_bounds("list-duplicate").is_none());
    }

    #[test]
    fn only_another_entrys_text_is_a_duplicate() {
        let entries = list(&["printk", "assert"]);
        assert_eq!(
            duplicate_of_another(&entries, &first("assert"), " printk "),
            Some("printk".to_string())
        );
        assert_eq!(
            duplicate_of_another(&entries, &first("assert"), "assert"),
            None
        );
        assert_eq!(
            duplicate_of_another(&entries, &first("assert"), "trace"),
            None
        );
    }

    #[gpui::test]
    fn of_two_equal_entries_only_the_clicked_one_is_edited(cx: &mut TestAppContext) {
        let (calls, editors, cx) =
            list_of_entries_in_window(Some(list(&["printk", "printk", "assert"])), cx);
        let bounds = cx
            .debug_bounds("list-entry-1")
            .unwrap_or_else(|| panic!("list-entry-1 should be drawn"));
        cx.simulate_click(bounds.center(), Modifiers::none());
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("list-entry-0").is_some(),
            "the other printk turned into a field too"
        );
        assert!(cx.debug_bounds("list-entry-1").is_none());
        assert_eq!(
            editors
                .borrow()
                .iter()
                .filter(|editor| editor.upgrade().is_some())
                .count(),
            2,
            "one field to add and one to edit"
        );
        let edit_field = editor_at(&editors, 1);
        edit_field.update_in(cx, |editor, window, cx| {
            editor.set_text("trace", window, cx)
        });
        cx.simulate_keystrokes("enter");
        assert_eq!(
            calls.borrow().replaced,
            [("printk".to_string(), 1, "trace".to_string())]
        );
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

    fn first(text: &str) -> EditedEntry {
        EditedEntry {
            text: text.to_string(),
            occurrence: 0,
        }
    }

    #[test]
    fn replacing_keeps_the_place_and_follows_the_rules_of_adding() {
        let entries = list(&["printk", "assert", "likely"]);
        assert_eq!(
            string_list_with_replaced(&entries, &first("assert"), "  BUG_ON "),
            Some(list(&["printk", "BUG_ON", "likely"]))
        );
        // Emptying an entry cancels the edit rather than removing it.
        assert_eq!(
            string_list_with_replaced(&entries, &first("assert"), ""),
            None
        );
        assert_eq!(
            string_list_with_replaced(&entries, &first("assert"), " \t"),
            None
        );
        // Unchanged, or the same as another entry.
        assert_eq!(
            string_list_with_replaced(&entries, &first("assert"), "assert "),
            None
        );
        assert_eq!(
            string_list_with_replaced(&entries, &first("assert"), "likely"),
            None
        );
        // The entry went away before the edit was written.
        assert_eq!(
            string_list_with_replaced(&entries, &first("missing"), "trace"),
            None
        );
        // Of equal entries, the one that was clicked; none when the file has
        // fewer of them by the time the edit is written.
        let entries = list(&["printk", "assert", "printk"]);
        let second = EditedEntry {
            text: "printk".to_string(),
            occurrence: 1,
        };
        assert_eq!(
            string_list_with_replaced(&entries, &second, "trace"),
            Some(list(&["printk", "assert", "trace"]))
        );
        assert_eq!(
            string_list_with_replaced(&list(&["printk", "assert"]), &second, "trace"),
            None
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
