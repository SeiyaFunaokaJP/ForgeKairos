//! The user journey half of the Mermaid editor's form.
//!
//! A journey is a title and a list of sections, each holding tasks, and a task
//! is a name, a score and the people taking part. The form is one group per
//! section with one row per task: the score is picked from a menu because
//! Mermaid only has faces for 1 to 5, and the people are one comma separated
//! field because that is how the fence writes them. A person's name is spread
//! over every task they take part in, so renaming one is a separate tool that
//! rewrites all of those fields at once.

use editor::Editor;
use gpui::{AnyElement, App, Context, Entity, Focusable as _, Window};
use mermaid_render::journey_model::{
    JOURNEY_SCORES, JourneyDocument, JourneySection, JourneyTask, are_valid_journey_people,
    is_valid_journey_person, is_valid_journey_section_name, is_valid_journey_task_name,
    is_valid_journey_title,
};
use ui::{ContextMenu, DropdownMenu, DropdownStyle, IconPosition, prelude::*};
use util::ResultExt as _;

use super::{
    Form, LABEL_FIELD_WIDTH, MermaidEditor, add_button, field_frame, neighbour, row_controls,
    row_controls_labelled, section_title,
};

/// The texts new rows are added with, so that they are visible in the drawing
/// before anything has been typed into them.
fn new_task_name() -> &'static str {
    i18n::tc!("mermaid journey", "New task")
}
const NEW_TASK_SCORE: u8 = 3;
const NEW_SECTION_NAME: &str = i18n::msg!("New section");

pub(super) struct JourneyForm {
    pub(super) document: JourneyDocument,
    pub(super) title: Entity<Editor>,
    pub(super) sections: Vec<SectionRow>,
    /// The person picked in the rename tool's menu.
    pub(super) rename_from: Option<String>,
    pub(super) rename_to: Entity<Editor>,
}

pub(super) struct SectionRow {
    pub(super) name: Entity<Editor>,
    pub(super) tasks: Vec<TaskRow>,
}

pub(super) struct TaskRow {
    pub(super) name: Entity<Editor>,
    pub(super) people: Entity<Editor>,
}

impl JourneyForm {
    pub(super) fn fields(&self) -> Vec<Entity<Editor>> {
        std::iter::once(self.title.clone())
            .chain(self.sections.iter().flat_map(|section| {
                std::iter::once(section.name.clone()).chain(
                    section
                        .tasks
                        .iter()
                        .flat_map(|task| [task.name.clone(), task.people.clone()]),
                )
            }))
            .chain(std::iter::once(self.rename_to.clone()))
            .collect()
    }

    /// The user journey the form currently describes: the structure and the
    /// scores held in `document` with the text fields' current contents written
    /// over it.
    pub(super) fn current_document(&self, cx: &App) -> JourneyDocument {
        let mut document = self.document.clone();
        document.title = field_value(&self.title, cx);
        for (section, row) in document.sections.iter_mut().zip(&self.sections) {
            section.name = field_value(&row.name, cx);
            for (task, task_row) in section.tasks.iter_mut().zip(&row.tasks) {
                task.name = field_text(&task_row.name, cx);
                task.people = people_from_text(&field_text(&task_row.people, cx), &task.people);
            }
        }
        document
    }
}

/// The first thing wrong with what the user typed, in the words shown above the
/// form. Every check lives in `JourneyDocument::structure_error`, which the
/// caller consults right after this; the red frames come from the
/// `is_valid_journey_*` predicates directly.
pub(super) fn problem(_document: &JourneyDocument) -> Option<SharedString> {
    None
}

fn field_text(field: &Entity<Editor>, cx: &App) -> String {
    field.read(cx).text(cx).trim().to_string()
}

/// A field's text, or `None` when it is empty. Mermaid draws nothing for a
/// `title` with nothing after it, so an empty field means "no statement".
fn field_value(field: &Entity<Editor>, cx: &App) -> Option<String> {
    let text = field_text(field, cx);
    (!text.is_empty()).then_some(text)
}

/// The people a task's field lists. An empty field is no people, except on a
/// task that was written with a trailing `:` and nobody after it: that one is
/// drawn with a nameless person, and a field the user left empty must not
/// quietly take the person away.
fn people_from_text(text: &str, previous: &[String]) -> Vec<String> {
    if text.is_empty() {
        return if previous == [String::new()] {
            previous.to_vec()
        } else {
            Vec::new()
        };
    }
    text.split(',')
        .map(|person| person.trim().to_string())
        .collect()
}

fn people_text(people: &[String]) -> String {
    people.join(", ")
}

/// Whether the section at `index` may trade places with the one beside it.
///
/// The band written before the first `section` line has no name, and only the
/// first band can go without one, so it is pinned to the front. The buttons and
/// the handler ask this together, so a button that is live can always be
/// pressed.
fn section_may_move(sections: &[JourneySection], index: usize, forward: bool) -> bool {
    let Some(other) = neighbour(index, forward, sections.len()) else {
        return false;
    };
    if index >= sections.len() {
        return false;
    }
    !((index == 0 || other == 0)
        && sections
            .first()
            .is_some_and(|section| section.name.is_none()))
}

fn score_label(score: u8) -> SharedString {
    match score {
        5 => i18n::t!("5 (Very Good)").into(),
        3 => i18n::t!("3 (Neutral)").into(),
        1 => i18n::t!("1 (Very Bad)").into(),
        score => score.to_string().into(),
    }
}

impl MermaidEditor {
    pub(super) fn new_journey_form(
        &mut self,
        document: JourneyDocument,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> JourneyForm {
        let title = self.new_field(
            document.title.clone().unwrap_or_default().as_str(),
            window,
            cx,
        );
        let mut sections = Vec::with_capacity(document.sections.len());
        for section in &document.sections {
            let name = self.new_field(
                section.name.clone().unwrap_or_default().as_str(),
                window,
                cx,
            );
            let mut tasks = Vec::with_capacity(section.tasks.len());
            for task in &section.tasks {
                tasks.push(self.new_journey_task_row(task, window, cx));
            }
            sections.push(SectionRow { name, tasks });
        }
        let rename_to = self.new_field("", window, cx);
        JourneyForm {
            rename_from: document.people().into_iter().next(),
            document,
            title,
            sections,
            rename_to,
        }
    }

    fn new_journey_task_row(
        &mut self,
        task: &JourneyTask,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> TaskRow {
        TaskRow {
            name: self.new_field(&task.name, window, cx),
            people: self.new_field(&people_text(&task.people), window, cx),
        }
    }

    pub(super) fn journey(&self) -> Option<&JourneyForm> {
        match &self.form {
            Form::Journey(form) => Some(form),
            _ => None,
        }
    }

    pub(super) fn journey_mut(&mut self) -> Option<&mut JourneyForm> {
        match &mut self.form {
            Form::Journey(form) => Some(form),
            _ => None,
        }
    }

    /// Reads the fields into the document before a structural change, which
    /// moves the document and the rows in step.
    fn sync_journey(&mut self, cx: &mut Context<Self>) {
        let Some(document) = self.journey().map(|form| form.current_document(cx)) else {
            return;
        };
        if let Some(form) = self.journey_mut() {
            form.document = document;
        }
    }

    fn journey_changed(&mut self, cx: &mut Context<Self>) {
        self.schedule_preview(cx);
        cx.notify();
    }

    pub(super) fn add_journey_section(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_journey(cx);
        let Some(form) = self.journey() else {
            return;
        };
        // Two sections with one name next to each other are drawn as one
        // band, so a new one gets a name no other section has.
        let taken = form
            .document
            .sections
            .iter()
            .filter_map(|section| section.name.as_deref())
            .collect::<Vec<_>>();
        let new_section_name = i18n::tr(NEW_SECTION_NAME);
        let name = std::iter::once(new_section_name.to_string())
            .chain((2..).map(|number| format!("{new_section_name} {number}")))
            .find(|name| !taken.contains(&name.as_str()))
            .unwrap_or_else(|| new_section_name.to_string());
        let field = self.new_field(&name, window, cx);
        let Some(form) = self.journey_mut() else {
            return;
        };
        form.document.sections.push(JourneySection {
            name: Some(name),
            tasks: Vec::new(),
        });
        form.sections.push(SectionRow {
            name: field,
            tasks: Vec::new(),
        });
        self.journey_changed(cx);
    }

    pub(super) fn remove_journey_section(&mut self, index: usize, cx: &mut Context<Self>) {
        self.sync_journey(cx);
        let Some(form) = self.journey_mut() else {
            return;
        };
        if index >= form.sections.len() || index >= form.document.sections.len() {
            return;
        }
        form.document.sections.remove(index);
        form.sections.remove(index);
        self.journey_changed(cx);
    }

    pub(super) fn move_journey_section(
        &mut self,
        index: usize,
        forward: bool,
        cx: &mut Context<Self>,
    ) {
        self.sync_journey(cx);
        let Some(form) = self.journey_mut() else {
            return;
        };
        let Some(other) = neighbour(index, forward, form.sections.len()) else {
            return;
        };
        if other >= form.document.sections.len()
            || !section_may_move(&form.document.sections, index, forward)
        {
            return;
        }
        form.document.sections.swap(index, other);
        form.sections.swap(index, other);
        self.journey_changed(cx);
    }

    pub(super) fn add_journey_task(
        &mut self,
        section: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sync_journey(cx);
        let Some(form) = self.journey() else {
            return;
        };
        // A journey with no `section` line still draws its tasks; the first
        // task added to one opens the nameless band they live in.
        let needs_band = form.document.sections.is_empty() && section == 0;
        if !needs_band && section >= form.document.sections.len() {
            return;
        }
        let band = needs_band.then(|| self.new_field("", window, cx));
        let task = JourneyTask {
            name: new_task_name().to_string(),
            score: NEW_TASK_SCORE,
            people: Vec::new(),
        };
        let row = self.new_journey_task_row(&task, window, cx);
        let focus = row.name.clone();
        let Some(form) = self.journey_mut() else {
            return;
        };
        if let Some(name) = band {
            form.document.sections.push(JourneySection {
                name: None,
                tasks: Vec::new(),
            });
            form.sections.push(SectionRow {
                name,
                tasks: Vec::new(),
            });
        }
        let (Some(tasks), Some(rows)) = (
            form.document.sections.get_mut(section),
            form.sections.get_mut(section),
        ) else {
            return;
        };
        tasks.tasks.push(task);
        rows.tasks.push(row);
        focus.focus_handle(cx).focus(window, cx);
        self.journey_changed(cx);
    }

    pub(super) fn remove_journey_task(
        &mut self,
        section: usize,
        index: usize,
        cx: &mut Context<Self>,
    ) {
        self.sync_journey(cx);
        let Some(form) = self.journey_mut() else {
            return;
        };
        let (Some(tasks), Some(rows)) = (
            form.document.sections.get_mut(section),
            form.sections.get_mut(section),
        ) else {
            return;
        };
        if index >= tasks.tasks.len() || index >= rows.tasks.len() {
            return;
        }
        tasks.tasks.remove(index);
        rows.tasks.remove(index);
        self.journey_changed(cx);
    }

    /// Moves a task within its section, or into the section next to it when it
    /// is already at the end it is moving towards.
    pub(super) fn move_journey_task(
        &mut self,
        section: usize,
        index: usize,
        forward: bool,
        cx: &mut Context<Self>,
    ) {
        self.sync_journey(cx);
        let Some(form) = self.journey_mut() else {
            return;
        };
        let length = match (
            form.document.sections.get(section),
            form.sections.get(section),
        ) {
            (Some(tasks), Some(rows)) if tasks.tasks.len() == rows.tasks.len() => tasks.tasks.len(),
            _ => return,
        };
        if index >= length {
            return;
        }
        if let Some(other) = neighbour(index, forward, length) {
            if let Some(tasks) = form.document.sections.get_mut(section) {
                tasks.tasks.swap(index, other);
            }
            if let Some(rows) = form.sections.get_mut(section) {
                rows.tasks.swap(index, other);
            }
        } else {
            let Some(target) = (if forward {
                (section + 1 < form.sections.len()).then_some(section + 1)
            } else {
                section.checked_sub(1)
            }) else {
                return;
            };
            if target >= form.document.sections.len() {
                return;
            }
            let Some(task) = form
                .document
                .sections
                .get_mut(section)
                .map(|tasks| tasks.tasks.remove(index))
            else {
                return;
            };
            let Some(row) = form
                .sections
                .get_mut(section)
                .map(|rows| rows.tasks.remove(index))
            else {
                return;
            };
            if let (Some(tasks), Some(rows)) = (
                form.document.sections.get_mut(target),
                form.sections.get_mut(target),
            ) {
                if forward {
                    tasks.tasks.insert(0, task);
                    rows.tasks.insert(0, row);
                } else {
                    tasks.tasks.push(task);
                    rows.tasks.push(row);
                }
            }
        }
        self.journey_changed(cx);
    }

    pub(super) fn set_journey_score(
        &mut self,
        section: usize,
        index: usize,
        score: u8,
        cx: &mut Context<Self>,
    ) {
        self.sync_journey(cx);
        let Some(task) = self.journey_mut().and_then(|form| {
            form.document
                .sections
                .get_mut(section)
                .and_then(|section| section.tasks.get_mut(index))
        }) else {
            return;
        };
        task.score = score;
        self.journey_changed(cx);
    }

    pub(super) fn set_journey_rename_from(&mut self, person: String, cx: &mut Context<Self>) {
        if let Some(form) = self.journey_mut() {
            form.rename_from = Some(person);
        }
        cx.notify();
    }

    /// The new name the rename tool would write, when it can be written and
    /// differs from the old one.
    fn journey_rename_target(form: &JourneyForm, cx: &App) -> Option<(String, String)> {
        let from = form.rename_from.clone()?;
        let to = field_text(&form.rename_to, cx);
        (is_valid_journey_person(&to) && to != from).then_some((from, to))
    }

    /// Renames the person picked in the rename tool in every task, and writes
    /// the new lists into the tasks' fields.
    pub(super) fn apply_journey_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_journey(cx);
        let Some((from, to)) = self
            .journey()
            .and_then(|form| Self::journey_rename_target(form, cx))
        else {
            return;
        };
        let Some(form) = self.journey_mut() else {
            return;
        };
        form.document.rename_person(&from, &to);
        form.rename_from = Some(to);
        let updates = form
            .document
            .sections
            .iter()
            .zip(&form.sections)
            .flat_map(|(section, row)| {
                section
                    .tasks
                    .iter()
                    .zip(&row.tasks)
                    .map(|(task, task_row)| (task_row.people.clone(), people_text(&task.people)))
            })
            .collect::<Vec<_>>();
        let rename_to = form.rename_to.clone();
        for (field, text) in updates {
            if field.read(cx).text(cx).trim() != text {
                field.update(cx, |field, cx| field.set_text(text, window, cx));
            }
        }
        rename_to.update(cx, |field, cx| field.set_text("", window, cx));
        self.journey_changed(cx);
    }

    pub(super) fn render_journey(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(form) = self.journey() else {
            return Vec::new();
        };
        let current = form.current_document(cx);
        vec![
            h_flex()
                .w_full()
                .gap_2()
                .child(section_title(i18n::t!("Title")))
                .child(
                    field_frame(
                        &form.title,
                        LABEL_FIELD_WIDTH,
                        current
                            .title
                            .as_ref()
                            .is_none_or(|title| is_valid_journey_title(title)),
                        window,
                        cx,
                    )
                    .flex_1()
                    .child(form.title.clone()),
                )
                .into_any_element(),
            self.render_journey_sections(form, &current, window, cx),
            self.render_journey_people(form, &current, window, cx),
        ]
    }

    fn render_journey_sections(
        &self,
        form: &JourneyForm,
        current: &JourneyDocument,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut list = v_flex().w_full().gap_2().child(section_title(i18n::tc!(
            "mermaid journey",
            "Sections and Tasks (task name, score and people, separated by commas)",
        )));

        for index in 0..form.sections.len() {
            let (Some(row), Some(section)) =
                (form.sections.get(index), current.sections.get(index))
            else {
                continue;
            };
            let mut tasks = v_flex().w_full().gap_1().pl_4();
            for task_index in 0..row.tasks.len() {
                let (Some(task_row), Some(task)) =
                    (row.tasks.get(task_index), section.tasks.get(task_index))
                else {
                    continue;
                };
                tasks = tasks.child(self.render_journey_task(
                    index,
                    task_index,
                    row.tasks.len(),
                    form.sections.len(),
                    task_row,
                    task,
                    window,
                    cx,
                ));
            }

            list = list.child(
                v_flex()
                    .w_full()
                    .gap_1()
                    .child(
                        h_flex()
                            .w_full()
                            .gap_1()
                            .child(section_title(i18n::t!("Section")))
                            .child(
                                field_frame(
                                    &row.name,
                                    LABEL_FIELD_WIDTH,
                                    // Only the band before the first `section`
                                    // line may go without a name.
                                    section.name.as_ref().map_or(index == 0, |name| {
                                        is_valid_journey_section_name(name)
                                    }),
                                    window,
                                    cx,
                                )
                                .flex_1()
                                .child(row.name.clone()),
                            )
                            .child(row_controls_labelled(
                                "mermaid-journey-section",
                                index,
                                !section_may_move(&current.sections, index, false),
                                !section_may_move(&current.sections, index, true),
                                i18n::tc!("mermaid journey", "Delete the Section and Its Tasks"),
                                cx.listener(move |this, _, _window, cx| {
                                    this.move_journey_section(index, false, cx)
                                }),
                                cx.listener(move |this, _, _window, cx| {
                                    this.move_journey_section(index, true, cx)
                                }),
                                cx.listener(move |this, _, _window, cx| {
                                    this.remove_journey_section(index, cx)
                                }),
                            )),
                    )
                    .child(tasks)
                    .child(div().pl_4().child(add_button(
                        SharedString::from(format!("mermaid-journey-add-task-{index}")),
                        i18n::tc!("mermaid journey", "Add Task"),
                        cx.listener(move |this, _, window, cx| {
                            this.add_journey_task(index, window, cx)
                        }),
                    ))),
            );
        }

        if form.sections.is_empty() {
            list = list.child(add_button(
                "mermaid-journey-add-first-task",
                i18n::tc!("mermaid journey", "Add Task"),
                cx.listener(|this, _, window, cx| this.add_journey_task(0, window, cx)),
            ));
        }

        list.child(add_button(
            "mermaid-journey-add-section",
            i18n::t!("Add Section"),
            cx.listener(|this, _, window, cx| this.add_journey_section(window, cx)),
        ))
        .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn render_journey_task(
        &self,
        section: usize,
        index: usize,
        length: usize,
        section_count: usize,
        row: &TaskRow,
        task: &JourneyTask,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        h_flex()
            .w_full()
            .gap_1()
            .child(
                field_frame(
                    &row.name,
                    LABEL_FIELD_WIDTH,
                    is_valid_journey_task_name(&task.name),
                    window,
                    cx,
                )
                .child(row.name.clone()),
            )
            .child(self.render_journey_score_menu(section, index, task.score, window, cx))
            .child(
                field_frame(
                    &row.people,
                    LABEL_FIELD_WIDTH,
                    are_valid_journey_people(&task.people),
                    window,
                    cx,
                )
                .flex_1()
                .child(row.people.clone()),
            )
            .child(row_controls(
                &format!("mermaid-journey-task-{section}"),
                index,
                index == 0 && section == 0,
                index + 1 >= length && section + 1 >= section_count,
                cx.listener(move |this, _, _window, cx| {
                    this.move_journey_task(section, index, false, cx)
                }),
                cx.listener(move |this, _, _window, cx| {
                    this.move_journey_task(section, index, true, cx)
                }),
                cx.listener(move |this, _, _window, cx| {
                    this.remove_journey_task(section, index, cx)
                }),
            ))
            .into_any_element()
    }

    fn render_journey_score_menu(
        &self,
        section: usize,
        index: usize,
        current: u8,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
            for score in JOURNEY_SCORES.rev() {
                let this = this.clone();
                menu = menu.toggleable_entry(
                    score_label(score),
                    score == current,
                    IconPosition::Start,
                    None,
                    move |_window, cx| {
                        this.update(cx, |this, cx| {
                            this.set_journey_score(section, index, score, cx)
                        })
                        .log_err();
                    },
                );
            }
            menu
        });

        DropdownMenu::new(
            SharedString::from(format!("mermaid-journey-score-{section}-{index}")),
            SharedString::from(i18n::t!("Score {current}")),
            menu,
        )
        .style(DropdownStyle::Outlined)
        .trigger_size(ButtonSize::Compact)
        .into_any_element()
    }

    /// Everyone taking part, and the tool that renames one of them in every
    /// task at once.
    fn render_journey_people(
        &self,
        form: &JourneyForm,
        current: &JourneyDocument,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let people = current.people();
        let mut block = v_flex()
            .w_full()
            .gap_1()
            .child(section_title(i18n::t!("People")))
            .child(
                Label::new(if people.is_empty() {
                    i18n::t!("(None yet)").to_string()
                } else {
                    people.join(i18n::tc!("list separator", ", "))
                })
                .size(LabelSize::Small),
            );
        if people.is_empty() {
            return block.into_any_element();
        }

        let from = form
            .rename_from
            .clone()
            .filter(|person| people.contains(person))
            .or_else(|| people.first().cloned())
            .unwrap_or_default();
        let this = cx.entity().downgrade();
        let menu = ContextMenu::build(window, cx, {
            let from = from.clone();
            move |mut menu, _window, _cx| {
                for person in people.clone() {
                    let this = this.clone();
                    menu = menu.toggleable_entry(
                        person.clone(),
                        person == from,
                        IconPosition::Start,
                        None,
                        move |_window, cx| {
                            let person = person.clone();
                            this.update(cx, |this, cx| this.set_journey_rename_from(person, cx))
                                .log_err();
                        },
                    );
                }
                menu
            }
        });
        let to = field_text(&form.rename_to, cx);
        let can_rename = is_valid_journey_person(&to) && to != from;
        block = block.child(
            h_flex()
                .w_full()
                .gap_1()
                .child(
                    DropdownMenu::new("mermaid-journey-rename-from", from.clone(), menu)
                        .style(DropdownStyle::Outlined)
                        .trigger_size(ButtonSize::Compact),
                )
                .child(Label::new("→").size(LabelSize::Small).color(Color::Muted))
                .child(
                    field_frame(
                        &form.rename_to,
                        LABEL_FIELD_WIDTH,
                        to.is_empty() || is_valid_journey_person(&to),
                        window,
                        cx,
                    )
                    .flex_1()
                    .child(form.rename_to.clone()),
                )
                .child(
                    Button::new("mermaid-journey-rename", i18n::t!("Rename in All Tasks"))
                        .label_size(LabelSize::Small)
                        .disabled(!can_rename)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            // The menu shows the first person when the one
                            // picked earlier has since gone, so that is who
                            // gets renamed.
                            if let Some(form) = this.journey_mut() {
                                form.rename_from = Some(from.clone());
                            }
                            this.apply_journey_rename(window, cx)
                        })),
                ),
        );
        block.into_any_element()
    }

    pub(super) fn render_journey_toolbar(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> AnyElement {
        div().into_any_element()
    }
}

#[cfg(test)]
#[path = "journey_tests.rs"]
mod journey_tests;
