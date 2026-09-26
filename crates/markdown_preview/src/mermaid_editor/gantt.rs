//! The gantt chart half of the Mermaid editor's form.
//!
//! A gantt chart is a header and a list of sections, each holding the tasks
//! drawn in its band, so the form is one group per section with one block per
//! task in it. A task's start and end are each a menu and a field rather than
//! one text box, because the three ways Mermaid can spell a start — a date,
//! `after` another task, or nothing at all — are told apart by *counting* the
//! comma separated fields, and a form that let them be typed freely would turn
//! a typo into a different schedule.
//!
//! Everything a row refers to is another task's id, so the structural
//! operations go through the model: renaming an id carries into every `after`
//! and `until` that named it, and deleting a task follows those references one
//! step before thinning them out.

use editor::Editor;
use gpui::{AnyElement, App, Context, Entity, Window};
use mermaid_render::gantt_model::{
    DEFAULT_TASK_LENGTH, GanttDocument, GanttEnd, GanttSection, GanttStart, GanttTag, GanttTask,
    is_valid_gantt_date, is_valid_gantt_format, is_valid_gantt_id, is_valid_gantt_label,
    is_valid_gantt_section_name, is_valid_gantt_task_id, is_valid_gantt_title,
};
use ui::{ContextMenu, DropdownMenu, DropdownStyle, IconPosition, Tooltip, prelude::*};
use util::ResultExt as _;

use super::{
    Form, ID_FIELD_WIDTH, LABEL_FIELD_WIDTH, MermaidEditor, add_button, field_frame, neighbour,
    row_controls_labelled, section_title,
};

/// The label a task is added with, so that a new row is visible in the drawing
/// before anything has been typed into it.
const NEW_TASK_LABEL: &str = "新しいタスク";
const NEW_SECTION_NAME: &str = "新しいセクション";

const TASK_REMOVE_TOOLTIP: &str = "タスクを削除 (このタスクを after / until で参照しているタスクは、同じ日時の日付に置き換わります)";
const TAG_TOOLTIP: &str = "タスクの描き方: 完了 = done / 進行中 = active / 重要 = crit / マイルストーン = milestone / 縦線 = vert";
const START_TOOLTIP: &str = "開始の書き方: 日付 / after = 別のタスクが終わったら / 前のタスクに続く = 直前のタスクの終わりから";
const END_TOOLTIP: &str = "終了の書き方: 日付か長さ (5d = 5 日、2h = 2 時間、1w = 1 週間) / until = 別のタスクが始まるまで";

pub(super) struct GanttForm {
    pub(super) document: GanttDocument,
    pub(super) title: Entity<Editor>,
    pub(super) date_format: Entity<Editor>,
    pub(super) axis_format: Entity<Editor>,
    pub(super) sections: Vec<SectionRow>,
}

pub(super) struct SectionRow {
    pub(super) name: Entity<Editor>,
    pub(super) tasks: Vec<TaskRow>,
}

pub(super) struct TaskRow {
    pub(super) label: Entity<Editor>,
    pub(super) id: Entity<Editor>,
    /// The date, or the space separated ids of an `after`, depending on which
    /// [`GanttStart`] the task carries. Kept even while the start is implicit,
    /// so that switching the menu back does not lose what was typed.
    pub(super) start: Entity<Editor>,
    pub(super) end: Entity<Editor>,
}

/// How a task's start is written, which is what the menu beside the field
/// picks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StartMode {
    Date,
    After,
    Implicit,
}

impl StartMode {
    const ALL: [StartMode; 3] = [StartMode::Date, StartMode::After, StartMode::Implicit];

    fn label(self) -> &'static str {
        match self {
            Self::Date => "日付",
            Self::After => "after (別のタスクの後)",
            Self::Implicit => "前のタスクに続く",
        }
    }

    fn of(start: &GanttStart) -> Self {
        match start {
            GanttStart::Date(_) => Self::Date,
            GanttStart::After(_) => Self::After,
            GanttStart::Implicit => Self::Implicit,
        }
    }
}

/// How a task's end is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EndMode {
    Fixed,
    Until,
}

impl EndMode {
    const ALL: [EndMode; 2] = [EndMode::Fixed, EndMode::Until];

    fn label(self) -> &'static str {
        match self {
            Self::Fixed => "日付か長さ",
            Self::Until => "until (別のタスクの開始まで)",
        }
    }

    fn of(end: &GanttEnd) -> Self {
        match end {
            GanttEnd::Fixed(_) => Self::Fixed,
            GanttEnd::Until(_) => Self::Until,
        }
    }
}

impl GanttForm {
    pub(super) fn fields(&self) -> Vec<Entity<Editor>> {
        let header = [
            self.title.clone(),
            self.date_format.clone(),
            self.axis_format.clone(),
        ];
        header
            .into_iter()
            .chain(self.sections.iter().flat_map(|section| {
                std::iter::once(section.name.clone()).chain(section.tasks.iter().flat_map(|task| {
                    [
                        task.label.clone(),
                        task.id.clone(),
                        task.start.clone(),
                        task.end.clone(),
                    ]
                }))
            }))
            .collect()
    }

    /// The chart the form currently describes: the structure held in
    /// `document` with the text fields' current contents written over it.
    ///
    /// An id is what `after` and `until` address a task by, so renaming one in
    /// its field has to carry into every reference to it. Otherwise the old id
    /// would still be written out and Mermaid would quietly start the task
    /// today instead.
    pub(super) fn current_document(&self, cx: &App) -> GanttDocument {
        let mut document = self.document.clone();
        document.title = field_value(&self.title, cx);
        document.date_format = field_value(&self.date_format, cx);
        document.axis_format = field_value(&self.axis_format, cx);

        let mut renames: Vec<(String, String)> = Vec::new();
        for (section, row) in document.sections.iter_mut().zip(&self.sections) {
            section.name = field_value(&row.name, cx);
            for (task, task_row) in section.tasks.iter_mut().zip(&row.tasks) {
                task.label = task_row.label.read(cx).text(cx).trim().to_string();
                let id = field_value(&task_row.id, cx);
                if id != task.id {
                    if let (Some(old), Some(new)) = (&task.id, &id) {
                        renames.push((old.clone(), new.clone()));
                    }
                    task.id = id;
                }
                let start = task_row.start.read(cx).text(cx).trim().to_string();
                task.start = match &task.start {
                    GanttStart::Implicit => GanttStart::Implicit,
                    GanttStart::Date(_) => GanttStart::Date(start),
                    GanttStart::After(_) => GanttStart::After(split_ids(&start)),
                };
                let end = task_row.end.read(cx).text(cx).trim().to_string();
                task.end = match &task.end {
                    GanttEnd::Fixed(_) => GanttEnd::Fixed(end),
                    GanttEnd::Until(_) => GanttEnd::Until(split_ids(&end)),
                };
            }
        }

        document.rename_references(&renames);
        document
    }
}

/// The first thing wrong with what the user typed, in the words shown above the
/// form.
///
/// Every check a gantt chart needs is about how one row sits in the whole
/// document — an `after` that names a task which is not there, an id on a task
/// with no start of its own, a nameless section that would run into the one
/// above it — so they all live in `GanttDocument::structure_error`, which the
/// caller consults right after this. The red frames in the form come from the
/// `is_valid_gantt_*` predicates directly.
pub(super) fn problem(_document: &GanttDocument) -> Option<SharedString> {
    None
}

/// A field's text, or `None` when it is empty. Mermaid drops a `title`,
/// `dateFormat` or `section` statement with nothing after it, so an empty field
/// has to mean "no statement" rather than "a statement with an empty value".
fn field_value(field: &Entity<Editor>, cx: &App) -> Option<String> {
    let text = field.read(cx).text(cx).trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// What a task's tag menu says: the tags it carries, or the menu's name when
/// it carries none.
fn tag_menu_label(tags: &[GanttTag]) -> SharedString {
    if tags.is_empty() {
        return "タグ".into();
    }
    tags.iter()
        .map(|tag| tag.label())
        .collect::<Vec<_>>()
        .join("・")
        .into()
}

fn split_ids(text: &str) -> Vec<String> {
    text.split_whitespace().map(str::to_string).collect()
}

fn start_text(start: &GanttStart) -> String {
    match start {
        GanttStart::Date(date) => date.clone(),
        GanttStart::After(ids) => ids.join(" "),
        GanttStart::Implicit => String::new(),
    }
}

fn end_text(end: &GanttEnd) -> String {
    match end {
        GanttEnd::Fixed(text) => text.clone(),
        GanttEnd::Until(ids) => ids.join(" "),
    }
}

/// Whether the ids a reference names are all spellable and all defined. A field
/// pointing at a task that is not there gets the same red frame as one holding
/// a date Mermaid cannot read.
fn references_are_valid(ids: &[String], known: &[&str]) -> bool {
    !ids.is_empty()
        && ids
            .iter()
            .all(|id| is_valid_gantt_id(id) && known.contains(&id.as_str()))
}

fn start_is_valid(start: &GanttStart, known: &[&str]) -> bool {
    match start {
        GanttStart::Date(date) => is_valid_gantt_date(date),
        GanttStart::After(ids) => references_are_valid(ids, known),
        GanttStart::Implicit => true,
    }
}

fn end_is_valid(end: &GanttEnd, known: &[&str]) -> bool {
    match end {
        GanttEnd::Fixed(text) => is_valid_gantt_date(text),
        GanttEnd::Until(ids) => references_are_valid(ids, known),
    }
}

/// Whether the section at `index` may trade places with the one beside it.
///
/// The band written before the first `section` line is the one section Mermaid
/// lets go without a name, and only while it is the first one, so it is pinned
/// to the front: moving it down, or moving the section below it up, would leave
/// a nameless band in the middle, which Mermaid reads as part of the section
/// above it. The buttons and the handler ask this together, so a button that is
/// live can always be pressed.
fn section_may_move(sections: &[GanttSection], index: usize, forward: bool) -> bool {
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

impl MermaidEditor {
    pub(super) fn new_gantt_form(
        &mut self,
        document: GanttDocument,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> GanttForm {
        let title = self.new_field(
            document.title.clone().unwrap_or_default().as_str(),
            window,
            cx,
        );
        let date_format = self.new_field(
            document.date_format.clone().unwrap_or_default().as_str(),
            window,
            cx,
        );
        let axis_format = self.new_field(
            document.axis_format.clone().unwrap_or_default().as_str(),
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
                let (label, id) = (task.label.clone(), task.id.clone().unwrap_or_default());
                let (start, end) = (start_text(&task.start), end_text(&task.end));
                tasks.push(TaskRow {
                    label: self.new_field(&label, window, cx),
                    id: self.new_field(&id, window, cx),
                    start: self.new_field(&start, window, cx),
                    end: self.new_field(&end, window, cx),
                });
            }
            sections.push(SectionRow { name, tasks });
        }
        GanttForm {
            document,
            title,
            date_format,
            axis_format,
            sections,
        }
    }

    pub(super) fn gantt(&self) -> Option<&GanttForm> {
        match &self.form {
            Form::Gantt(form) => Some(form),
            _ => None,
        }
    }

    pub(super) fn gantt_mut(&mut self) -> Option<&mut GanttForm> {
        match &mut self.form {
            Form::Gantt(form) => Some(form),
            _ => None,
        }
    }

    /// Brings the document and the fields back into step before a structural
    /// change, in both directions.
    ///
    /// The rows only ever hold text, so which task an `after` points at — and
    /// therefore what deleting a task has to rewrite — is decided by the ids in
    /// those fields, which have to be read first. Writing back matters just as
    /// much: a rename carries into the references that named the task, and a
    /// reference field still showing the old id would put it straight back on
    /// the next read.
    fn sync_gantt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(document) = self.gantt().map(|form| form.current_document(cx)) else {
            return;
        };
        if let Some(form) = self.gantt_mut() {
            form.document = document;
        }
        self.refresh_gantt_reference_fields(window, cx);
    }

    /// Writes each task's start and end back into its fields, leaving an
    /// implicit start's field alone so that switching the menu away and back
    /// keeps what was typed there.
    fn refresh_gantt_reference_fields(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(updates) = self.gantt().map(|form| {
            form.document
                .sections
                .iter()
                .zip(&form.sections)
                .flat_map(|(section, row)| section.tasks.iter().zip(&row.tasks))
                .map(|(task, row)| {
                    let start = match &task.start {
                        GanttStart::Implicit => None,
                        start => Some(start_text(start)),
                    };
                    (
                        row.start.clone(),
                        start,
                        row.end.clone(),
                        end_text(&task.end),
                    )
                })
                .collect::<Vec<_>>()
        }) else {
            return;
        };
        for (start_field, start, end_field, end) in updates {
            if let Some(start) = start
                && start_field.read(cx).text(cx) != start
            {
                start_field.update(cx, |editor, cx| editor.set_text(start, window, cx));
            }
            if end_field.read(cx).text(cx) != end {
                end_field.update(cx, |editor, cx| editor.set_text(end, window, cx));
            }
        }
    }

    pub(super) fn add_gantt_section(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_gantt(window, cx);
        if self.gantt().is_none() {
            return;
        }
        let name = self.new_field(NEW_SECTION_NAME, window, cx);
        let Some(form) = self.gantt_mut() else {
            return;
        };
        form.document.sections.push(GanttSection {
            name: Some(NEW_SECTION_NAME.to_string()),
            tasks: Vec::new(),
            preserved_after: Vec::new(),
        });
        form.sections.push(SectionRow {
            name,
            tasks: Vec::new(),
        });
        self.schedule_preview(cx);
        cx.notify();
    }

    pub(super) fn remove_gantt_section(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sync_gantt(window, cx);
        let Some(form) = self.gantt_mut() else {
            return;
        };
        if index >= form.sections.len() {
            return;
        }
        match form.document.remove_section(index) {
            Ok(Some(_)) => {}
            Ok(None) => return,
            Err(error) => {
                self.refuse_gantt_change(error, cx);
                return;
            }
        }
        form.sections.remove(index);
        // The tasks that pointed into the section have been rewritten onto what
        // it resolved to, and their fields have to say so too.
        self.refresh_gantt_reference_fields(window, cx);
        self.schedule_preview(cx);
        cx.notify();
    }

    pub(super) fn move_gantt_section(
        &mut self,
        index: usize,
        forward: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sync_gantt(window, cx);
        let Some(form) = self.gantt_mut() else {
            return;
        };
        let Some(other) = neighbour(index, forward, form.sections.len()) else {
            return;
        };
        if other >= form.document.sections.len() || index >= form.document.sections.len() {
            return;
        }
        if !section_may_move(&form.document.sections, index, forward) {
            return;
        }
        form.document.sections.swap(index, other);
        form.sections.swap(index, other);
        self.schedule_preview(cx);
        cx.notify();
    }

    pub(super) fn add_gantt_task(
        &mut self,
        section: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sync_gantt(window, cx);
        let Some(form) = self.gantt() else {
            return;
        };
        // A chart with no `section` line at all still draws its tasks, in a
        // band of its own; the first task added to one opens that band.
        let needs_band = form.document.sections.is_empty() && section == 0;
        if !needs_band && section >= form.document.sections.len() {
            return;
        }
        // Mermaid resolves a start that is left out against the task before it,
        // and the first task in the chart has none.
        let preceded = form
            .document
            .sections
            .iter()
            .take(section + 1)
            .any(|section| !section.tasks.is_empty());
        let start = if preceded {
            GanttStart::Implicit
        } else {
            GanttStart::Date(String::new())
        };

        let band = needs_band.then(|| self.new_field("", window, cx));
        let label = self.new_field(NEW_TASK_LABEL, window, cx);
        let id = self.new_field("", window, cx);
        let start_field = self.new_field("", window, cx);
        let end = self.new_field(DEFAULT_TASK_LENGTH, window, cx);

        let Some(form) = self.gantt_mut() else {
            return;
        };
        if let Some(name) = band {
            form.document.sections.push(GanttSection {
                name: None,
                tasks: Vec::new(),
                preserved_after: Vec::new(),
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
        tasks.tasks.push(GanttTask {
            label: NEW_TASK_LABEL.to_string(),
            tags: Vec::new(),
            id: None,
            start,
            end: GanttEnd::Fixed(DEFAULT_TASK_LENGTH.to_string()),
            preserved_after: Vec::new(),
        });
        rows.tasks.push(TaskRow {
            label,
            id,
            start: start_field,
            end,
        });
        self.schedule_preview(cx);
        cx.notify();
    }

    pub(super) fn remove_gantt_task(
        &mut self,
        section: usize,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sync_gantt(window, cx);
        let Some(form) = self.gantt_mut() else {
            return;
        };
        let Some(rows) = form.sections.get_mut(section) else {
            return;
        };
        if index >= rows.tasks.len() {
            return;
        }
        match form.document.remove_task(section, index) {
            Ok(Some(_)) => {}
            Ok(None) => return,
            Err(error) => {
                self.refuse_gantt_change(error, cx);
                return;
            }
        }
        // Taken after the model call, so that a document the rows do not match
        // leaves both of them alone.
        if let Some(rows) = form.sections.get_mut(section) {
            rows.tasks.remove(index);
        }
        // `remove_task` rewrote every `after` and `until` that named the task
        // onto what it resolved to; a field still holding the old id would put
        // it back the next time the form is read.
        self.refresh_gantt_reference_fields(window, cx);
        self.schedule_preview(cx);
        cx.notify();
    }

    /// Shows why a deletion was refused. The preview is left alone, because
    /// nothing changed and redrawing it would clear the reason straight away.
    fn refuse_gantt_change(&mut self, error: anyhow::Error, cx: &mut Context<Self>) {
        self.message = Some(format!("{error}").into());
        cx.notify();
    }

    /// Moves a task within its section, or into the section next to it when it
    /// is already at the end it is moving towards.
    pub(super) fn move_gantt_task(
        &mut self,
        section: usize,
        index: usize,
        forward: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sync_gantt(window, cx);
        let Some(form) = self.gantt_mut() else {
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
            let Some(task) = form
                .document
                .sections
                .get_mut(section)
                .map(|tasks| tasks.tasks.remove(index))
            else {
                return;
            };
            let row = form
                .sections
                .get_mut(section)
                .map(|rows| rows.tasks.remove(index));
            let Some(row) = row else {
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
        self.schedule_preview(cx);
        cx.notify();
    }

    pub(super) fn toggle_gantt_tag(
        &mut self,
        section: usize,
        index: usize,
        tag: GanttTag,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sync_gantt(window, cx);
        let Some(form) = self.gantt_mut() else {
            return;
        };
        let Some(task) = form
            .document
            .sections
            .get_mut(section)
            .and_then(|section| section.tasks.get_mut(index))
        else {
            return;
        };
        if task.tags.contains(&tag) {
            task.tags.retain(|other| *other != tag);
        } else {
            task.tags.push(tag);
        }
        self.schedule_preview(cx);
        cx.notify();
    }

    pub(super) fn set_gantt_start_mode(
        &mut self,
        section: usize,
        index: usize,
        mode: StartMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sync_gantt(window, cx);
        let Some(form) = self.gantt() else {
            return;
        };
        // Read from the field rather than from the document, so that switching
        // to "前のタスクに続く" and back keeps the date that was typed.
        let Some(text) = form
            .sections
            .get(section)
            .and_then(|section| section.tasks.get(index))
            .map(|row| row.start.read(cx).text(cx).trim().to_string())
        else {
            return;
        };
        let start = match mode {
            StartMode::Date => GanttStart::Date(text),
            StartMode::After => GanttStart::After(split_ids(&text)),
            StartMode::Implicit => GanttStart::Implicit,
        };
        let Some(form) = self.gantt_mut() else {
            return;
        };
        if let Some(task) = form
            .document
            .sections
            .get_mut(section)
            .and_then(|section| section.tasks.get_mut(index))
        {
            task.start = start;
        }
        self.schedule_preview(cx);
        cx.notify();
    }

    pub(super) fn set_gantt_end_mode(
        &mut self,
        section: usize,
        index: usize,
        mode: EndMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sync_gantt(window, cx);
        let Some(form) = self.gantt() else {
            return;
        };
        let Some(text) = form
            .sections
            .get(section)
            .and_then(|section| section.tasks.get(index))
            .map(|row| row.end.read(cx).text(cx).trim().to_string())
        else {
            return;
        };
        let end = match mode {
            EndMode::Fixed => GanttEnd::Fixed(text),
            EndMode::Until => GanttEnd::Until(split_ids(&text)),
        };
        let Some(form) = self.gantt_mut() else {
            return;
        };
        if let Some(task) = form
            .document
            .sections
            .get_mut(section)
            .and_then(|section| section.tasks.get_mut(index))
        {
            task.end = end;
        }
        self.schedule_preview(cx);
        cx.notify();
    }

    pub(super) fn render_gantt(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(form) = self.gantt() else {
            return Vec::new();
        };
        let current = form.current_document(cx);
        vec![
            self.render_gantt_header(form, &current, window, cx),
            self.render_gantt_sections(form, &current, window, cx),
        ]
    }

    fn render_gantt_header(
        &self,
        form: &GanttForm,
        current: &GanttDocument,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let formats = [
            ("日付書式", &form.date_format, &current.date_format),
            ("軸の書式", &form.axis_format, &current.axis_format),
        ];
        let mut row = h_flex().w_full().gap_2();
        for (label, field, value) in formats {
            row = row.child(section_title(label)).child(
                field_frame(
                    field,
                    ID_FIELD_WIDTH,
                    value
                        .as_ref()
                        .is_none_or(|value| is_valid_gantt_format(value)),
                    window,
                    cx,
                )
                .child(field.clone()),
            );
        }

        v_flex()
            .w_full()
            .gap_1()
            .child(
                h_flex()
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
                                .is_none_or(|title| is_valid_gantt_title(title)),
                            window,
                            cx,
                        )
                        .flex_1()
                        .child(form.title.clone()),
                    ),
            )
            .child(row)
            .into_any_element()
    }

    fn render_gantt_sections(
        &self,
        form: &GanttForm,
        current: &GanttDocument,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let known = current.task_ids();
        let mut list = v_flex()
            .w_full()
            .gap_2()
            .child(section_title("セクションとタスク"));

        for index in 0..form.sections.len() {
            let (Some(row), Some(section)) =
                (form.sections.get(index), current.sections.get(index))
            else {
                continue;
            };
            let mut tasks = v_flex().w_full().gap_2().pl_4();
            for task_index in 0..row.tasks.len() {
                let (Some(task_row), Some(task)) =
                    (row.tasks.get(task_index), section.tasks.get(task_index))
                else {
                    continue;
                };
                tasks = tasks.child(self.render_gantt_task(
                    index,
                    task_index,
                    row.tasks.len(),
                    task_row,
                    task,
                    &known,
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
                            .child(section_title("セクション"))
                            .child(
                                field_frame(
                                    &row.name,
                                    LABEL_FIELD_WIDTH,
                                    // Only the band before the first `section`
                                    // line may go without a name.
                                    section.name.as_ref().map_or(index == 0, |name| {
                                        is_valid_gantt_section_name(name)
                                    }),
                                    window,
                                    cx,
                                )
                                .flex_1()
                                .child(row.name.clone()),
                            )
                            .child(row_controls_labelled(
                                "mermaid-gantt-section",
                                index,
                                !section_may_move(&current.sections, index, false),
                                !section_may_move(&current.sections, index, true),
                                "セクションとその中のタスクを削除",
                                cx.listener(move |this, _, window, cx| {
                                    this.move_gantt_section(index, false, window, cx)
                                }),
                                cx.listener(move |this, _, window, cx| {
                                    this.move_gantt_section(index, true, window, cx)
                                }),
                                cx.listener(move |this, _, window, cx| {
                                    this.remove_gantt_section(index, window, cx)
                                }),
                            )),
                    )
                    .child(tasks)
                    .child(div().pl_4().child(add_button(
                        SharedString::from(format!("mermaid-gantt-add-task-{index}")),
                        "タスクを追加",
                        cx.listener(move |this, _, window, cx| {
                            this.add_gantt_task(index, window, cx)
                        }),
                    ))),
            );
        }

        if form.sections.is_empty() {
            list = list.child(add_button(
                "mermaid-gantt-add-first-task",
                "タスクを追加",
                cx.listener(|this, _, window, cx| this.add_gantt_task(0, window, cx)),
            ));
        }

        list.child(add_button(
            "mermaid-gantt-add-section",
            "セクションを追加",
            cx.listener(|this, _, window, cx| this.add_gantt_section(window, cx)),
        ))
        .into_any_element()
    }

    /// One task in two rows: what it is (name, id, tags) above, when it runs
    /// (start and end) below. The tags are a menu rather than five checkboxes,
    /// which would take a third row of their own.
    #[allow(clippy::too_many_arguments)]
    fn render_gantt_task(
        &self,
        section: usize,
        index: usize,
        length: usize,
        row: &TaskRow,
        task: &GanttTask,
        known: &[&str],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let start_field = (task.start != GanttStart::Implicit).then(|| {
            field_frame(
                &row.start,
                ID_FIELD_WIDTH,
                start_is_valid(&task.start, known),
                window,
                cx,
            )
            .child(row.start.clone())
        });

        v_flex()
            .w_full()
            .gap_1()
            .child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .child(
                        field_frame(
                            &row.label,
                            LABEL_FIELD_WIDTH,
                            is_valid_gantt_label(&task.label),
                            window,
                            cx,
                        )
                        .flex_1()
                        .child(row.label.clone()),
                    )
                    .child(section_title("ID"))
                    .child(
                        field_frame(
                            &row.id,
                            ID_FIELD_WIDTH,
                            // A task may be called anything it can be spelled
                            // with; whether an `after` can name it is the
                            // reference field's business, not this one's.
                            task.id.as_ref().is_none_or(|id| is_valid_gantt_task_id(id)),
                            window,
                            cx,
                        )
                        .child(row.id.clone()),
                    )
                    .child(self.render_gantt_tag_menu(section, index, task, window, cx))
                    .child(row_controls_labelled(
                        &format!("mermaid-gantt-task-{section}"),
                        index,
                        index == 0 && section == 0,
                        index + 1 >= length && section + 1 >= self.gantt_section_count(),
                        TASK_REMOVE_TOOLTIP,
                        cx.listener(move |this, _, window, cx| {
                            this.move_gantt_task(section, index, false, window, cx)
                        }),
                        cx.listener(move |this, _, window, cx| {
                            this.move_gantt_task(section, index, true, window, cx)
                        }),
                        cx.listener(move |this, _, window, cx| {
                            this.remove_gantt_task(section, index, window, cx)
                        }),
                    )),
            )
            .child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .child(section_title("開始"))
                    .child(self.render_gantt_start_menu(section, index, task, window, cx))
                    .children(start_field)
                    .child(section_title("終了"))
                    .child(self.render_gantt_end_menu(section, index, task, window, cx))
                    .child(
                        field_frame(
                            &row.end,
                            ID_FIELD_WIDTH,
                            end_is_valid(&task.end, known),
                            window,
                            cx,
                        )
                        .child(row.end.clone()),
                    ),
            )
            .into_any_element()
    }

    fn render_gantt_tag_menu(
        &self,
        section: usize,
        index: usize,
        task: &GanttTask,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        let assigned = task.tags.clone();
        let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
            for tag in GanttTag::ALL {
                let this = this.clone();
                menu = menu.toggleable_entry(
                    tag.label(),
                    assigned.contains(&tag),
                    IconPosition::Start,
                    None,
                    move |window, cx| {
                        this.update(cx, |this, cx| {
                            this.toggle_gantt_tag(section, index, tag, window, cx);
                        })
                        .log_err();
                    },
                );
            }
            menu
        });

        DropdownMenu::new(
            SharedString::from(format!("mermaid-gantt-tags-{section}-{index}")),
            tag_menu_label(&task.tags),
            menu,
        )
        .style(DropdownStyle::Outlined)
        .trigger_size(ButtonSize::Compact)
        .trigger_tooltip(Tooltip::text(TAG_TOOLTIP))
        .into_any_element()
    }

    fn gantt_section_count(&self) -> usize {
        self.gantt().map_or(0, |form| form.sections.len())
    }

    fn render_gantt_start_menu(
        &self,
        section: usize,
        index: usize,
        task: &GanttTask,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let current = StartMode::of(&task.start);
        let this = cx.entity().downgrade();
        let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
            for mode in StartMode::ALL {
                let this = this.clone();
                menu = menu.toggleable_entry(
                    mode.label(),
                    mode == current,
                    IconPosition::Start,
                    None,
                    move |window, cx| {
                        this.update(cx, |this, cx| {
                            this.set_gantt_start_mode(section, index, mode, window, cx);
                        })
                        .log_err();
                    },
                );
            }
            menu
        });

        DropdownMenu::new(
            SharedString::from(format!("mermaid-gantt-start-{section}-{index}")),
            current.label(),
            menu,
        )
        .style(DropdownStyle::Outlined)
        .trigger_size(ButtonSize::Compact)
        .trigger_tooltip(Tooltip::text(START_TOOLTIP))
        .into_any_element()
    }

    fn render_gantt_end_menu(
        &self,
        section: usize,
        index: usize,
        task: &GanttTask,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let current = EndMode::of(&task.end);
        let this = cx.entity().downgrade();
        let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
            for mode in EndMode::ALL {
                let this = this.clone();
                menu = menu.toggleable_entry(
                    mode.label(),
                    mode == current,
                    IconPosition::Start,
                    None,
                    move |window, cx| {
                        this.update(cx, |this, cx| {
                            this.set_gantt_end_mode(section, index, mode, window, cx);
                        })
                        .log_err();
                    },
                );
            }
            menu
        });

        DropdownMenu::new(
            SharedString::from(format!("mermaid-gantt-end-{section}-{index}")),
            current.label(),
            menu,
        )
        .style(DropdownStyle::Outlined)
        .trigger_size(ButtonSize::Compact)
        .trigger_tooltip(Tooltip::text(END_TOOLTIP))
        .into_any_element()
    }
}

#[cfg(test)]
#[path = "gantt_tests.rs"]
mod gantt_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use editor::test::editor_test_context::EditorTestContext;
    use gpui::TestAppContext;
    use indoc::indoc;
    use mermaid_render::gantt_model::{emit_gantt, parse_gantt};
    use mermaid_render::mermaid_model::parse_document;
    use std::sync::Arc;

    fn document(source: &str) -> GanttDocument {
        parse_gantt(source).unwrap_or_else(|error| panic!("{source} should parse: {error}"))
    }

    #[test]
    fn a_start_and_an_end_round_trip_through_their_field_text() {
        let document = document(
            "gantt\n    dateFormat YYYY-MM-DD\n    A :a, 2026-01-01, 7d\n    B :b, after a, until a\n    C :2d",
        );
        let starts = document
            .tasks()
            .map(|task| (StartMode::of(&task.start), start_text(&task.start)))
            .collect::<Vec<_>>();
        assert_eq!(
            starts,
            vec![
                (StartMode::Date, "2026-01-01".to_string()),
                (StartMode::After, "a".to_string()),
                (StartMode::Implicit, String::new()),
            ]
        );
        let ends = document
            .tasks()
            .map(|task| (EndMode::of(&task.end), end_text(&task.end)))
            .collect::<Vec<_>>();
        assert_eq!(
            ends,
            vec![
                (EndMode::Fixed, "7d".to_string()),
                (EndMode::Until, "a".to_string()),
                (EndMode::Fixed, "2d".to_string()),
            ]
        );
        assert_eq!(split_ids(" a   b "), vec!["a".to_string(), "b".to_string()]);
        assert!(split_ids("  ").is_empty());
    }

    #[test]
    fn a_reference_is_only_valid_while_the_task_it_names_is_there() {
        let known = ["a", "b"];
        assert!(references_are_valid(&["a".to_string()], &known));
        assert!(references_are_valid(
            &["a".to_string(), "b".to_string()],
            &known
        ));
        assert!(!references_are_valid(&[], &known));
        assert!(!references_are_valid(&["c".to_string()], &known));
        assert!(!references_are_valid(&["タスク".to_string()], &known));

        assert!(start_is_valid(&GanttStart::Implicit, &[]));
        assert!(start_is_valid(
            &GanttStart::Date("2026-01-01".to_string()),
            &[]
        ));
        assert!(!start_is_valid(&GanttStart::Date(String::new()), &[]));
        assert!(!start_is_valid(
            &GanttStart::After(vec!["c".to_string()]),
            &known
        ));
        assert!(end_is_valid(&GanttEnd::Fixed("5d".to_string()), &[]));
        assert!(!end_is_valid(
            &GanttEnd::Until(vec!["c".to_string()]),
            &known
        ));
    }

    /// Deleting a task from the form goes through the model, which keeps the
    /// task after it pointing at something that is still there.
    #[test]
    fn the_form_deletes_through_the_model() {
        let mut chart = document(
            "gantt\n    dateFormat YYYY-MM-DD\n    section 前半\n    A :a, 2026-01-01, 3d\n    B :b, after a, 2026-02-01\n    section 後半\n    C :c, after b, 5d",
        );
        chart
            .remove_task(0, 1)
            .expect("the deletion should be allowed")
            .expect("the task should be removed");
        assert_eq!(
            emit_gantt(&chart),
            "gantt\n    dateFormat YYYY-MM-DD\n    section 前半\n    A :a, 2026-01-01, 3d\n    section 後半\n    C :c, 2026-02-01, 5d"
        );
        assert_eq!(chart.structure_error(), None);
    }

    fn init_test(cx: &mut TestAppContext) -> Arc<workspace::AppState> {
        cx.update(|cx| {
            let state = workspace::AppState::test(cx);
            editor::init(cx);
            crate::init(cx);
            state
        })
    }

    /// An editor holding `state` with the modal opened on the fence the cursor
    /// marker sits in.
    async fn open_on(
        state: &str,
        cx: &mut TestAppContext,
    ) -> (
        EditorTestContext,
        Entity<MermaidEditor>,
        Arc<workspace::AppState>,
    ) {
        let app_state = init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.update_buffer(|buffer, cx| {
            buffer.set_language(
                Some(Arc::new(language::Language::new(
                    language::LanguageConfig {
                        name: "Markdown".into(),
                        ..Default::default()
                    },
                    None,
                ))),
                cx,
            )
        });
        cx.set_state(state);

        let editor = cx.editor.clone();
        let target = cx
            .update_editor(|editor, _window, cx| editor.mermaid_fence_target(cx))
            .expect("the cursor is inside a mermaid fence");
        let document = parse_document(&target.source)
            .unwrap_or_else(|error| panic!("the fence should open: {error}"));
        let modal = cx.update(|window, cx| {
            cx.new(|cx| MermaidEditor::new(editor.downgrade(), target, Some(document), window, cx))
        });
        (cx, modal, app_state)
    }

    fn task_row(modal: &MermaidEditor, section: usize, index: usize) -> &TaskRow {
        modal
            .gantt()
            .and_then(|form| form.sections.get(section))
            .and_then(|section| section.tasks.get(index))
            .unwrap_or_else(|| panic!("task {section}/{index} should have a row"))
    }

    const TEMPLATE_FENCE: &str = indoc! {"
        ```mermaid
        ganttˇ
            title 開発スケジュール
            dateFormat YYYY-MM-DD
            axisFormat %m/%d
            section 設計
            要件定義 :done, req, 2026-01-06, 5d
            基本設計 :active, design, after req, 7d
            section 実装
            実装 :impl, after design, 10d
        ```
    "};

    /// The template `markdown::InsertMermaidDiagram` writes is read from the
    /// action rather than copied here, so that a template which drifts away
    /// from what the model emits turns this red instead of turning every
    /// inserted chart into a rewrite the first time it is confirmed.
    #[test]
    fn the_insert_template_is_the_source_the_model_writes_back() {
        let template = editor::actions::MermaidDiagramKind::Gantt.template();
        let chart = parse_gantt(template)
            .unwrap_or_else(|error| panic!("the insert template should open: {error}"));
        assert_eq!(emit_gantt(&chart), template);
        assert!(
            TEMPLATE_FENCE.replace('\u{2c7}', "").contains(template),
            "the fence these tests open should hold the insert template"
        );
    }

    /// The fence the insert template writes has to survive being opened and
    /// confirmed without a single byte moving.
    #[gpui::test]
    async fn confirming_the_template_unchanged_leaves_the_fence_alone(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;
        let before = cx.update_editor(|editor, _window, cx| editor.text(cx));

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                modal.confirm(&menu::SecondaryConfirm, window, cx)
            })
        });

        assert_eq!(
            cx.update_editor(|editor, _window, cx| editor.text(cx)),
            before,
            "an unedited gantt chart must be written back exactly as it was"
        );
    }

    /// An id is what `after` addresses a task by, so renaming one in its field
    /// has to reach the tasks that named it.
    #[gpui::test]
    async fn renaming_an_id_follows_into_every_after(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                let id = task_row(modal, 0, 0).id.clone();
                id.update(cx, |id, cx| id.set_text("requirement", window, cx));
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        let text = cx.update_editor(|editor, _window, cx| editor.text(cx));
        assert!(
            text.contains("要件定義 :done, requirement, 2026-01-06, 5d"),
            "the id should be renamed:\n{text}"
        );
        assert!(
            text.contains("基本設計 :active, design, after requirement, 7d"),
            "the reference should follow the rename:\n{text}"
        );
        assert!(!text.contains("after req,"), "no stale reference:\n{text}");
    }

    /// Deleting a task has to work off the dates the fields hold, not the ones
    /// the document was opened with: `after` resolves to where the removed task
    /// ended, and an edit to that end is what the task after it inherits.
    #[gpui::test]
    async fn deleting_a_task_inherits_the_end_as_it_is_now_typed(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                let end = task_row(modal, 0, 0).end.clone();
                end.update(cx, |end, cx| end.set_text("2026-01-20", window, cx));
                modal.remove_gantt_task(0, 0, window, cx);
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        let text = cx.update_editor(|editor, _window, cx| editor.text(cx));
        assert!(
            text.contains("基本設計 :active, design, 2026-01-20, 7d"),
            "the task after the deleted one should start where it now ends:\n{text}"
        );
        assert!(
            !text.contains("要件定義"),
            "the task should be gone:\n{text}"
        );
        assert!(
            text.contains("実装 :impl, after design, 10d"),
            "the untouched task should keep its reference:\n{text}"
        );
    }

    /// A chart with no `section` line still draws its tasks, and the first task
    /// of the whole chart has nothing to follow, so it is asked for a date.
    #[gpui::test]
    async fn the_first_task_of_a_blank_chart_is_asked_for_a_start(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                ganttˇ
                ```
            "},
            cx,
        )
        .await;

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                modal.add_gantt_task(0, window, cx);
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });
        let message = cx.update(|_window, cx| modal.read(cx).message.clone());
        assert_eq!(
            message.as_deref(),
            Some("タスク `新しいタスク` の開始日を入力してください。"),
            "a task with nowhere to start must not be written back"
        );

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                let row = task_row(modal, 0, 0);
                let (label, start) = (row.label.clone(), row.start.clone());
                label.update(cx, |label, cx| label.set_text("調査", window, cx));
                start.update(cx, |start, cx| start.set_text("2026-01-06", window, cx));
                modal.toggle_gantt_tag(0, 0, GanttTag::Crit, window, cx);
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        assert_eq!(
            cx.update_editor(|editor, _window, cx| editor.text(cx)),
            indoc! {"
                ```mermaid
                gantt
                    調査 :crit, 2026-01-06, 1d
                ```
            "},
        );
    }

    /// The menu beside the start field is what picks between the three ways
    /// Mermaid can spell a start, and switching it must not lose the text.
    #[gpui::test]
    async fn switching_the_start_menu_keeps_what_was_typed(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                modal.set_gantt_start_mode(0, 1, StartMode::Date, window, cx);
                let start = modal
                    .gantt()
                    .and_then(|form| form.document.tasks().nth(1).cloned())
                    .expect("the second task");
                assert_eq!(start.start, GanttStart::Date("req".to_string()));
                modal.set_gantt_start_mode(0, 1, StartMode::Implicit, window, cx);
                modal.set_gantt_start_mode(0, 1, StartMode::After, window, cx);
                let back = modal
                    .gantt()
                    .and_then(|form| form.document.tasks().nth(1).cloned())
                    .expect("the second task");
                assert_eq!(
                    back.start,
                    GanttStart::After(vec!["req".to_string()]),
                    "the text has to survive the trip through the implicit start"
                );

                modal.set_gantt_end_mode(0, 1, EndMode::Until, window, cx);
                let end = modal
                    .gantt()
                    .and_then(|form| form.document.tasks().nth(1).cloned())
                    .expect("the second task");
                assert_eq!(end.end, GanttEnd::Until(vec!["7d".to_string()]));
            })
        });
    }

    /// A rename has to reach the `after` fields themselves, not just the
    /// document: the next thing the form does is read those fields back.
    #[gpui::test]
    async fn a_rename_survives_the_next_structural_change(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                let id = task_row(modal, 0, 0).id.clone();
                id.update(cx, |id, cx| id.set_text("requirement", window, cx));
                modal.add_gantt_section(window, cx);
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        let text = cx.update_editor(|editor, _window, cx| editor.text(cx));
        assert!(
            text.contains("基本設計 :active, design, after requirement, 7d"),
            "the reference should still follow the rename:\n{text}"
        );
        assert!(
            text.contains("section 新しいセクション"),
            "the new section should be written:\n{text}"
        );
    }

    /// Two tasks spelled with the same id cannot be told apart by `after`, so a
    /// rename that lands on one another task already carries is stopped rather
    /// than written back as a chart whose references moved.
    #[gpui::test]
    async fn a_rename_onto_an_id_already_in_use_is_refused(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;
        let before = cx.update_editor(|editor, _window, cx| editor.text(cx));

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                let id = task_row(modal, 0, 1).id.clone();
                id.update(cx, |id, cx| id.set_text("req", window, cx));
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        let message = cx.update(|_window, cx| modal.read(cx).message.clone());
        assert!(
            message
                .as_deref()
                .is_some_and(|message| message.contains("`req`")),
            "the clash should be named: {message:?}"
        );
        assert_eq!(
            cx.update_editor(|editor, _window, cx| editor.text(cx)),
            before,
            "nothing may be written while two tasks share an id"
        );
    }

    /// The one rule the section arrows carry, asked of the same predicate the
    /// handler asks: a band with no name only reads as a band of its own while
    /// it is the first section, so it cannot be swapped away from the front.
    #[test]
    fn the_band_with_no_name_may_not_leave_the_front() {
        let band = GanttSection {
            name: None,
            tasks: Vec::new(),
            preserved_after: Vec::new(),
        };
        let named = |name: &str| GanttSection {
            name: Some(name.to_string()),
            tasks: Vec::new(),
            preserved_after: Vec::new(),
        };

        let pinned = vec![band, named("後半"), named("最後")];
        assert!(!section_may_move(&pinned, 0, true), "the band may not sink");
        assert!(
            !section_may_move(&pinned, 1, false),
            "and nothing may rise past it"
        );
        assert!(section_may_move(&pinned, 1, true));
        assert!(section_may_move(&pinned, 2, false));
        assert!(!section_may_move(&pinned, 2, true));

        // With every section named there is nothing to pin.
        let named_only = vec![named("前半"), named("後半")];
        assert!(section_may_move(&named_only, 0, true));
        assert!(section_may_move(&named_only, 1, false));
        assert!(!section_may_move(&named_only, 0, false));
        assert!(!section_may_move(&named_only, 1, true));

        assert!(!section_may_move(&[], 0, true));
        assert!(!section_may_move(&named_only, 9, false));
    }

    /// The tasks written before the first `section` line live in a band that has
    /// no name, which Mermaid only reads as a band of its own while it is the
    /// first one, so it may not trade places with the section below it.
    #[gpui::test]
    async fn the_nameless_band_stays_in_front(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                ganttˇ
                    dateFormat YYYY-MM-DD
                    最初 :first, 2026-01-01, 3d
                    section 後半
                    仕上げ :after first, 1w
                ```
            "},
            cx,
        )
        .await;

        let order = |modal: &MermaidEditor| {
            modal.gantt().map(|form| {
                form.document
                    .sections
                    .iter()
                    .map(|section| section.name.clone())
                    .collect::<Vec<_>>()
            })
        };
        let before = cx.update(|_window, cx| order(modal.read(cx)));

        // Checked one move at a time: run together, a swap and its opposite
        // would put the order back and the test would pass either way.
        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                modal.move_gantt_section(0, true, window, cx);
            })
        });
        assert_eq!(
            cx.update(|_window, cx| order(modal.read(cx))),
            before,
            "the band may not be pushed below the section under it"
        );

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                modal.move_gantt_section(1, false, window, cx);
            })
        });
        assert_eq!(
            cx.update(|_window, cx| order(modal.read(cx))),
            before,
            "the section under the band may not be pulled above it"
        );

        // Naming it makes it a section like any other, and then it moves.
        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                let name = modal
                    .gantt()
                    .and_then(|form| form.sections.first())
                    .map(|section| section.name.clone())
                    .expect("the band should have a name field");
                name.update(cx, |name, cx| name.set_text("前半", window, cx));
                modal.move_gantt_section(0, true, window, cx);
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        assert_eq!(
            cx.update_editor(|editor, _window, cx| editor.text(cx)),
            indoc! {"
                ```mermaid
                gantt
                    dateFormat YYYY-MM-DD
                    section 後半
                    仕上げ :after first, 1w
                    section 前半
                    最初 :first, 2026-01-01, 3d
                ```
            "},
        );
    }

    /// Moving a task past the end of its section carries it into the next one,
    /// rows and document together.
    #[gpui::test]
    async fn a_task_moves_between_sections(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                modal.move_gantt_task(0, 1, true, window, cx);
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        assert_eq!(
            cx.update_editor(|editor, _window, cx| editor.text(cx)),
            indoc! {"
                ```mermaid
                gantt
                    title 開発スケジュール
                    dateFormat YYYY-MM-DD
                    axisFormat %m/%d
                    section 設計
                    要件定義 :done, req, 2026-01-06, 5d
                    section 実装
                    基本設計 :active, design, after req, 7d
                    実装 :impl, after design, 10d
                ```
            "},
        );
    }
}
