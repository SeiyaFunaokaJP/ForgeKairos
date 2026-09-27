//! The Venn diagram half of the Mermaid editor's form.
//!
//! A Venn diagram is a list of sets and a list of overlaps between them. An
//! overlap names its sets by id, so its sets are picked from a menu of the ids
//! rather than typed, and renaming a set carries into every overlap, `text` and
//! `style` statement that names it. Deleting a set deletes the overlaps it is
//! part of with it, since Mermaid refuses an overlap of a set that is not
//! declared.

use editor::Editor;
use gpui::{AnyElement, App, Context, Entity, Window};
use mermaid_render::venn_model::{
    VennArea, VennDocument, is_valid_venn_id, is_valid_venn_label, is_valid_venn_size,
    is_valid_venn_title,
};
use ui::{ContextMenu, DropdownMenu, DropdownStyle, IconPosition, prelude::*};
use util::ResultExt as _;

use super::{
    Form, ID_FIELD_WIDTH, LABEL_FIELD_WIDTH, MermaidEditor, NUMBER_FIELD_WIDTH, add_button,
    field_frame, row_controls_labelled, section_title,
};

const NEW_SET_ID: &str = "S";

pub(super) struct VennForm {
    pub(super) document: VennDocument,
    pub(super) title: Entity<Editor>,
    /// One row per area, in the document's order.
    pub(super) areas: Vec<AreaRow>,
}

pub(super) struct AreaRow {
    /// The set's id; `None` for an overlap, whose sets are picked from a menu.
    pub(super) id: Option<Entity<Editor>>,
    pub(super) label: Entity<Editor>,
    pub(super) size: Entity<Editor>,
}

impl VennForm {
    pub(super) fn fields(&self) -> Vec<Entity<Editor>> {
        std::iter::once(self.title.clone())
            .chain(self.areas.iter().flat_map(|row| {
                row.id
                    .clone()
                    .into_iter()
                    .chain([row.label.clone(), row.size.clone()])
            }))
            .collect()
    }

    /// The Venn diagram the form currently describes, with every overlap and
    /// statement pointed at the set ids as they are now typed.
    pub(super) fn current_document(&self, cx: &App) -> VennDocument {
        self.document_with_renames(cx, |_, _| true)
    }

    /// The document the fields describe, with the overlaps and statements
    /// carried along only by the renames `follow` accepts. A rename it refuses
    /// leaves the stored id as it was, so the rename is still pending once it
    /// settles.
    fn document_with_renames(&self, cx: &App, follow: impl Fn(&str, &str) -> bool) -> VennDocument {
        let mut document = self.document.clone();
        let title = self.title.read(cx).text(cx).trim().to_string();
        document.title = if title.is_empty() {
            document.title.take().filter(|title| title.is_empty())
        } else {
            Some(title)
        };
        let mut renames = Vec::new();
        for (area, row) in document.areas.iter_mut().zip(&self.areas) {
            if let (Some(field), Some(id)) = (&row.id, area.sets.first())
                && area.is_set()
            {
                let typed = field.read(cx).text(cx).trim().to_string();
                if typed != *id && follow(id, &typed) {
                    renames.push((id.clone(), typed));
                }
            }
            let label = row.label.read(cx).text(cx).trim().to_string();
            area.label = (!label.is_empty()).then_some(label);
            let size = row.size.read(cx).text(cx).trim().to_string();
            area.size = (!size.is_empty()).then_some(size);
        }
        document.rename_sets(&renames);
        document
    }
}

/// The first thing wrong with what the user typed, in the words shown above the
/// form. Every check lives in `VennDocument::structure_error`, which the caller
/// consults right after this.
pub(super) fn problem(_document: &VennDocument) -> Option<SharedString> {
    None
}

impl MermaidEditor {
    pub(super) fn new_venn_form(
        &mut self,
        document: VennDocument,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> VennForm {
        let title = self.new_field(
            document.title.clone().unwrap_or_default().as_str(),
            window,
            cx,
        );
        let contents = document
            .areas
            .iter()
            .map(|area| {
                (
                    area.is_set()
                        .then(|| area.sets.first().cloned().unwrap_or_default()),
                    area.label.clone().unwrap_or_default(),
                    area.size.clone().unwrap_or_default(),
                )
            })
            .collect::<Vec<_>>();
        let areas = contents
            .iter()
            .map(|(id, label, size)| AreaRow {
                id: id.as_ref().map(|id| self.new_field(id, window, cx)),
                label: self.new_field(label, window, cx),
                size: self.new_field(size, window, cx),
            })
            .collect();
        VennForm {
            document,
            title,
            areas,
        }
    }

    pub(super) fn venn(&self) -> Option<&VennForm> {
        match &self.form {
            Form::Venn(form) => Some(form),
            _ => None,
        }
    }

    pub(super) fn venn_mut(&mut self) -> Option<&mut VennForm> {
        match &mut self.form {
            Form::Venn(form) => Some(form),
            _ => None,
        }
    }

    /// Reads the fields into the document before a structural change, so that
    /// a rename already typed is what the change works on.
    ///
    /// An id typed onto one that another set still has or is still known by,
    /// or not yet an id at all, is not a rename the overlaps can follow yet:
    /// taking them along now would hand them to the wrong set once the id is
    /// typed back.
    fn sync_venn(&mut self, cx: &mut Context<Self>) {
        let Some(document) = self.venn().map(|form| {
            let typed = form
                .areas
                .iter()
                .filter_map(|row| row.id.as_ref())
                .map(|field| field.read(cx).text(cx).trim().to_string())
                .collect::<Vec<_>>();
            let stored = form.document.set_ids();
            form.document_with_renames(cx, |_, new| {
                is_valid_venn_id(new)
                    && typed.iter().filter(|id| *id == new).count() == 1
                    && !stored.contains(&new)
            })
        }) else {
            return;
        };
        if let Some(form) = self.venn_mut() {
            form.document = document;
        }
    }

    fn venn_changed(&mut self, cx: &mut Context<Self>) {
        self.schedule_preview(cx);
        cx.notify();
    }

    pub(super) fn add_venn_set(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_venn(cx);
        let taken = self
            .venn()
            .map(|form| {
                form.document
                    .set_ids()
                    .into_iter()
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let id = (1..)
            .map(|number| format!("{NEW_SET_ID}{number}"))
            .find(|id| !taken.contains(id))
            .unwrap_or_else(|| NEW_SET_ID.to_string());
        let row = AreaRow {
            id: Some(self.new_field(&id, window, cx)),
            label: self.new_field("", window, cx),
            size: self.new_field("", window, cx),
        };
        let Some(form) = self.venn_mut() else {
            return;
        };
        // A new set goes after the last set, where every overlap below it can
        // come to use it.
        let position = form
            .document
            .areas
            .iter()
            .rposition(VennArea::is_set)
            .map_or(0, |index| index + 1)
            .min(form.areas.len());
        form.document.areas.insert(
            position,
            VennArea {
                sets: vec![id],
                label: None,
                size: None,
                notes: Vec::new(),
            },
        );
        form.areas.insert(position, row);
        self.venn_changed(cx);
    }

    pub(super) fn add_venn_union(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_venn(cx);
        let sets = self
            .venn()
            .map(|form| {
                form.document
                    .set_ids()
                    .into_iter()
                    .take(2)
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if sets.len() < 2 {
            self.message = Some("先に集合を 2 つ以上追加してください。".into());
            cx.notify();
            return;
        }
        let row = AreaRow {
            id: None,
            label: self.new_field("", window, cx),
            size: self.new_field("", window, cx),
        };
        let Some(form) = self.venn_mut() else {
            return;
        };
        form.document.areas.push(VennArea {
            sets,
            label: None,
            size: None,
            notes: Vec::new(),
        });
        form.areas.push(row);
        self.venn_changed(cx);
    }

    /// Deletes an area. A set takes the overlaps, notes and styles that name
    /// it along, which the delete button's tooltip says.
    pub(super) fn remove_venn_area(&mut self, index: usize, cx: &mut Context<Self>) {
        self.sync_venn(cx);
        let Some(form) = self.venn_mut() else {
            return;
        };
        if index >= form.areas.len() || index >= form.document.areas.len() {
            return;
        }
        let removed = if form.document.areas.get(index).is_some_and(VennArea::is_set) {
            form.document.remove_set(index)
        } else {
            form.document.areas.remove(index);
            vec![index]
        };
        for position in removed.into_iter().rev() {
            if position < form.areas.len() {
                form.areas.remove(position);
            }
        }
        self.venn_changed(cx);
    }

    /// Trades places with the nearest area of the same kind, so that the sets
    /// and the overlaps each keep their own order.
    pub(super) fn move_venn_area(&mut self, index: usize, forward: bool, cx: &mut Context<Self>) {
        self.sync_venn(cx);
        let Some(form) = self.venn_mut() else {
            return;
        };
        let Some(other) = venn_neighbour(&form.document.areas, index, forward) else {
            return;
        };
        if other >= form.areas.len() || index >= form.areas.len() {
            return;
        }
        form.document.areas.swap(index, other);
        form.areas.swap(index, other);
        self.venn_changed(cx);
    }

    /// Adds `id` to the overlap at `index`, or takes it away. An overlap of a
    /// single set would be written as a second `set`, so the last two stay.
    pub(super) fn toggle_venn_member(&mut self, index: usize, id: String, cx: &mut Context<Self>) {
        self.sync_venn(cx);
        let Some(area) = self
            .venn_mut()
            .and_then(|form| form.document.areas.get_mut(index))
            .filter(|area| !area.is_set())
        else {
            return;
        };
        let refused = match area.sets.iter().position(|member| *member == id) {
            Some(_) if area.sets.len() <= 2 => true,
            Some(position) => {
                area.sets.remove(position);
                false
            }
            None => {
                area.sets.push(id);
                false
            }
        };
        if refused {
            self.message = Some("重なりには集合が 2 つ以上必要です。".into());
            cx.notify();
            return;
        }
        self.venn_changed(cx);
    }

    pub(super) fn render_venn(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(form) = self.venn() else {
            return Vec::new();
        };
        let current = form.current_document(cx);

        let header = h_flex()
            .w_full()
            .gap_2()
            .child(section_title("タイトル"))
            .child(
                field_frame(
                    &form.title,
                    LABEL_FIELD_WIDTH,
                    current
                        .title
                        .as_ref()
                        .is_none_or(|title| is_valid_venn_title(title)),
                    window,
                    cx,
                )
                .flex_1()
                .child(form.title.clone()),
            )
            .into_any_element();

        let mut elements = vec![
            header,
            self.render_venn_areas(form, &current, true, window, cx),
            self.render_venn_areas(form, &current, false, window, cx),
        ];
        let kept = current.texts.len()
            + current.styles.len()
            + current
                .areas
                .iter()
                .map(|area| area.notes.len())
                .sum::<usize>();
        if kept > 0 {
            elements.push(
                Label::new(format!(
                    "text・style の文 {kept} 件はそのまま残し、集合の改名と削除に追随させます。"
                ))
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element(),
            );
        }
        elements
    }

    fn render_venn_areas(
        &self,
        form: &VennForm,
        current: &VennDocument,
        sets: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let set_ids = current
            .set_ids()
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let mut section = v_flex().w_full().gap_1().child(section_title(if sets {
            "集合 (ID・ラベル・大きさ)"
        } else {
            "重なり (集合・ラベル・大きさ)"
        }));
        for index in 0..form.areas.len() {
            let (Some(row), Some(area)) = (form.areas.get(index), current.areas.get(index)) else {
                continue;
            };
            if area.is_set() != sets {
                continue;
            }
            let mut line = h_flex().w_full().gap_1();
            line = match (&row.id, area.sets.first()) {
                (Some(field), Some(id)) if sets => line.child(
                    field_frame(
                        field,
                        ID_FIELD_WIDTH,
                        is_valid_venn_id(id)
                            && set_ids.iter().filter(|other| *other == id).count() == 1,
                        window,
                        cx,
                    )
                    .child(field.clone()),
                ),
                _ => line
                    .child(self.render_venn_member_menu(index, &area.sets, &set_ids, window, cx)),
            };
            let removes = if sets {
                "削除 (この集合を含む重なり・text・style も削除)"
            } else {
                "削除"
            };
            line = line
                .child(
                    field_frame(
                        &row.label,
                        LABEL_FIELD_WIDTH,
                        area.label
                            .as_ref()
                            .is_none_or(|label| is_valid_venn_label(label)),
                        window,
                        cx,
                    )
                    .flex_1()
                    .child(row.label.clone()),
                )
                .child(
                    field_frame(
                        &row.size,
                        NUMBER_FIELD_WIDTH,
                        area.size
                            .as_ref()
                            .is_none_or(|size| is_valid_venn_size(size)),
                        window,
                        cx,
                    )
                    .child(row.size.clone()),
                )
                .when(!area.notes.is_empty(), |this| {
                    this.child(
                        Label::new(format!("text {} 件", area.notes.len()))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                })
                .child(row_controls_labelled(
                    if sets {
                        "mermaid-venn-set"
                    } else {
                        "mermaid-venn-union"
                    },
                    index,
                    venn_neighbour(&current.areas, index, false).is_none(),
                    venn_neighbour(&current.areas, index, true).is_none(),
                    removes,
                    cx.listener(move |this, _, _window, cx| this.move_venn_area(index, false, cx)),
                    cx.listener(move |this, _, _window, cx| this.move_venn_area(index, true, cx)),
                    cx.listener(move |this, _, _window, cx| this.remove_venn_area(index, cx)),
                ));
            section = section.child(line);
        }
        let add = if sets {
            add_button(
                "mermaid-venn-add-set",
                "集合を追加",
                cx.listener(|this, _, window, cx| this.add_venn_set(window, cx)),
            )
            .into_any_element()
        } else {
            add_button(
                "mermaid-venn-add-union",
                "重なりを追加",
                cx.listener(|this, _, window, cx| this.add_venn_union(window, cx)),
            )
            .into_any_element()
        };
        section.child(add).into_any_element()
    }

    fn render_venn_member_menu(
        &self,
        index: usize,
        members: &[String],
        set_ids: &[String],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        let selected = members.to_vec();
        let choices = set_ids.to_vec();
        let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
            for id in choices.clone() {
                let this = this.clone();
                let is_selected = selected.contains(&id);
                menu = menu.toggleable_entry(
                    id.clone(),
                    is_selected,
                    IconPosition::Start,
                    None,
                    move |_window, cx| {
                        let id = id.clone();
                        this.update(cx, |this, cx| this.toggle_venn_member(index, id, cx))
                            .log_err();
                    },
                );
            }
            menu
        });

        DropdownMenu::new(
            ("mermaid-venn-members", index),
            SharedString::from(members.join(", ")),
            menu,
        )
        .style(DropdownStyle::Outlined)
        .trigger_size(ButtonSize::Compact)
        .into_any_element()
    }

    pub(super) fn render_venn_toolbar(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> AnyElement {
        div().into_any_element()
    }
}

/// The nearest area before or after `index` of the same kind — set or overlap.
fn venn_neighbour(areas: &[VennArea], index: usize, forward: bool) -> Option<usize> {
    let is_set = areas.get(index)?.is_set();
    if forward {
        areas
            .iter()
            .enumerate()
            .skip(index + 1)
            .find(|(_, area)| area.is_set() == is_set)
            .map(|(position, _)| position)
    } else {
        areas
            .iter()
            .enumerate()
            .take(index)
            .rev()
            .find(|(_, area)| area.is_set() == is_set)
            .map(|(position, _)| position)
    }
}

#[cfg(test)]
#[path = "venn_tests.rs"]
mod venn_tests;
