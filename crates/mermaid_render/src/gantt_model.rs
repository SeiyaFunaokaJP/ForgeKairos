//! A round-trippable editing model for Mermaid gantt charts.
//!
//! The shape of this module mirrors [`crate::flowchart_model`]: [`parse_gantt`]
//! reads the body of a ```` ```mermaid ```` fence into [`GanttDocument`], and
//! [`emit_gantt`] writes it back. See [`crate::mermaid_model`] for the
//! invariants every diagram kind keeps.
//!
//! Three decisions are specific to gantt charts:
//!
//! - `merman`'s gantt render model resolves every task to a pair of
//!   milliseconds, and the specification the user wrote (`after req`, `5d`,
//!   `2026-01-06`) lives in fields that are private to it. Nothing can be
//!   written back from that model, so the source is scanned here and `merman`
//!   is only asked whether the fence is a gantt chart it can draw at all.
//! - A date and a duration are kept as the text the source spells them with, in
//!   the way [`crate::pie_model`] keeps a slice value. Which of the two a field
//!   holds cannot be told without applying the chart's own `dateFormat`, and
//!   re-spelling either of them would rewrite a fence the user only looked at.
//! - The statements this form does not show — `excludes`, `includes`,
//!   `todayMarker`, `tickInterval`, `weekday`, `weekend`, `topAxis`,
//!   `inclusiveEndDates`, `click`, comments and the accessibility statements —
//!   are held verbatim. The ones written before the first section or task are
//!   [`PreservedLine`]s; every later one belongs to the section header or the
//!   task right above it, moves with it, and falls back onto whatever came
//!   before it when that element is deleted. Everything else that is not a
//!   task, a section, a title or a date format makes [`parse_gantt`] fail with
//!   a reason rather than being dropped.
//!
//! The one place where the fence is read differently from the rest of Mermaid
//! is the task line: its fields are told apart by *counting* them, not by what
//! they look like (`a, b, c` is id, start, end; `b, c` is start, end; `c` is an
//! end alone). A spec with four fields or with nothing but tags is one Mermaid
//! itself cannot resolve, so it is refused rather than opened.
//!
//! What the parser rewrites, all of it stable from the first emit: indentation,
//! the case of a keyword, the whitespace around a statement's argument, the
//! order of the `title` / `dateFormat` / `axisFormat` statements, a repeated
//! tag on one task, and the runs of spaces inside an `after` / `until` list.

use std::fmt::Write as _;

use anyhow::{Result, anyhow, bail};
use merman::diagram::RenderSemanticModel;

use crate::flowchart_model::{PreservedLine, PreservedPosition};
use crate::mermaid_model::{INDENT, first_statement_line, join_lines, keyword_tail, scan_preamble};

mod gantt_dates;

/// The length a new task is given.
pub const DEFAULT_TASK_LENGTH: &str = "1d";

/// A tag that changes how a task is drawn. Tags come before a task's other
/// fields, and Mermaid only reads them there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GanttTag {
    Done,
    Active,
    Crit,
    Milestone,
    Vert,
}

impl GanttTag {
    pub const ALL: [GanttTag; 5] = [
        GanttTag::Done,
        GanttTag::Active,
        GanttTag::Crit,
        GanttTag::Milestone,
        GanttTag::Vert,
    ];

    /// The word the fence spells the tag with. Mermaid matches it exactly, so
    /// `Done` is an id rather than a tag.
    pub fn keyword(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Active => "active",
            Self::Crit => "crit",
            Self::Milestone => "milestone",
            Self::Vert => "vert",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Done => i18n::tc!("Gantt task tag", "Done"),
            Self::Active => i18n::tc!("Gantt task tag", "In progress"),
            Self::Crit => i18n::tc!("Gantt task tag", "Critical"),
            Self::Milestone => i18n::tc!("Gantt task tag", "Milestone"),
            Self::Vert => i18n::tc!("Gantt task tag", "Vertical line"),
        }
    }

    fn from_keyword(word: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|tag| tag.keyword() == word)
    }
}

/// When a task starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GanttStart {
    /// A date, as the source spells it. Mermaid reads it with the chart's
    /// `dateFormat`, which this model does not interpret.
    Date(String),
    /// `after a b`: the task starts when the last of those tasks ends.
    After(Vec<String>),
    /// No start field at all, which starts the task where the one before it in
    /// the fence ended. Only a task without an id can be written this way,
    /// because Mermaid tells the fields apart by counting them.
    Implicit,
}

/// When a task ends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GanttEnd {
    /// A date or a duration, as the source spells it (`2026-01-10`, `5d`).
    /// Mermaid tries the chart's `dateFormat` first and falls back to reading
    /// the text as a duration, and which of the two it is cannot be decided
    /// without that format, so both share one variant.
    Fixed(String),
    /// `until a b`: the task ends when the first of those tasks starts.
    Until(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GanttTask {
    /// The text before the `:`, without the whitespace around it.
    pub label: String,
    pub tags: Vec<GanttTag>,
    /// The name `after` and `until` address this task by.
    pub id: Option<String>,
    pub start: GanttStart,
    pub end: GanttEnd,
    /// The lines this form does not show that were written right below the
    /// task, verbatim and in order. They travel with the task when it moves.
    pub preserved_after: Vec<String>,
}

impl GanttTask {
    /// The ids this task's start and end point at.
    pub fn references(&self) -> Vec<&str> {
        let start = match &self.start {
            GanttStart::After(ids) => ids.as_slice(),
            GanttStart::Date(_) | GanttStart::Implicit => &[],
        };
        let end = match &self.end {
            GanttEnd::Until(ids) => ids.as_slice(),
            GanttEnd::Fixed(_) => &[],
        };
        start
            .iter()
            .chain(end)
            .map(String::as_str)
            .collect::<Vec<_>>()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GanttSection {
    /// `None` for the tasks written before the first `section` line, which
    /// Mermaid draws in a band of their own. Only the first section can be
    /// nameless: a later one would run into the section above it.
    pub name: Option<String>,
    pub tasks: Vec<GanttTask>,
    /// The lines written between the `section` line and the first task in it.
    /// Always empty for the nameless band, whose leading lines are the chart's
    /// own [`PreservedPosition::Top`] lines.
    pub preserved_after: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GanttDocument {
    /// The chart's title, without the `title` keyword. Mermaid drops a `title`
    /// statement with nothing after it, so an empty title is `None`.
    pub title: Option<String>,
    /// How the dates in the task fields are spelled, e.g. `YYYY-MM-DD`.
    pub date_format: Option<String>,
    /// How the dates along the axis are drawn, e.g. `%m/%d`.
    pub axis_format: Option<String>,
    pub sections: Vec<GanttSection>,
    pub preserved_lines: Vec<PreservedLine>,
}

impl GanttDocument {
    /// A blank gantt chart, used when the editor is opened on an empty fence.
    pub fn empty() -> Self {
        Self {
            title: None,
            date_format: None,
            axis_format: None,
            sections: Vec::new(),
            preserved_lines: Vec::new(),
        }
    }

    /// Every task, in the order the fence writes them, which is the order
    /// Mermaid resolves them in.
    pub fn tasks(&self) -> impl Iterator<Item = &GanttTask> {
        self.sections.iter().flat_map(|section| &section.tasks)
    }

    fn tasks_mut(&mut self) -> impl Iterator<Item = &mut GanttTask> {
        self.sections
            .iter_mut()
            .flat_map(|section| &mut section.tasks)
    }

    /// The ids `after` and `until` can address, in document order.
    pub fn task_ids(&self) -> Vec<&str> {
        self.tasks()
            .filter_map(|task| task.id.as_deref())
            .collect::<Vec<_>>()
    }

    /// Removes one task, keeping every task that pointed at it where it was.
    ///
    /// `after X` means "start when X ends" and `until X` "end when X starts".
    /// When X goes, a reference to it is replaced by the moment it resolved
    /// to: the rest of the list when that still resolves to the same moment,
    /// otherwise that moment spelled with the chart's `dateFormat`. A dangling
    /// id would be read as "today" without a word, and the length of X cannot
    /// be carried over by pointing at what X itself pointed at.
    ///
    /// The chart is resolved by `merman` before and after, and when a task
    /// that pointed at X would move, nothing is removed and the reason comes
    /// back as the error. `Ok(None)` when there is no such task.
    ///
    /// The lines written below the task stay where they were, below whatever
    /// came before it.
    pub fn remove_task(&mut self, section: usize, task: usize) -> Result<Option<GanttTask>> {
        let Some(position) = self.flat_index(section, task) else {
            return Ok(None);
        };
        let mut edited = self.clone();
        let Some(mut removed) = edited
            .sections
            .get_mut(section)
            .map(|section| section.tasks.remove(task))
        else {
            return Ok(None);
        };
        let lines = std::mem::take(&mut removed.preserved_after);
        edited.keep_lines_before(section, task, lines);
        edited.follow_removed_references(
            self,
            &[position],
            &i18n::t!("task `{}`", removed.label),
        )?;
        *self = edited;
        Ok(Some(removed))
    }

    /// Removes one section together with the tasks in it, the same way.
    pub fn remove_section(&mut self, section: usize) -> Result<Option<GanttSection>> {
        if section >= self.sections.len() {
            return Ok(None);
        }
        let first = self
            .sections
            .iter()
            .take(section)
            .map(|section| section.tasks.len())
            .sum::<usize>();
        let mut edited = self.clone();
        let mut removed = edited.sections.remove(section);
        let positions = (first..first + removed.tasks.len()).collect::<Vec<_>>();
        let mut lines = std::mem::take(&mut removed.preserved_after);
        for task in &mut removed.tasks {
            lines.append(&mut task.preserved_after);
        }
        edited.keep_lines_at_end_of(section, lines);
        let what = match &removed.name {
            Some(name) => i18n::t!("section `{name}`"),
            None => i18n::t!("the first section").to_string(),
        };
        edited.follow_removed_references(self, &positions, &what)?;
        *self = edited;
        Ok(Some(removed))
    }

    /// The position of a task among all of them, in fence order.
    fn flat_index(&self, section: usize, task: usize) -> Option<usize> {
        let before = self
            .sections
            .get(..section)?
            .iter()
            .map(|section| section.tasks.len())
            .sum::<usize>();
        (task < self.sections.get(section)?.tasks.len()).then_some(before + task)
    }

    /// Hands `lines` to the element that now sits right above task `task` of
    /// `section`, so that they are written exactly where they were.
    fn keep_lines_before(&mut self, section: usize, task: usize, lines: Vec<String>) {
        if lines.is_empty() {
            return;
        }
        if let Some(target) = self.sections.get_mut(section) {
            if let Some(previous) = task
                .checked_sub(1)
                .and_then(|previous| target.tasks.get_mut(previous))
            {
                previous.preserved_after.extend(lines);
                return;
            }
            if task == 0 && target.name.is_some() {
                target.preserved_after.extend(lines);
                return;
            }
        }
        self.keep_lines_at_end_of(section, lines);
    }

    /// Keeps a line the form does not show below the element read last.
    fn keep_below_last(&mut self, text: String) {
        self.keep_lines_at_end_of(self.sections.len(), vec![text]);
    }

    /// Hands `lines` to the last element of the sections before `section`, or
    /// to the top of the chart when there is none.
    fn keep_lines_at_end_of(&mut self, section: usize, lines: Vec<String>) {
        if lines.is_empty() {
            return;
        }
        for index in (0..section.min(self.sections.len())).rev() {
            let Some(candidate) = self.sections.get_mut(index) else {
                continue;
            };
            if let Some(last) = candidate.tasks.last_mut() {
                last.preserved_after.extend(lines);
                return;
            }
            if candidate.name.is_some() {
                candidate.preserved_after.extend(lines);
                return;
            }
        }
        self.preserved_lines
            .extend(lines.into_iter().map(|text| PreservedLine {
                position: PreservedPosition::Top,
                text,
            }));
    }

    /// Rewrites every `after` / `until` in `self` that names one of the tasks
    /// at `removed` in `before` onto the moment it resolved to there, and
    /// checks with `merman` that none of those tasks moved.
    fn follow_removed_references(
        &mut self,
        before: &GanttDocument,
        removed: &[usize],
        what: &str,
    ) -> Result<()> {
        let before_tasks = before.tasks().collect::<Vec<_>>();
        let still_there = self
            .task_ids()
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let removed_ids = removed
            .iter()
            .filter_map(|position| before_tasks.get(*position))
            .filter_map(|task| task.id.clone())
            .filter(|id| !still_there.contains(id))
            .collect::<Vec<_>>();
        let names_removed = |ids: &[String]| ids.iter().any(|id| removed_ids.contains(id));
        let affected = self.tasks().any(|task| {
            matches!(&task.start, GanttStart::After(ids) if names_removed(ids))
                || matches!(&task.end, GanttEnd::Until(ids) if names_removed(ids))
        });
        if !affected {
            return Ok(());
        }

        let times = gantt_dates::task_times(&emit_gantt(before)).map_err(|error| {
            anyhow!(i18n::t!(
                "Cannot delete {what}, because the dates of the tasks that refer to it cannot be calculated. The diagram cannot be drawn with its current content ({error}). Fix the fields outlined in red first."
            ))
        })?;
        if times.len() != before_tasks.len() {
            bail!(i18n::t!(
                "Cannot delete {what}, because the dates of the tasks that refer to it cannot be calculated."
            ));
        }
        let kept = (0..before_tasks.len())
            .filter(|position| !removed.contains(position))
            .collect::<Vec<_>>();
        let position_before = |position: usize| kept.get(position).copied();
        // Mermaid keeps one entry per id and lets the last task spelled with it
        // win, and so does this lookup.
        let time_of = |id: &str| {
            before_tasks
                .iter()
                .zip(&times)
                .rev()
                .find(|(task, _)| task.id.as_deref() == Some(id))
                .map(|(_, time)| *time)
        };
        let date_format = before.date_format.as_deref();
        let inclusive = before.has_preserved_statement("inclusiveEndDates");

        let mut failure = None;
        // The rest of an `after` list is tried first, because it keeps the
        // relation the user wrote; a date is the fallback, and the one that is
        // always tried when the first attempt moved something.
        for keep_relative in [true, false] {
            let mut candidate = self.clone();
            let mut pins: Vec<Pin> = Vec::new();
            let mut used_relative = false;
            let mut unwritable = None;
            for (position, task) in candidate.tasks_mut().enumerate() {
                let Some(&gantt_dates::TaskTime { start, end, .. }) =
                    kept.get(position).and_then(|index| times.get(*index))
                else {
                    continue;
                };
                if let GanttStart::After(ids) = &task.start
                    && names_removed(ids)
                {
                    let rest = ids
                        .iter()
                        .filter(|id| !removed_ids.contains(id))
                        .cloned()
                        .collect::<Vec<_>>();
                    let resolves = rest
                        .iter()
                        .filter_map(|id| time_of(id))
                        .map(|time| time.end)
                        .max();
                    if keep_relative && resolves == Some(start) {
                        task.start = GanttStart::After(rest);
                        used_relative = true;
                    } else {
                        match gantt_dates::start_text(date_format, start) {
                            Ok(text) => task.start = GanttStart::Date(text),
                            Err(error) => {
                                unwritable = Some(i18n::t!(
                                    "Cannot delete {what}: the start of task `{}` would have to be written as a date, which is not possible ({error}).",
                                    task.label
                                ));
                                break;
                            }
                        }
                    }
                    pins.push(Pin {
                        position,
                        is_start: true,
                        before: position_before(position),
                        label: task.label.clone(),
                    });
                }
                if let GanttEnd::Until(ids) = &task.end
                    && names_removed(ids)
                {
                    let rest = ids
                        .iter()
                        .filter(|id| !removed_ids.contains(id))
                        .cloned()
                        .collect::<Vec<_>>();
                    let resolves = rest
                        .iter()
                        .filter_map(|id| time_of(id))
                        .map(|time| time.start)
                        .min();
                    if keep_relative && resolves == Some(end) {
                        task.end = GanttEnd::Until(rest);
                        used_relative = true;
                    } else {
                        match gantt_dates::end_text(date_format, inclusive, end, start) {
                            Ok(text) => task.end = GanttEnd::Fixed(text),
                            Err(error) => {
                                unwritable = Some(i18n::t!(
                                    "Cannot delete {what}: the end of task `{}` would have to be written as a date, which is not possible ({error}).",
                                    task.label
                                ));
                                break;
                            }
                        }
                    }
                    pins.push(Pin {
                        position,
                        is_start: false,
                        before: position_before(position),
                        label: task.label.clone(),
                    });
                }
            }

            if let Some(reason) = unwritable {
                failure = Some(reason);
            } else {
                // The whole of a rewritten task is compared, not only the field
                // that was rewritten: an end spelled as a date is read without
                // the `excludes` that pushed the same end out of an `until`, and
                // the bar is then drawn to a different day.
                let moved = match gantt_dates::task_times(&emit_gantt(&candidate)) {
                    Ok(after) => pins.iter().find(|pin| {
                        let expected = pin.before.and_then(|index| times.get(index));
                        after.get(pin.position) != expected || expected.is_none()
                    }),
                    Err(_) => pins.first(),
                };
                let Some(pin) = moved else {
                    *self = candidate;
                    return Ok(());
                };
                let task = &pin.label;
                failure = Some(if pin.is_start {
                    i18n::t!(
                        "Cannot delete {what}: the start of task `{task}`, which refers to it, would change. First rewrite the `after` / `until` of `{task}` as a date."
                    )
                } else {
                    i18n::t!(
                        "Cannot delete {what}: the end of task `{task}`, which refers to it, would change. First rewrite the `after` / `until` of `{task}` as a date."
                    )
                });
            }
            if !used_relative {
                break;
            }
        }
        Err(anyhow!(
            "{}",
            failure.unwrap_or_else(|| i18n::t!("Cannot delete {what}."))
        ))
    }

    /// Whether one of the verbatim lines of the body is the statement
    /// `keyword`.
    fn has_preserved_statement(&self, keyword: &str) -> bool {
        self.preserved_texts()
            .any(|text| starts_with_gantt_keyword(text.trim_start(), keyword))
    }

    /// Every verbatim line of the body, wherever it is kept.
    fn preserved_texts(&self) -> impl Iterator<Item = &str> {
        self.preserved_lines
            .iter()
            .filter(|line| line.position != PreservedPosition::Preamble)
            .map(|line| line.text.as_str())
            .chain(self.sections.iter().flat_map(|section| {
                section.preserved_after.iter().map(String::as_str).chain(
                    section
                        .tasks
                        .iter()
                        .flat_map(|task| task.preserved_after.iter().map(String::as_str)),
                )
            }))
    }

    /// Points every `after`, `until` and `click` that names an id in the left
    /// half of `renames` at the right half instead, in one pass, so that two
    /// ids trading places do not collapse into one.
    pub fn rename_references(&mut self, renames: &[(String, String)]) {
        if renames.is_empty() {
            return;
        }
        let rename = |id: &mut String| {
            if let Some((_, new)) = renames.iter().find(|(old, _)| old == id) {
                *id = new.clone();
            }
        };
        for task in self.tasks_mut() {
            if let GanttStart::After(ids) = &mut task.start {
                ids.iter_mut().for_each(rename);
            }
            if let GanttEnd::Until(ids) = &mut task.end {
                ids.iter_mut().for_each(rename);
            }
        }
        let rewrite = |text: &mut String| {
            if let Some(rewritten) = rename_click_ids(text, renames) {
                *text = rewritten;
            }
        };
        for line in &mut self.preserved_lines {
            if line.position != PreservedPosition::Preamble {
                rewrite(&mut line.text);
            }
        }
        for section in &mut self.sections {
            section.preserved_after.iter_mut().for_each(rewrite);
            for task in &mut section.tasks {
                task.preserved_after.iter_mut().for_each(rewrite);
            }
        }
    }

    /// The body in the order Mermaid reads it, from the first line after the
    /// header statements to the last one.
    fn body(&self) -> Vec<BodyItem<'_>> {
        let mut items = self
            .preserved_lines
            .iter()
            .filter(|line| line.position == PreservedPosition::Top)
            .map(|line| BodyItem::Line(line.text.as_str()))
            .collect::<Vec<_>>();
        for section in &self.sections {
            if let Some(name) = &section.name {
                items.push(BodyItem::Section(name));
            }
            items.extend(
                section
                    .preserved_after
                    .iter()
                    .map(|line| BodyItem::Line(line.as_str())),
            );
            for task in &section.tasks {
                items.push(BodyItem::Task(task));
                items.extend(
                    task.preserved_after
                        .iter()
                        .map(|line| BodyItem::Line(line.as_str())),
                );
            }
        }
        items.extend(
            self.preserved_lines
                .iter()
                .filter(|line| line.position == PreservedPosition::Bottom)
                .map(|line| BodyItem::Line(line.text.as_str())),
        );
        items
    }

    /// Why the chart cannot be written back as a diagram Mermaid accepts, or
    /// `None` when it can.
    pub fn structure_error(&self) -> Option<String> {
        if let Some(title) = &self.title
            && !is_valid_gantt_title(title)
        {
            return Some(i18n::t!(
                "The title cannot contain line breaks or `%%{{` (and cannot be empty)."
            ));
        }
        for (format, name) in [
            (&self.date_format, "dateFormat"),
            (&self.axis_format, "axisFormat"),
        ] {
            if let Some(format) = format
                && !is_valid_gantt_format(format)
            {
                return Some(i18n::t!(
                    "`{format}` cannot be used for `{name}` (`#`, `;` and line breaks are not allowed, and it cannot be empty)."
                ));
            }
        }

        for (index, section) in self.sections.iter().enumerate() {
            match &section.name {
                Some(name) if !is_valid_gantt_section_name(name) => {
                    return Some(i18n::t!(
                        "The section name `{name}` cannot be used (line breaks and `%%{{` are not allowed)."
                    ));
                }
                None if index > 0 => {
                    return Some(
                        i18n::t!("Every section after the first needs a name (without one, it merges with the section above).")
                            .to_string(),
                    );
                }
                _ => {}
            }
        }

        let ids = self.task_ids();
        // Mermaid keeps one entry per id and lets the last task spelled with it
        // win, so a reference never names the first of two. Deleting or
        // renaming either of them would rewrite an `after` that was pointing at
        // the other one, which is why such a document is refused instead.
        for (position, id) in ids.iter().enumerate() {
            if ids
                .get(..position)
                .is_some_and(|earlier| earlier.contains(id))
            {
                return Some(i18n::t!(
                    "The ID `{id}` is used by two tasks. Mermaid uses only the later one as the target of `after`, so change one of the IDs."
                ));
            }
        }

        for (index, task) in self.tasks().enumerate() {
            if !is_valid_gantt_label(&task.label) {
                return Some(i18n::t!(
                    "The task name `{}` cannot be used (`:`, `%%{{` and line breaks are not allowed, and it cannot start with `%%`).",
                    task.label
                ));
            }
            if let Some(id) = &task.id {
                if !is_valid_gantt_task_id(id) {
                    return Some(i18n::t!(
                        "The ID `{id}` cannot be used (`,`, `#`, `;` and line breaks are not allowed, and it cannot be empty; tag names such as `done` cannot be used either)."
                    ));
                }
                if task.start == GanttStart::Implicit {
                    return Some(i18n::t!(
                        "To give task `{}` an ID, make its start a date or `after`.",
                        task.label
                    ));
                }
            }
            if index == 0 && task.start == GanttStart::Implicit {
                return Some(i18n::t!(
                    "The first task `{}` needs a start date (there is no previous task).",
                    task.label
                ));
            }
            match &task.start {
                GanttStart::Date(date) if date.is_empty() => {
                    return Some(i18n::t!("Enter the start date of task `{}`.", task.label));
                }
                GanttStart::Date(date) if !is_valid_gantt_date(date) => {
                    return Some(i18n::t!(
                        "The start `{date}` of task `{}` cannot be used (`,`, `#`, `;` and line breaks are not allowed, and it cannot be empty).",
                        task.label
                    ));
                }
                GanttStart::After(references) if references.is_empty() => {
                    return Some(i18n::t!(
                        "The `after` of task `{}` has no target.",
                        task.label
                    ));
                }
                _ => {}
            }
            match &task.end {
                GanttEnd::Fixed(text) if text.is_empty() => {
                    return Some(i18n::t!(
                        "Enter the end of task `{}` (a date or a duration such as `5d`).",
                        task.label
                    ));
                }
                GanttEnd::Fixed(text) if !is_valid_gantt_date(text) => {
                    return Some(i18n::t!(
                        "The end `{text}` of task `{}` cannot be used (`,`, `#`, `;` and line breaks are not allowed, and it cannot be empty).",
                        task.label
                    ));
                }
                GanttEnd::Until(references) if references.is_empty() => {
                    return Some(i18n::t!(
                        "The `until` of task `{}` has no target.",
                        task.label
                    ));
                }
                _ => {}
            }
            for reference in task.references() {
                // A task may be called anything it can be spelled with, but
                // Mermaid reads the run after `after` as `[A-Za-z0-9_- ]` and
                // stops there, so the ones that are pointed at are narrower
                // than the ones that are merely written.
                if !is_valid_gantt_id(reference) {
                    return Some(i18n::t!(
                        "IDs referred to by `after` / `until` can only contain ASCII letters, digits, `_` and `-` (`{reference}` in task `{}`).",
                        task.label
                    ));
                }
                if !ids.contains(&reference) {
                    return Some(i18n::t!(
                        "The ID `{reference}` that task `{}` refers to does not exist.",
                        task.label
                    ));
                }
            }
        }

        // Mermaid attaches a `click` to the tasks it names as the line is read,
        // so one written above its task does nothing. Moving a task below the
        // line would silently take its link away, and moving one above it
        // would give it one.
        let mut defined: Vec<&str> = Vec::new();
        for item in self.body() {
            match item {
                BodyItem::Task(task) => defined.extend(task.id.as_deref()),
                BodyItem::Line(text) => {
                    for id in click_ids(text) {
                        // A task without an id is `task1`, `task2`, … in the
                        // order `merman` reads it, so adding, deleting or
                        // moving any task without an id would hand the link
                        // to a different task without a word.
                        if !ids.contains(&id) && is_automatic_task_id(id) {
                            return Some(i18n::t!(
                                "`{id}` in `click` is the name a task without an ID gets automatically. It points to a different task when tasks are added, deleted or moved, so give that task an ID and change `click` to use it."
                            ));
                        }
                        if ids.contains(&id) && !defined.contains(&id) {
                            return Some(i18n::t!(
                                "The `click` line is above task `{id}`. Mermaid does not add links to tasks written after `click`, so move task `{id}` above the `click` line."
                            ));
                        }
                    }
                }
                BodyItem::Section(_) => {}
            }
        }
        None
    }
}

/// Whether `source` is a gantt chart. Cheap enough to call before parsing.
pub fn is_gantt_source(source: &str) -> bool {
    first_statement_line(source).is_some_and(|line| keyword_tail(line, "gantt").is_some())
}

/// Reads `source` — the body of a ```` ```mermaid ```` fence — into an editable
/// document.
pub fn parse_gantt(source: &str) -> Result<GanttDocument> {
    if let Some(problem) = crate::mermaid_model::preprocessing_problem(source) {
        bail!("{problem}");
    }
    let document = scan_gantt(source)?;

    // `merman` is asked last, so that a construct this module knows about is
    // reported with the reason that says what to do rather than with whatever
    // the resolver tripped over first.
    if !source.trim().is_empty() {
        let parsed = crate::parse_for_render_model(source)
            .map_err(|error| {
                anyhow!(i18n::t!(
                    "Mermaid could not parse this diagram ({error}). Edit it as text."
                ))
            })?
            .ok_or_else(|| anyhow!(i18n::t!("Mermaid could not read this as a diagram.")))?;
        if !matches!(parsed.model(), RenderSemanticModel::Gantt(_)) {
            bail!(i18n::t!("This diagram is not a Gantt chart."));
        }
    }

    Ok(document)
}

fn scan_gantt(source: &str) -> Result<GanttDocument> {
    let lines = source.lines().collect::<Vec<_>>();
    let preamble = scan_preamble(&lines);
    let mut document = GanttDocument::empty();
    document.preserved_lines = preamble.preserved;

    // An empty fence: the editor opens on a blank chart, and whatever comments
    // the user left keep their place.
    let Some(header_index) = preamble.header else {
        return Ok(document);
    };

    let header = lines.get(header_index).copied().unwrap_or_default().trim();
    let Some(tail) = keyword_tail(header, "gantt") else {
        bail!(i18n::t!(
            "This is not a Gantt chart (only diagrams that start with `gantt` can be edited in the structured editor)."
        ));
    };
    if !tail.is_empty() {
        bail!(i18n::t!(
            "`{tail}` on the `gantt` line is not supported by the structured editor. Move it to the next line, or edit it as text."
        ));
    }

    let mut index = header_index + 1;
    while let Some(line) = lines.get(index).copied() {
        let start = index;
        index += 1;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with("%%") {
            if trimmed.starts_with("%%{") && !trimmed.contains("}%%") {
                while lines.get(index).is_some_and(|line| !line.contains("}%%")) {
                    index += 1;
                }
                if index < lines.len() {
                    index += 1;
                }
            }
            document.keep_below_last(join_lines(lines.get(start..index).unwrap_or_default()));
            continue;
        }

        // Mermaid reads a statement from the first non-blank column, keeping
        // whatever whitespace follows its keyword, so the line is only trimmed
        // at its front here and each value is trimmed as it is taken.
        let statement = line.trim_start();
        match classify(statement) {
            Statement::Preserved => {
                if is_accessibility_block(statement) {
                    while lines.get(index).is_some_and(|line| !line.contains('}')) {
                        index += 1;
                    }
                    if index < lines.len() {
                        index += 1;
                    }
                }
                document.keep_below_last(join_lines(lines.get(start..index).unwrap_or_default()));
            }
            Statement::Title(value) => {
                let value = value.trim();
                // Mermaid keeps the last one, so a second statement — even an
                // empty one, which clears the title — would make writing the
                // chart back drop a line the user can see.
                if document.title.is_some() {
                    bail!(i18n::t!(
                        "`title` is written twice. Merge them into one, then open it again."
                    ));
                }
                document.title = set_header_value(value, "title")?;
            }
            Statement::DateFormat(value) => {
                let value = value.trim();
                if document.date_format.is_some() {
                    bail!(i18n::t!(
                        "`dateFormat` is written twice. Merge them into one, then open it again."
                    ));
                }
                if value.contains(['#', ';']) {
                    bail!(i18n::t!(
                        "Mermaid ignores everything from `#` or `;` onward in the `dateFormat` value `{value}`. Edit it as text."
                    ));
                }
                document.date_format = set_header_value(value, "dateFormat")?;
            }
            Statement::AxisFormat(value) => {
                let value = value.trim();
                if document.axis_format.is_some() {
                    bail!(i18n::t!(
                        "`axisFormat` is written twice. Merge them into one, then open it again."
                    ));
                }
                if value.contains(['#', ';']) {
                    bail!(i18n::t!(
                        "Mermaid ignores everything from `#` or `;` onward in the `axisFormat` value `{value}`. Edit it as text."
                    ));
                }
                document.axis_format = set_header_value(value, "axisFormat")?;
            }
            Statement::Section(name) => {
                let name = name.trim();
                if !is_valid_gantt_section_name(name) {
                    bail!(i18n::t!(
                        "A `section` without a name is not supported by the structured editor ({statement}). Give it a name, or edit it as text."
                    ));
                }
                document.sections.push(GanttSection {
                    name: Some(name.to_string()),
                    tasks: Vec::new(),
                    preserved_after: Vec::new(),
                });
            }
            Statement::DisplayMode => {
                bail!(i18n::t!(
                    "`displayMode` cannot be written in the body ({statement}). In Mermaid, set it with `displayMode: compact` in the front matter or with `%%{{init: {{\"gantt\": {{\"displayMode\": \"compact\"}}}}}}%%`. Edit it as text."
                ));
            }
            Statement::Task => {
                let task = parse_task(statement)?;
                if document.sections.is_empty() {
                    document.sections.push(GanttSection {
                        name: None,
                        tasks: Vec::new(),
                        preserved_after: Vec::new(),
                    });
                }
                if let Some(section) = document.sections.last_mut() {
                    section.tasks.push(task);
                }
            }
        }
    }

    // Mermaid resolves an `after` that names nothing to today without saying
    // so, which is a fence whose drawing moves every day. Opening the editor on
    // one and writing it back unchanged would hide that, so it is refused with
    // the reason instead.
    if let Some(error) = document.structure_error() {
        bail!("{error}");
    }
    Ok(document)
}

/// A header value, or `None` for the spelling Mermaid reads as no statement at
/// all (`title` with nothing after it).
fn set_header_value(value: &str, keyword: &str) -> Result<Option<String>> {
    if value.is_empty() {
        return Ok(None);
    }
    if value.contains(['\n', '\r']) {
        bail!(i18n::t!(
            "The value of `{keyword}` cannot contain line breaks."
        ));
    }
    Ok(Some(value.to_string()))
}

/// What one line of a gantt body is, decided the way `merman`'s gantt parser
/// decides it.
enum Statement<'a> {
    /// Held verbatim: a header statement the form does not show, an
    /// accessibility statement or a `click`.
    Preserved,
    Title(&'a str),
    DateFormat(&'a str),
    AxisFormat(&'a str),
    Section(&'a str),
    /// `displayMode` written in the body, which Mermaid only reads from the
    /// configuration.
    DisplayMode,
    Task,
}

/// The statements whose keyword has to be followed by whitespace and whose
/// argument this model holds verbatim.
const PRESERVED_ARGUMENT_KEYWORDS: [&str; 6] = [
    "tickInterval",
    "includes",
    "excludes",
    "todayMarker",
    "weekday",
    "weekend",
];

/// The statements Mermaid recognizes from their keyword alone, with no
/// whitespace needed after it — so `topAxisish` is one of them too.
const PRESERVED_BARE_KEYWORDS: [&str; 2] = ["inclusiveEndDates", "topAxis"];

fn classify(statement: &str) -> Statement<'_> {
    if let Some(value) = gantt_keyword_tail(statement, "dateFormat") {
        return Statement::DateFormat(value);
    }
    if let Some(value) = gantt_keyword_tail(statement, "axisFormat") {
        return Statement::AxisFormat(value);
    }
    if PRESERVED_BARE_KEYWORDS
        .iter()
        .any(|keyword| starts_with_gantt_keyword(statement, keyword))
        || PRESERVED_ARGUMENT_KEYWORDS
            .iter()
            .any(|keyword| gantt_keyword_tail(statement, keyword).is_some())
        || accessibility_kind(statement).is_some()
        // `merman` takes any line that opens with the word, with or without
        // whitespace after it, so a task called `clicks` is a `click` too.
        || starts_with_gantt_keyword(statement, "click")
    {
        return Statement::Preserved;
    }
    if let Some(value) = gantt_keyword_tail(statement, "title") {
        return Statement::Title(value);
    }
    if let Some(value) = gantt_keyword_tail(statement, "section") {
        return Statement::Section(value);
    }
    // Neither Mermaid nor `merman` has a `displayMode` statement: with a `:`
    // the line is an ordinary task, and without one it is an error that is
    // worth explaining.
    if gantt_keyword_tail(statement, "displayMode").is_some() && !statement.contains(':') {
        return Statement::DisplayMode;
    }
    Statement::Task
}

/// Which spelling of an accessibility statement `statement` is, or `None` when
/// it is not one.
fn accessibility_kind(statement: &str) -> Option<bool> {
    for keyword in ["accTitle", "accDescr"] {
        if !starts_with_gantt_keyword(statement, keyword) {
            continue;
        }
        let rest = statement
            .get(keyword.len()..)
            .unwrap_or_default()
            .trim_start();
        if rest.starts_with(':') {
            return Some(false);
        }
        if keyword == "accDescr" && rest.starts_with('{') {
            return Some(true);
        }
    }
    None
}

/// Whether the statement opens an `accDescr { … }` block that runs past its own
/// line.
fn is_accessibility_block(statement: &str) -> bool {
    accessibility_kind(statement) == Some(true) && !statement.contains('}')
}

/// What follows `keyword` at the start of `line`, the way `merman`'s gantt
/// parser reads a keyword argument: the word is matched without regard to case
/// and has to be followed by one whitespace character, which is dropped.
///
/// The rest is returned as it was written, because Mermaid keeps it that way;
/// the caller trims it.
fn gantt_keyword_tail<'a>(line: &'a str, keyword: &str) -> Option<&'a str> {
    if !starts_with_gantt_keyword(line, keyword) {
        return None;
    }
    let rest = line.get(keyword.len()..)?;
    let first = rest.chars().next()?;
    if !first.is_whitespace() {
        return None;
    }
    rest.get(first.len_utf8()..)
}

/// Whether `line` opens with `keyword`, ignoring case. `get` rather than
/// slicing, because a line that opens with a multibyte label would otherwise
/// be cut inside a character.
fn starts_with_gantt_keyword(line: &str, keyword: &str) -> bool {
    line.get(..keyword.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(keyword))
}

fn parse_task(statement: &str) -> Result<GanttTask> {
    let Some(colon) = statement.find(':') else {
        bail!(i18n::t!(
            "Could not read this as a Gantt chart line ({statement}). Write it as `task name :start, end`."
        ));
    };
    let label = statement.get(..colon).unwrap_or_default().trim();
    let data = statement.get(colon + 1..).unwrap_or_default();
    if label.is_empty() {
        bail!(i18n::t!("There is no task name before `:` ({statement})."));
    }
    if data.trim().is_empty() {
        bail!(i18n::t!(
            "There is no start or end after `:` ({statement})."
        ));
    }
    if data.contains(['#', ';']) {
        bail!(i18n::t!(
            "Mermaid ignores everything from `#` or `;` onward in the fields of a task ({statement}). Edit it as text."
        ));
    }
    if !is_valid_gantt_label(label) {
        bail!(i18n::t!(
            "`{label}` cannot be used as a task name ({statement}). `:` and words that start a statement, such as `title` and `section`, are not allowed."
        ));
    }

    let mut fields = data.split(',').map(str::trim).collect::<Vec<_>>();
    let mut tags: Vec<GanttTag> = Vec::new();
    while let Some(tag) = fields.first().copied().and_then(GanttTag::from_keyword) {
        if !tags.contains(&tag) {
            tags.push(tag);
        }
        fields.remove(0);
    }

    // Mermaid tells the fields apart by counting them, and resolves nothing at
    // all when there are none or more than three.
    let (id, start, end) = match fields.as_slice() {
        [end] => (None, None, *end),
        [start, end] => (None, Some(*start), *end),
        [id, start, end] => (Some(*id), Some(*start), *end),
        _ => bail!(i18n::t!(
            "Could not read the task fields `{}` ({statement}). Of `tags, ID, start, end`, write 1 to 3 fields, not counting the tags.",
            data.trim()
        )),
    };

    let id = match id {
        None => None,
        Some(id) => {
            // Anything spellable is kept, and whether it can be pointed at is
            // decided by [`GanttDocument::structure_error`] from the references
            // that are actually there.
            if !is_valid_gantt_task_id(id) {
                bail!(i18n::t!(
                    "The ID `{id}` is not supported by the structured editor ({statement}). `,`, `#`, `;` and line breaks cannot be used, and tag names such as `done` cannot be IDs."
                ));
            }
            Some(id.to_string())
        }
    };

    let start = match start {
        None => GanttStart::Implicit,
        Some(text) => match relative_reference(text, "after") {
            Some(ids) => GanttStart::After(ids?),
            None => {
                if relative_reference(text, "until").is_some() {
                    bail!(i18n::t!("The start cannot be `until` ({statement})."));
                }
                if !is_valid_gantt_date(text) {
                    bail!(i18n::t!("Could not read the start `{text}` ({statement})."));
                }
                GanttStart::Date(text.to_string())
            }
        },
    };

    let end = match relative_reference(end, "until") {
        Some(ids) => GanttEnd::Until(ids?),
        None => {
            if relative_reference(end, "after").is_some() {
                bail!(i18n::t!(
                    "The end cannot be `after` ({statement}). Mermaid reads it as a duration of 0."
                ));
            }
            if !is_valid_gantt_date(end) {
                bail!(i18n::t!("Could not read the end `{end}` ({statement})."));
            }
            GanttEnd::Fixed(end.to_string())
        }
    };

    Ok(GanttTask {
        label: label.to_string(),
        tags,
        id,
        start,
        end,
        preserved_after: Vec::new(),
    })
}

/// The ids `text` names with `keyword`, or `None` when it does not open with
/// that keyword.
///
/// Mermaid reads the ids as the run of `[A-Za-z0-9_- ]` that follows, and
/// silently ignores whatever comes after it — which is why anything but a plain
/// list of ids is refused here rather than quietly shortened.
fn relative_reference(text: &str, keyword: &str) -> Option<Result<Vec<String>>> {
    let rest = text.strip_prefix(keyword)?;
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let ids = rest
        .trim()
        .split(' ')
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    if ids.is_empty() || ids.iter().any(|id| !is_valid_gantt_id(id)) {
        return Some(Err(anyhow!(i18n::t!(
            "Could not read the targets `{}` of `{keyword}`. Write IDs made of ASCII letters, digits, `_` and `-`, separated by spaces.",
            rest.trim()
        ))));
    }
    Some(Ok(ids))
}

/// Writes `document` back as Mermaid source, without a trailing newline.
pub fn emit_gantt(document: &GanttDocument) -> String {
    let mut out = String::new();

    for line in preserved_at(document, PreservedPosition::Preamble) {
        out.push_str(line);
        out.push('\n');
    }

    out.push_str("gantt\n");
    for (keyword, value) in [
        ("title", &document.title),
        ("dateFormat", &document.date_format),
        ("axisFormat", &document.axis_format),
    ] {
        if let Some(value) = value {
            let _ = writeln!(out, "{INDENT}{keyword} {value}");
        }
    }

    for item in document.body() {
        match item {
            BodyItem::Line(line) => {
                out.push_str(line);
                out.push('\n');
            }
            BodyItem::Section(name) => {
                let _ = writeln!(out, "{INDENT}section {name}");
            }
            BodyItem::Task(task) => {
                let _ = writeln!(out, "{INDENT}{}", emit_task(task));
            }
        }
    }

    while out.ends_with('\n') {
        out.pop();
    }
    out
}

fn emit_task(task: &GanttTask) -> String {
    let mut fields = task
        .tags
        .iter()
        .map(|tag| tag.keyword().to_string())
        .collect::<Vec<_>>();
    if let Some(id) = &task.id {
        fields.push(id.clone());
    }
    match &task.start {
        GanttStart::Implicit => {}
        GanttStart::Date(date) => fields.push(date.clone()),
        GanttStart::After(ids) => fields.push(format!("after {}", ids.join(" "))),
    }
    match &task.end {
        GanttEnd::Fixed(text) => fields.push(text.clone()),
        GanttEnd::Until(ids) => fields.push(format!("until {}", ids.join(" "))),
    }
    format!("{} :{}", task.label, fields.join(", "))
}

fn preserved_at(
    document: &GanttDocument,
    position: PreservedPosition,
) -> impl Iterator<Item = &str> {
    document
        .preserved_lines
        .iter()
        .filter(move |line| line.position == position)
        .map(|line| line.text.as_str())
}

/// Whether `label` can be written as a task name.
///
/// The `:` is what separates the name from the fields, and a name that opens
/// with a statement keyword is read as that statement instead — `title x : …`
/// becomes the chart's title, and a line that opens with `click` disappears
/// altogether. That is decided on the very line [`emit_gantt`] writes
/// and by the rules the parser reads that line with, so `sectional review` is
/// a task while `section 2` — and a bare `title`, which is followed by the
/// space before the `:` — are not.
pub fn is_valid_gantt_label(label: &str) -> bool {
    if label.is_empty() || label.trim() != label {
        return false;
    }
    if label.contains([':', '\n', '\r']) {
        return false;
    }
    if label.starts_with("%%") || crate::mermaid_model::opens_directive(label) {
        return false;
    }
    matches!(classify(&format!("{label} :")), Statement::Task)
}

/// One line of the body as [`emit_gantt`] writes it.
enum BodyItem<'a> {
    Line(&'a str),
    Section(&'a str),
    Task(&'a GanttTask),
}

/// A task whose start or end a deletion rewrote, and where it was before.
struct Pin {
    position: usize,
    is_start: bool,
    /// The task's position in the chart before the deletion.
    before: Option<usize>,
    label: String,
}

/// The byte range of the id list in a `click` line, the way `merman` reads it:
/// the first run of non-whitespace after the keyword.
fn click_id_range(text: &str) -> Option<std::ops::Range<usize>> {
    let statement = text.trim_start();
    if !starts_with_gantt_keyword(statement, "click") {
        return None;
    }
    let keyword_end = text.len() - statement.len() + "click".len();
    let rest = text.get(keyword_end..)?;
    let start = keyword_end + (rest.len() - rest.trim_start().len());
    let tail = text.get(start..)?;
    let length = tail.find(char::is_whitespace).unwrap_or(tail.len());
    (length > 0).then_some(start..start + length)
}

/// The ids a `click` line names, or nothing when `text` is not one.
fn click_ids(text: &str) -> Vec<&str> {
    click_id_range(text)
        .and_then(|range| text.get(range))
        .map(|ids| {
            ids.split(',')
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

/// `text` with the ids of its `click` renamed, or `None` when nothing changes.
fn rename_click_ids(text: &str, renames: &[(String, String)]) -> Option<String> {
    let range = click_id_range(text)?;
    let ids = text.get(range.clone())?;
    let renamed = ids
        .split(',')
        .map(|id| {
            renames
                .iter()
                .find(|(old, _)| old == id.trim())
                .map_or(id, |(_, new)| new.as_str())
        })
        .collect::<Vec<_>>()
        .join(",");
    if renamed == ids {
        return None;
    }
    Some(format!(
        "{}{renamed}{}",
        text.get(..range.start)?,
        text.get(range.end..)?
    ))
}

/// Whether `id` is spelled like the id `merman` gives a task written without
/// one.
fn is_automatic_task_id(id: &str) -> bool {
    id.strip_prefix("task").is_some_and(|number| {
        !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())
    })
}

/// Whether `id` can be addressed by `after` / `until`.
///
/// Mermaid reads the ids after `after` as a run of `[A-Za-z0-9_- ]` and stops
/// at the first character that is not one, so nothing else can be pointed at.
/// A task may still *carry* a wider id — see [`is_valid_gantt_task_id`].
pub fn is_valid_gantt_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
        && GanttTag::from_keyword(id).is_none()
}

/// Whether `id` can be written as a task's own id.
///
/// The id is a comma separated field like the start and the end, so the only
/// things in the way are the characters that would end the field early and a
/// word Mermaid eats as a tag before it counts the fields. A Japanese id is
/// written back as it was; it simply cannot be named by an `after`.
pub fn is_valid_gantt_task_id(id: &str) -> bool {
    is_valid_gantt_date(id) && GanttTag::from_keyword(id).is_none()
}

/// Whether `text` can be written as a task's start or end.
///
/// The value is kept as the source spells it, so the only checks are the ones
/// that decide whether Mermaid would still read it as one field: a `,` starts
/// the next field, and `#` and `;` cut the rest of the statement off.
pub fn is_valid_gantt_date(text: &str) -> bool {
    !text.is_empty()
        && text.trim() == text
        && !text.contains([',', '#', ';', '\n', '\r'])
        && !opens_with_relative_keyword(text)
}

/// Whether `text` opens with `after` or `until`, which belong to the start and
/// the end field respectively and are never part of a date.
fn opens_with_relative_keyword(text: &str) -> bool {
    ["after", "until"].iter().any(|keyword| {
        text.strip_prefix(keyword)
            .is_some_and(|rest| rest.starts_with(char::is_whitespace))
    })
}

/// Whether `text` is a duration Mermaid can add to a start time, rather than a
/// date it would read with the chart's `dateFormat`.
pub fn is_gantt_duration(text: &str) -> bool {
    let integer = text.len() - text.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    if integer == 0 {
        return false;
    }
    let mut rest = text.get(integer..).unwrap_or_default();
    if let Some(after_dot) = rest.strip_prefix('.') {
        let fraction = after_dot.len()
            - after_dot
                .trim_start_matches(|c: char| c.is_ascii_digit())
                .len();
        if fraction == 0 {
            return false;
        }
        rest = after_dot.get(fraction..).unwrap_or_default();
    }
    matches!(rest, "M" | "d" | "h" | "m" | "s" | "w" | "y" | "ms")
}

/// Whether `name` can be written as a section name. The name runs to the end of
/// its line, so only a line break and an empty name are in the way.
pub fn is_valid_gantt_section_name(name: &str) -> bool {
    !name.is_empty()
        && name.trim() == name
        && !name.contains(['\n', '\r'])
        && !crate::mermaid_model::opens_directive(name)
}

/// Whether `title` can be written as the chart's title. Mermaid drops a `title`
/// statement with nothing after it, so an empty title has to be `None` instead.
pub fn is_valid_gantt_title(title: &str) -> bool {
    !title.is_empty()
        && title.trim() == title
        && !title.contains(['\n', '\r'])
        && !crate::mermaid_model::opens_directive(title)
}

/// Whether `format` can be written as a `dateFormat` or `axisFormat` value,
/// which Mermaid cuts at the first `#` or `;`.
pub fn is_valid_gantt_format(format: &str) -> bool {
    !format.is_empty() && format.trim() == format && !format.contains(['#', ';', '\n', '\r'])
}

#[cfg(test)]
mod gantt2_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MermaidTheme, render_to_svg};

    /// The emitted source must parse to the same document and emit the same
    /// text, otherwise confirming the editor twice would keep rewriting the
    /// fence.
    fn assert_stable(source: &str) -> String {
        let first =
            parse_gantt(source).unwrap_or_else(|error| panic!("{source} should parse: {error}"));
        let first_text = emit_gantt(&first);
        let second = parse_gantt(&first_text)
            .unwrap_or_else(|error| panic!("emitted source should parse: {error}\n{first_text}"));
        let second_text = emit_gantt(&second);
        assert_eq!(
            first, second,
            "the document changed through a round trip of:\n{first_text}"
        );
        assert_eq!(
            first_text, second_text,
            "emitting twice changed the source:\n{first_text}\n---\n{second_text}"
        );
        assert_eq!(
            first.structure_error(),
            None,
            "a parsed document should always be writable back:\n{first_text}"
        );
        first_text
    }

    fn assert_renders(source: &str) {
        let svg = render_to_svg(source, &MermaidTheme::default())
            .unwrap_or_else(|error| panic!("emitted source should render: {error}\n{source}"));
        assert!(
            !svg.contains("Syntax error in text"),
            "emitted source rendered an error diagram:\n{source}"
        );
    }

    const TEMPLATE: &str = "gantt\n    title 開発スケジュール\n    dateFormat YYYY-MM-DD\n    axisFormat %m/%d\n    section 設計\n    要件定義 :done, req, 2026-01-06, 5d\n    基本設計 :active, design, after req, 7d\n    section 実装\n    実装 :impl, after design, 10d";

    const EXAMPLES: [&str; 9] = [
        TEMPLATE,
        "gantt\n    dateFormat YYYY-MM-DD\n    A :a, 2026-01-01, 7d",
        // No section at all: the tasks live in the nameless band Mermaid draws
        // before the first `section`.
        "gantt\n    dateFormat YYYY-MM-DD\n    最初 :first, 2026-01-01, 3d\n    次 :2d\n    section 後半\n    仕上げ :after first, 1w",
        // Every tag, including a task that carries several.
        "gantt\n    dateFormat YYYY-MM-DD\n    A :done, crit, a, 2026-01-01, 7d\n    M :milestone, m, 2026-01-08, 0d\n    V :vert, v, 2026-01-09, 1d\n    B :active, b, after a, 3d",
        // `until` in the end field, and a multi-id `after`.
        "gantt\n    dateFormat YYYY-MM-DD\n    A :a, 2026-01-01, 7d\n    B :b, 2026-01-02, 5d\n    C :c, after a b, 2d\n    D :d, 2026-01-03, until a",
        // The header statements the form does not show.
        "gantt\n    title 図\n    dateFormat YYYY-MM-DD\n    excludes weekends\n    includes 2026-01-04\n    todayMarker off\n    tickInterval 1week\n    weekday monday\n    weekend friday\n    topAxis\n    inclusiveEndDates\n    section S\n    A :a, 2026-01-01, 7d",
        // Frontmatter, an init directive and the accessibility statements.
        "---\ntitle: フロントマター\n---\n%%{init: {\"theme\": \"dark\"}}%%\ngantt\n    dateFormat YYYY-MM-DD\n    accTitle: 概要\n    accDescr {\n        説明の 1 行目\n        2 行目\n    }\n    A :a, 2026-01-01, 7d",
        // An empty section, and a nameless leading band with nothing in it is
        // never written.
        "gantt\n    dateFormat YYYY-MM-DD\n    section 空\n    section 中身あり\n    A :a, 2026-01-01, 7d",
        "gantt",
    ];

    #[test]
    fn examples_round_trip_and_render() {
        for source in EXAMPLES {
            let emitted = assert_stable(source);
            assert_renders(&emitted);
        }
    }

    #[test]
    fn the_insert_template_round_trips_byte_for_byte() {
        let document = parse_gantt(TEMPLATE).expect("the template should parse");
        assert_eq!(document.title.as_deref(), Some("開発スケジュール"));
        assert_eq!(document.date_format.as_deref(), Some("YYYY-MM-DD"));
        assert_eq!(document.axis_format.as_deref(), Some("%m/%d"));
        assert_eq!(
            document
                .sections
                .iter()
                .map(|section| (
                    section.name.as_deref(),
                    section
                        .tasks
                        .iter()
                        .map(|task| task.label.as_str())
                        .collect::<Vec<_>>()
                ))
                .collect::<Vec<_>>(),
            vec![
                (Some("設計"), vec!["要件定義", "基本設計"]),
                (Some("実装"), vec!["実装"]),
            ]
        );
        let first = document.tasks().next().expect("a first task");
        assert_eq!(first.tags, vec![GanttTag::Done]);
        assert_eq!(first.id.as_deref(), Some("req"));
        assert_eq!(first.start, GanttStart::Date("2026-01-06".to_string()));
        assert_eq!(first.end, GanttEnd::Fixed("5d".to_string()));
        let second = document.tasks().nth(1).expect("a second task");
        assert_eq!(second.start, GanttStart::After(vec!["req".to_string()]));
        assert_eq!(emit_gantt(&document), TEMPLATE);
    }

    #[test]
    fn preserved_lines_keep_their_place() {
        let source = "%%{init: {\"theme\": \"dark\"}}%%\ngantt\n    title 図\n    dateFormat YYYY-MM-DD\n    %% 先頭のメモ\n    excludes weekends\n    todayMarker off\n\n    section S\n    A :a, 2026-01-01, 7d\n    %% 末尾のメモ\n    tickInterval 1week";
        let document = parse_gantt(source).expect("source should parse");
        assert_eq!(
            document
                .preserved_lines
                .iter()
                .map(|line| (line.position, line.text.trim().to_string()))
                .collect::<Vec<_>>(),
            vec![
                (
                    PreservedPosition::Preamble,
                    "%%{init: {\"theme\": \"dark\"}}%%".to_string()
                ),
                (PreservedPosition::Top, "%% 先頭のメモ".to_string()),
                (PreservedPosition::Top, "excludes weekends".to_string()),
                (PreservedPosition::Top, "todayMarker off".to_string()),
            ]
        );
        assert_eq!(
            document
                .tasks()
                .next()
                .map(|task| task.preserved_after.clone()),
            Some(vec![
                "    %% 末尾のメモ".to_string(),
                "    tickInterval 1week".to_string()
            ])
        );

        let emitted = assert_stable(source);
        assert_eq!(
            emitted,
            "%%{init: {\"theme\": \"dark\"}}%%\ngantt\n    title 図\n    dateFormat YYYY-MM-DD\n    %% 先頭のメモ\n    excludes weekends\n    todayMarker off\n    section S\n    A :a, 2026-01-01, 7d\n    %% 末尾のメモ\n    tickInterval 1week"
        );
        assert_renders(&emitted);

        // Without a `section` line it is the first task that decides which half
        // of the chart a preserved line belongs to.
        let source = "gantt\n    %% 先頭のメモ\n    A :a, 2026-01-01, 7d\n    %% 末尾のメモ";
        let document = parse_gantt(source).expect("source should parse");
        assert_eq!(
            document
                .preserved_lines
                .iter()
                .map(|line| (line.position, line.text.trim().to_string()))
                .collect::<Vec<_>>(),
            vec![(PreservedPosition::Top, "%% 先頭のメモ".to_string())]
        );
        assert_eq!(assert_stable(source), source);

        // A line written after a `section` header belongs to that header, so
        // it is written back between the header and the first task.
        let source = "gantt\n    section S\n    %% セクションのメモ\n    A :a, 2026-01-01, 7d";
        let document = parse_gantt(source).expect("source should parse");
        assert!(document.preserved_lines.is_empty());
        assert_eq!(
            document
                .sections
                .first()
                .map(|section| section.preserved_after.clone()),
            Some(vec!["    %% セクションのメモ".to_string()])
        );
        assert_eq!(assert_stable(source), source);
    }

    #[test]
    fn a_multi_line_accessibility_block_survives() {
        let source = "gantt\n    dateFormat YYYY-MM-DD\n    accDescr {\n        説明の 1 行目\n        2 行目\n    }\n    A :a, 2026-01-01, 7d";
        let document = parse_gantt(source).expect("source should parse");
        assert_eq!(
            document
                .preserved_lines
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>(),
            vec!["    accDescr {\n        説明の 1 行目\n        2 行目\n    }"]
        );
        assert!(assert_stable(source).contains("説明の 1 行目"));
    }

    #[test]
    fn multibyte_labels_and_sections_survive() {
        let source = "gantt\n    title 日本語の題\n    dateFormat YYYY-MM-DD\n    section 設計フェーズ\n    要件定義とヒアリング :req, 2026-01-06, 5d\n    section 実装フェーズ\n    バックエンド実装 :after req, 2w";
        let emitted = assert_stable(source);
        assert_eq!(emitted, source);
        assert_renders(&emitted);

        let document = parse_gantt(source).expect("source should parse");
        assert_eq!(
            document
                .tasks()
                .map(|task| task.label.as_str())
                .collect::<Vec<_>>(),
            vec!["要件定義とヒアリング", "バックエンド実装"]
        );
    }

    /// A keyword ends where Mermaid's own scanner ends it, and the probe for it
    /// must not cut a multibyte label in half.
    #[test]
    fn a_label_is_only_a_keyword_when_mermaid_reads_it_as_one() {
        for label in [
            "sectional",
            "sectional review",
            "titles",
            "日本語",
            "50%達成",
            "A#B",
            "A;B",
            // The accessibility statements need a `:` or a `{` after the
            // word, and `displayMode` is no statement at all.
            "accTitle x",
            "accDescr x",
            "accTitles",
            "displayMode compact",
            "weekdays",
            "section会議",
        ] {
            assert!(is_valid_gantt_label(label), "{label} should be a label");
        }
        for label in [
            "title of it",
            "section two",
            "section 会議",
            // The name is followed by the space before the `:`, so a name that
            // is the keyword alone is read as the statement too.
            "title",
            "Section",
            "todayMarker",
            // `topAxis` and `click` need no whitespace after them, so a label
            // that merely opens with one is read as that statement.
            "topAxisish",
            "clicks the button",
            "clicking",
            "accTitle",
            "accDescr {x}",
            "dateFormat x",
            "weekday meeting",
            "%% メモ",
            "コロン : あり",
            "",
            " 前後の空白 ",
        ] {
            assert!(!is_valid_gantt_label(label), "{label} should be refused");
        }
    }

    #[test]
    fn constructs_that_would_be_dropped_are_refused() {
        for (source, expected) in [
            // A `click` above the task it names does nothing, and moving it
            // would give the task a link.
            (
                "gantt\n    dateFormat YYYY-MM-DD\n    click a href \"https://example.com\"\n    A :a, 2026-01-01, 7d",
                "click",
            ),
            // `displayMode` is configuration, not a statement.
            (
                "gantt\n    displayMode compact\n    dateFormat YYYY-MM-DD\n    A :a, 2026-01-01, 7d",
                "displayMode",
            ),
            // Four fields and a spec that is nothing but tags both leave
            // Mermaid with a task it cannot place.
            (
                "gantt\n    dateFormat YYYY-MM-DD\n    A :a, 2026-01-01, 7d, extra",
                "1 to 3",
            ),
            ("gantt\n    dateFormat YYYY-MM-DD\n    A :done", "1 to 3"),
            // The rest of the statement after a `#` or a `;` is thrown away.
            (
                "gantt\n    dateFormat YYYY-MM-DD\n    A :a, 2026-01-01, 7d#メモ",
                "`#`",
            ),
            (
                "gantt\n    dateFormat YYYY-MM-DD;x\n    A :a, 2026-01-01, 7d",
                "`#`",
            ),
            // `until` belongs to the end field and `after` to the start one;
            // the other way round Mermaid reads a zero length task or today.
            (
                "gantt\n    dateFormat YYYY-MM-DD\n    A :a, 2026-01-01, 7d\n    B :b, until a, 3d",
                "until",
            ),
            (
                "gantt\n    dateFormat YYYY-MM-DD\n    A :a, 2026-01-01, 7d\n    B :b, 2026-01-02, after a",
                "after",
            ),
            // An `after` that names nothing that exists silently becomes today.
            (
                "gantt\n    dateFormat YYYY-MM-DD\n    A :a, 2026-01-01, 7d\n    B :b, after nope, 3d",
                "nope",
            ),
            // A section with no name runs into the statement after it.
            (
                "gantt\n    dateFormat YYYY-MM-DD\n    section \n    A :a, 2026-01-01, 7d",
                "section",
            ),
            // An id something points at, spelled in a way that `after` stops
            // reading before the end of. The same id on a task nothing names is
            // allowed, because Mermaid never reads it.
            (
                "gantt\n    dateFormat YYYY-MM-DD\n    A :タスク, 2026-01-01, 7d\n    B :b, after タスク, 3d",
                "letters, digits",
            ),
            // The first task has nothing to follow.
            (
                "gantt\n    dateFormat YYYY-MM-DD\n    A :7d",
                "The first task",
            ),
            // A line that is neither a statement nor a task.
            (
                "gantt\n    dateFormat YYYY-MM-DD\n    なにか",
                "Could not read",
            ),
            // Mermaid keeps the last of two header statements, so writing the
            // chart back would drop the line the user can still see.
            (
                "gantt\n    title 図\n    title 案\n    A :a, 2026-01-01, 7d",
                "`title` is written twice",
            ),
            (
                "gantt\n    title 図\n    title \n    A :a, 2026-01-01, 7d",
                "`title` is written twice",
            ),
            (
                "gantt\n    dateFormat YYYY-MM-DD\n    dateFormat YYYY/MM/DD\n    A :a, 2026-01-01, 7d",
                "`dateFormat` is written twice",
            ),
            // Something written after the diagram keyword.
            (
                "gantt title 図\n    dateFormat YYYY-MM-DD\n    A :a, 2026-01-01, 7d",
                "on the `gantt` line",
            ),
        ] {
            let error = parse_gantt(source)
                .err()
                .unwrap_or_else(|| panic!("`{source}` should be refused"));
            let message = format!("{error}");
            assert!(
                message.contains(expected),
                "the reason for `{source}` should mention {expected}: {message}"
            );
            assert_ne!(
                japanese_reason(source).as_deref(),
                Some(message.as_str()),
                "the message should be the editor's own sentence for `{source}`"
            );
        }
    }

    /// The reason `source` is refused, read with the Japanese catalog. A
    /// sentence the editor wrote goes through the catalog and so changes with
    /// the language, while an error `merman` raised would come back unchanged.
    fn japanese_reason(source: &str) -> Option<String> {
        let _japanese = crate::mermaid_model::japanese_for_this_thread();
        parse_gantt(source).err().map(|error| error.to_string())
    }

    /// The drawing with `merman`'s own id blanked out, so that two renderings
    /// of the same chart compare equal.
    fn drawing_of(source: &str) -> String {
        let svg = render_to_svg(source, &MermaidTheme::default())
            .unwrap_or_else(|error| panic!("`{source}` should render: {error}"));
        let mut out = String::new();
        let mut rest = svg.as_str();
        while let Some(at) = rest.find("merman-") {
            out.push_str(rest.get(..at).unwrap_or_default());
            out.push_str("merman-x");
            rest = rest.get(at + "merman-".len()..).unwrap_or_default();
            rest = rest.trim_start_matches(|character: char| character.is_ascii_digit());
        }
        out.push_str(rest);
        out
    }

    /// The whole reason a statement this form does not show can be written back
    /// above the tasks rather than where it was: Mermaid resolves the tasks
    /// after the last line has been read, so none of these statements care
    /// where in the body they sit. If that ever stops being true, moving them
    /// would silently redraw the chart.
    #[test]
    fn a_header_statement_draws_the_same_wherever_it_sits() {
        for statement in [
            "excludes 2026-01-07",
            "includes 2026-01-10",
            "todayMarker off",
            "tickInterval 1week",
            "weekday monday",
            "weekend friday",
            "topAxis",
            "inclusiveEndDates",
            "dateFormat YYYY-MM-DD",
            "axisFormat %d",
            "title 図",
            "accTitle: 概要",
            "%% メモ",
        ] {
            let tasks = "    section S\n    A :a, 2026-01-05, 7d\n    B :b, after a, 3d";
            let above = format!("gantt\n    dateFormat YYYY-MM-DD\n    {statement}\n{tasks}");
            let below = format!("gantt\n    dateFormat YYYY-MM-DD\n{tasks}\n    {statement}");
            assert_eq!(
                drawing_of(&above),
                drawing_of(&below),
                "`{statement}` changed the drawing by moving"
            );
        }

        // A note written between the tasks comes back where it was.
        let source = "gantt\n    dateFormat YYYY-MM-DD\n    A :a, 2026-01-05, 7d\n    %% メモ\n    excludes 2026-01-07\n    B :b, after a, 3d";
        let emitted = assert_stable(source);
        assert_eq!(emitted, source);
        assert_eq!(drawing_of(source), drawing_of(&emitted));
    }

    /// Mermaid resolves the tasks in several passes, so `after` may name a task
    /// written below it; a ring of them resolves in none of the passes and is
    /// refused rather than opened on a chart that cannot be drawn.
    #[test]
    fn a_reference_may_point_forwards_but_not_round_in_a_ring() {
        let forwards =
            "gantt\n    dateFormat YYYY-MM-DD\n    A :a, after b, 3d\n    B :b, 2026-01-01, 4d";
        assert_eq!(assert_stable(forwards), forwards);
        assert_renders(forwards);

        for source in [
            "gantt\n    dateFormat YYYY-MM-DD\n    A :a, after b, 3d\n    B :b, after a, 4d",
            "gantt\n    dateFormat YYYY-MM-DD\n    A :a, after a, 3d",
            "gantt\n    dateFormat YYYY-MM-DD\n    A :a, after b, 3d\n    B :b, 2026-01-01, until a",
        ] {
            let error = parse_gantt(source)
                .err()
                .unwrap_or_else(|| panic!("`{source}` should be refused"));
            assert_ne!(
                japanese_reason(source),
                Some(error.to_string()),
                "the reason for `{source}` should be the editor's own sentence"
            );
        }
    }

    /// The shapes a hand written chart reaches for that no other test covers.
    #[test]
    fn the_spellings_a_hand_written_chart_reaches_for_survive() {
        for source in [
            // Several references on one field, and both fields relative at once.
            "gantt\n    dateFormat YYYY-MM-DD\n    A :a, 2026-01-01, 3d\n    B :b, 2026-01-02, 4d\n    C :c, after a b, until a b",
            // A zero length milestone and every duration unit.
            "gantt\n    dateFormat YYYY-MM-DD\n    M :milestone, m, 2026-01-08, 0d\n    W :w, 2026-01-08, 1w\n    H :h, 2026-01-08, 2h\n    N :n, 2026-01-08, 30m",
            // A date with a time in it, and a format with a space.
            "gantt\n    dateFormat YYYY-MM-DD HH:mm\n    A :a, 2026-01-06 10:00, 2h",
            // `#` and `;` in a task name: Mermaid cuts a task's *fields* at
            // them, but keeps them in the name.
            "gantt\n    dateFormat YYYY-MM-DD\n    50%達成#1;2 :a, 2026-01-01, 3d",
            // A section name with a colon and one padded with an ideographic
            // space, which Mermaid trims the same way this model does.
            "gantt\n    dateFormat YYYY-MM-DD\n    section 設計:第一期\n    A :a, 2026-01-01, 3d",
        ] {
            let emitted = assert_stable(source);
            assert_renders(&emitted);
        }
        assert_eq!(
            assert_stable(
                "gantt\n    dateFormat YYYY-MM-DD\n    section 設計\u{3000}\n    A :a, 2026-01-01, 3d"
            ),
            "gantt\n    dateFormat YYYY-MM-DD\n    section 設計\n    A :a, 2026-01-01, 3d"
        );

        // A separator Mermaid would only read half of, and a tag word as an id.
        for (source, expected) in [
            (
                "gantt\n    dateFormat YYYY-MM-DD\n    A :a, 2026-01-01, 3d\n    B :b, after a\tzzz, 2d",
                "after",
            ),
            (
                "gantt\n    dateFormat YYYY-MM-DD\n    A :a, 2026-01-01, 3d\n    B :b, after a\u{00a0}zzz, 2d",
                "after",
            ),
        ] {
            let error = parse_gantt(source)
                .err()
                .unwrap_or_else(|| panic!("`{source}` should be refused"));
            assert!(format!("{error}").contains(expected), "{error}");
        }
    }

    /// A task may be called anything a field can hold, because Mermaid never
    /// reads the id of a task nothing points at. Only the ids an `after` or an
    /// `until` has to address are narrower, because Mermaid stops reading a
    /// reference at the first character outside `[A-Za-z0-9_- ]`.
    #[test]
    fn an_id_only_has_to_be_spellable_until_something_points_at_it() {
        let source = "gantt\n    dateFormat YYYY-MM-DD\n    要件定義 :要件, 2026-01-06, 5d\n    基本設計 :設計 1, 2026-01-12, 7d";
        let emitted = assert_stable(source);
        assert_eq!(emitted, source);
        assert_renders(&emitted);
        let document = parse_gantt(source).expect("source should parse");
        assert_eq!(document.task_ids(), vec!["要件", "設計 1"]);

        // The same chart with one of those ids pointed at.
        let referenced = "gantt\n    dateFormat YYYY-MM-DD\n    要件定義 :要件, 2026-01-06, 5d\n    基本設計 :設計, after 要件, 7d";
        let error = parse_gantt(referenced)
            .err()
            .unwrap_or_else(|| panic!("`{referenced}` should be refused"));
        assert!(format!("{error}").contains("`after`"), "{error}");

        // And reached from the form, where a reference is typed rather than
        // parsed, the same chart is stopped before it is written.
        let mut document = parse_gantt(source).expect("source should parse");
        assert_eq!(document.structure_error(), None);
        if let Some(task) = document
            .sections
            .first_mut()
            .and_then(|section| section.tasks.get_mut(1))
        {
            task.start = GanttStart::After(vec!["要件".to_string()]);
        }
        let error = document
            .structure_error()
            .expect("a Japanese id may not be pointed at");
        assert!(error.contains("letters, digits"), "{error}");

        // A field separator, a statement cut and a tag word stay refused.
        for id in ["", "a,b", "a#b", "a;b", " a ", "done", "milestone"] {
            assert!(!is_valid_gantt_task_id(id), "`{id}` should be refused");
        }
        for id in ["要件", "設計 1", "Done", "step-1_a", "a:b"] {
            assert!(is_valid_gantt_task_id(id), "`{id}` should be allowed");
        }
        assert!(!is_valid_gantt_id("要件"));
        assert!(!is_valid_gantt_id("設計 1"));
    }

    #[test]
    fn other_diagram_types_are_refused() {
        for source in [
            "flowchart TD\n  A --> B",
            "pie title x\n  \"a\" : 1",
            "erDiagram\n  A ||--o{ B : x",
        ] {
            assert!(!is_gantt_source(source), "{source}");
            assert!(parse_gantt(source).is_err(), "{source}");
        }
        assert!(is_gantt_source("gantt\n  A :a, 2026-01-01, 7d"));
        assert!(is_gantt_source("%% メモ\ngantt"));
        assert!(is_gantt_source("---\ntitle: x\n---\ngantt"));
        assert!(!is_gantt_source("ganttish\n  A"));
    }

    #[test]
    fn an_empty_fence_opens_as_a_blank_chart() {
        let document = parse_gantt("").expect("an empty fence should parse");
        assert_eq!(document, GanttDocument::empty());
        assert_eq!(emit_gantt(&document), "gantt");
        assert_eq!(emit_gantt(&GanttDocument::empty()), "gantt");
        assert_eq!(GanttDocument::empty().structure_error(), None);
    }

    #[test]
    fn a_value_keeps_the_spelling_it_was_written_with() {
        let source = "gantt\n    dateFormat YYYY-MM-DD HH:mm\n    A :a, 2026-01-06 09:00, 1.5d\n    B :b, 2026-01-06, 0d";
        let document = parse_gantt(source).expect("source should parse");
        assert_eq!(
            document
                .tasks()
                .map(|task| (task.start.clone(), task.end.clone()))
                .collect::<Vec<_>>(),
            vec![
                (
                    GanttStart::Date("2026-01-06 09:00".to_string()),
                    GanttEnd::Fixed("1.5d".to_string())
                ),
                (
                    GanttStart::Date("2026-01-06".to_string()),
                    GanttEnd::Fixed("0d".to_string())
                ),
            ]
        );
        assert_eq!(emit_gantt(&document), source);
    }

    #[test]
    fn the_case_and_spacing_of_a_statement_are_normalised_once() {
        let source = "gantt\n\tDATEFORMAT   YYYY-MM-DD  \n  SECTION  設計  \n  A   :  a ,  2026-01-01 ,  7d  ";
        let emitted = assert_stable(source);
        assert_eq!(
            emitted,
            "gantt\n    dateFormat YYYY-MM-DD\n    section 設計\n    A :a, 2026-01-01, 7d"
        );
    }

    // Mermaid 11.16.1 reads a `title` with nothing after it; merman 0.8 refuses
    // it ("unrecognized statement: title"), so the diagram does not open.
    #[test]
    #[ignore = "merman 0.8: an empty gantt `title` is refused though Mermaid accepts it"]
    fn a_title_with_nothing_after_it_is_no_title_at_all() {
        let document = parse_gantt("gantt\n    title \n    A :a, 2026-01-01, 7d")
            .expect("source should parse");
        assert_eq!(document.title, None);
        assert_eq!(emit_gantt(&document), "gantt\n    A :a, 2026-01-01, 7d");
    }

    #[test]
    fn a_repeated_tag_is_written_once() {
        let document = parse_gantt("gantt\n    A :done, done, crit, a, 2026-01-01, 7d")
            .expect("source should parse");
        let task = document.tasks().next().expect("a task");
        assert_eq!(task.tags, vec![GanttTag::Done, GanttTag::Crit]);
        assert_eq!(
            emit_gantt(&document),
            "gantt\n    A :done, crit, a, 2026-01-01, 7d"
        );
    }

    /// Removing a task has to leave the tasks that pointed at it pointing at
    /// something that exists, and it must not disturb the task beside it.
    #[test]
    fn removing_a_task_follows_the_references_before_thinning_them() {
        // `B` ends on a date, so `after b` becomes that date exactly.
        let mut document = parse_gantt(
            "gantt\n    dateFormat YYYY-MM-DD\n    A :a, 2026-01-01, 3d\n    B :b, after a, 2026-01-20\n    C :c, after b, 5d",
        )
        .expect("source should parse");
        let removed = document
            .remove_task(0, 1)
            .expect("removable")
            .expect("the second task should be removed");
        assert_eq!(removed.id.as_deref(), Some("b"));
        assert_eq!(
            emit_gantt(&document),
            "gantt\n    dateFormat YYYY-MM-DD\n    A :a, 2026-01-01, 3d\n    C :c, 2026-01-20, 5d"
        );
        assert_eq!(document.structure_error(), None);

        // `B` ends after a duration, which is worked out into the date it
        // ended on.
        let mut document = parse_gantt(
            "gantt\n    dateFormat YYYY-MM-DD\n    A :a, 2026-01-01, 3d\n    B :b, after a, 4d\n    C :c, after b, 5d",
        )
        .expect("source should parse");
        document
            .remove_task(0, 1)
            .expect("removable")
            .expect("there is such a task");
        assert_eq!(
            emit_gantt(&document),
            "gantt\n    dateFormat YYYY-MM-DD\n    A :a, 2026-01-01, 3d\n    C :c, 2026-01-08, 5d"
        );
        assert_eq!(document.structure_error(), None);

        // `B` ended after `A`, so dropping it from the list would pull `C`
        // forwards; the moment `B` ended is written down instead. The case
        // where the rest of the list still means the same is in
        // `gantt2_tests`.
        let mut document = parse_gantt(
            "gantt\n    dateFormat YYYY-MM-DD\n    A :a, 2026-01-01, 3d\n    B :b, 2026-01-02, 4d\n    C :c, after a b, 5d",
        )
        .expect("source should parse");
        document
            .remove_task(0, 1)
            .expect("removable")
            .expect("there is such a task");
        assert_eq!(
            emit_gantt(&document),
            "gantt\n    dateFormat YYYY-MM-DD\n    A :a, 2026-01-01, 3d\n    C :c, 2026-01-06, 5d"
        );

        // `until` follows the removed task's start.
        let mut document = parse_gantt(
            "gantt\n    dateFormat YYYY-MM-DD\n    A :a, 2026-01-01, 3d\n    B :b, 2026-02-01, 4d\n    C :c, 2026-01-05, until b",
        )
        .expect("source should parse");
        document
            .remove_task(0, 1)
            .expect("removable")
            .expect("there is such a task");
        assert_eq!(
            emit_gantt(&document),
            "gantt\n    dateFormat YYYY-MM-DD\n    A :a, 2026-01-01, 3d\n    C :c, 2026-01-05, 2026-02-01"
        );
        assert_eq!(document.structure_error(), None);
    }

    /// A whole section goes the same way, including a chain of references that
    /// runs through several of the tasks being removed.
    #[test]
    fn removing_a_section_dissolves_a_chain_of_references() {
        let mut document = parse_gantt(
            "gantt\n    dateFormat YYYY-MM-DD\n    section 前半\n    A :a, 2026-01-01, 3d\n    B :b, after a, 4d\n    C :c, after b, 5d\n    section 後半\n    D :d, after c, 6d",
        )
        .expect("source should parse");
        let removed = document
            .remove_section(0)
            .expect("removable")
            .expect("the first section should be removed");
        assert_eq!(removed.name.as_deref(), Some("前半"));
        // `C` ended 3 + 4 + 5 days after the first of January.
        assert_eq!(
            emit_gantt(&document),
            "gantt\n    dateFormat YYYY-MM-DD\n    section 後半\n    D :d, 2026-01-13, 6d"
        );
        assert_eq!(document.structure_error(), None);
        assert_renders(&emit_gantt(&document));
    }

    /// Mermaid resolves `after x` against the *last* task spelled `x`, so a
    /// document with the same id twice cannot be edited structurally: deleting
    /// or renaming either of them rewrites references that were never pointing
    /// at it.
    #[test]
    fn the_same_id_on_two_tasks_is_refused() {
        let source = "gantt\n    dateFormat YYYY-MM-DD\n    A :a, 2026-01-01, 3d\n    B :a, 2026-02-01, 4d\n    C :c, after a, 5d";
        let error = parse_gantt(source)
            .err()
            .unwrap_or_else(|| panic!("`{source}` should be refused"));
        let message = format!("{error}");
        assert!(message.contains("`a`"), "{message}");
        assert!(message.contains("is used by two tasks"), "{message}");

        // The same document reached from the form — a rename that lands on an
        // id another task already carries — is stopped before it is written.
        let mut document = parse_gantt(
            "gantt\n    dateFormat YYYY-MM-DD\n    A :a, 2026-01-01, 3d\n    B :b, 2026-02-01, 4d\n    C :c, after a, 5d",
        )
        .expect("source should parse");
        assert_eq!(document.structure_error(), None);
        if let Some(task) = document
            .sections
            .first_mut()
            .and_then(|section| section.tasks.get_mut(1))
        {
            task.id = Some("a".to_string());
        }
        let error = document
            .structure_error()
            .expect("two tasks may not share an id");
        assert!(error.contains("`a`"), "{error}");
    }

    #[test]
    fn removing_an_index_that_is_not_there_changes_nothing() {
        let source = "gantt\n    dateFormat YYYY-MM-DD\n    A :a, 2026-01-01, 3d";
        let mut document = parse_gantt(source).expect("source should parse");
        assert_eq!(document.remove_task(0, 1).expect("nothing to refuse"), None);
        assert_eq!(document.remove_task(9, 0).expect("nothing to refuse"), None);
        assert_eq!(document.remove_section(9).expect("nothing to refuse"), None);
        assert_eq!(emit_gantt(&document), source);
    }

    #[test]
    fn a_duration_is_told_apart_from_a_date() {
        for text in ["5d", "1w", "0d", "1.5d", "12h", "300ms", "2M", "3y", "90s"] {
            assert!(is_gantt_duration(text), "{text} should be a duration");
        }
        for text in [
            "2026-01-06",
            "",
            "d",
            "5days",
            "5",
            "1.d",
            "-1d",
            "５d",
            "2026-01-06 09:00",
        ] {
            assert!(!is_gantt_duration(text), "{text} should not be a duration");
        }
    }

    #[test]
    fn values_ids_and_names_are_validated() {
        assert!(is_valid_gantt_id("req"));
        assert!(is_valid_gantt_id("step-1_a"));
        assert!(!is_valid_gantt_id(""));
        assert!(!is_valid_gantt_id("タスク"));
        assert!(!is_valid_gantt_id("a b"));
        assert!(!is_valid_gantt_id("done"));
        assert!(!is_valid_gantt_id("milestone"));
        assert!(is_valid_gantt_id("Done"));

        assert!(is_valid_gantt_date("2026-01-06"));
        assert!(is_valid_gantt_date("5d"));
        assert!(is_valid_gantt_date("2026-01-06 09:00"));
        assert!(!is_valid_gantt_date(""));
        assert!(!is_valid_gantt_date("2026-01-06, 5d"));
        assert!(!is_valid_gantt_date("5d ;x"));
        assert!(!is_valid_gantt_date("after a"));
        assert!(!is_valid_gantt_date("until a"));
        assert!(is_valid_gantt_date("afternoon"));

        assert!(is_valid_gantt_section_name("設計"));
        assert!(!is_valid_gantt_section_name(""));
        assert!(!is_valid_gantt_section_name(" 設計 "));

        assert!(is_valid_gantt_title("図"));
        assert!(!is_valid_gantt_title(""));

        assert!(is_valid_gantt_format("YYYY-MM-DD"));
        assert!(is_valid_gantt_format("%m/%d"));
        assert!(!is_valid_gantt_format(""));
        assert!(!is_valid_gantt_format("YYYY;MM"));
    }

    #[test]
    fn structure_errors_name_what_is_wrong() {
        let mut document =
            parse_gantt("gantt\n    A :a, 2026-01-01, 7d\n    B :2d").expect("source should parse");
        assert_eq!(document.structure_error(), None);

        // A second section with no name would run into the one above it.
        document.sections.push(GanttSection {
            name: None,
            tasks: Vec::new(),
            preserved_after: Vec::new(),
        });
        let error = document
            .structure_error()
            .expect("a nameless second section");
        assert!(error.contains("Every section after the first"), "{error}");
        document.sections.pop();

        // An id needs a start of its own, because Mermaid counts the fields.
        if let Some(task) = document.sections[0].tasks.get_mut(1) {
            task.id = Some("b".to_string());
        }
        let error = document.structure_error().expect("an id without a start");
        assert!(error.contains("ID"), "{error}");
    }

    #[test]
    fn crlf_and_trailing_whitespace_are_tolerated() {
        let source = "gantt\r\n    dateFormat YYYY-MM-DD\r\n    section S   \r\n    A :a, 2026-01-01, 7d   \r\n";
        let document = parse_gantt(source).expect("source should parse");
        assert_eq!(document.date_format.as_deref(), Some("YYYY-MM-DD"));
        assert_eq!(
            emit_gantt(&document),
            "gantt\n    dateFormat YYYY-MM-DD\n    section S\n    A :a, 2026-01-01, 7d"
        );
    }

    #[test]
    fn statements_merman_reads_around_a_directive_are_refused() {
        crate::mermaid_model::preprocessing_cases::assert_directive_rules(
            "gantt\n    dateFormat YYYY-MM-DD",
            "task :a, 2024-01-01, 1d",
            "task %%{init: {}}%% :a, 2024-01-01, 1d",
        );
    }

    #[test]
    fn text_that_would_open_a_directive_cannot_be_written() {
        assert!(!is_valid_gantt_label("a %%{x}%% b"));
        assert!(!is_valid_gantt_section_name("a %%{x}%% b"));
        assert!(!is_valid_gantt_title("a %%{x}%% b"));
        assert!(is_valid_gantt_label("50% 完了"));

        let mut document = parse_gantt(
            "gantt\n    dateFormat YYYY-MM-DD\n    section S\n    task :a, 2024-01-01, 1d",
        )
        .expect("the chart should open");
        document.sections[0].name = Some("S %%{x}%%".to_string());
        assert!(document.structure_error().is_some());
    }
}
