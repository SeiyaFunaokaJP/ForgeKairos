//! The timeline half of the Mermaid editor's form.
//!
//! A timeline is a title and a list of sections, each holding periods, and a
//! period holds the events drawn below it, so the form is one group per
//! section with one block per period and one field per event. The events are
//! separate fields rather than one text box because Mermaid separates them
//! with `: `, which an event can therefore not contain; a field per event keeps
//! that rule on the one event that breaks it.

use editor::Editor;
use gpui::{AnyElement, App, Context, Entity, Focusable as _, Window};
use mermaid_render::timeline_model::{
    TimelineDocument, TimelinePeriod, TimelineSection, is_valid_timeline_event,
    is_valid_timeline_period, is_valid_timeline_section_name, is_valid_timeline_title,
};
use ui::{IconButtonShape, Tooltip, prelude::*};

use super::{
    Form, LABEL_FIELD_WIDTH, MermaidEditor, add_button, field_frame, neighbour, row_controls,
    row_controls_labelled, section_title,
};

/// The texts new rows are added with, so that they are visible in the drawing
/// before anything has been typed into them.
const NEW_PERIOD_LABEL: &str = i18n::msg!("New period");
const NEW_SECTION_NAME: &str = i18n::msg!("New section");

// A function rather than a `const` like the others: "Event" needs a context to
// keep it apart from the event modeling diagram's events.
fn new_event() -> &'static str {
    i18n::tc!("mermaid timeline", "Event")
}

pub(super) struct TimelineForm {
    pub(super) document: TimelineDocument,
    pub(super) title: Entity<Editor>,
    pub(super) sections: Vec<SectionRow>,
}

pub(super) struct SectionRow {
    pub(super) name: Entity<Editor>,
    pub(super) periods: Vec<PeriodRow>,
}

pub(super) struct PeriodRow {
    pub(super) label: Entity<Editor>,
    pub(super) events: Vec<Entity<Editor>>,
}

impl TimelineForm {
    pub(super) fn fields(&self) -> Vec<Entity<Editor>> {
        std::iter::once(self.title.clone())
            .chain(self.sections.iter().flat_map(|section| {
                std::iter::once(section.name.clone()).chain(section.periods.iter().flat_map(
                    |period| std::iter::once(period.label.clone()).chain(period.events.clone()),
                ))
            }))
            .collect()
    }

    /// The timeline the form currently describes: the structure held in
    /// `document` with the text fields' current contents written over it.
    pub(super) fn current_document(&self, cx: &App) -> TimelineDocument {
        let mut document = self.document.clone();
        document.title = field_value(&self.title, cx);
        for (section, row) in document.sections.iter_mut().zip(&self.sections) {
            section.name = field_value(&row.name, cx);
            for (period, period_row) in section.periods.iter_mut().zip(&row.periods) {
                period.label = field_text(&period_row.label, cx);
                period.events = period_row
                    .events
                    .iter()
                    .map(|field| field_text(field, cx))
                    .collect();
            }
        }
        document
    }
}

/// The first thing wrong with what the user typed, in the words shown above the
/// form. Every check lives in `TimelineDocument::structure_error`, which the
/// caller consults right after this; the red frames come from the
/// `is_valid_timeline_*` predicates directly.
pub(super) fn problem(_document: &TimelineDocument) -> Option<SharedString> {
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

/// Whether the section at `index` may trade places with the one beside it.
///
/// The band written before the first `section` line has no name, and only the
/// first band can go without one, so it is pinned to the front. The buttons and
/// the handler ask this together, so a button that is live can always be
/// pressed.
fn section_may_move(sections: &[TimelineSection], index: usize, forward: bool) -> bool {
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
    pub(super) fn new_timeline_form(
        &mut self,
        document: TimelineDocument,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> TimelineForm {
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
            let mut periods = Vec::with_capacity(section.periods.len());
            for period in &section.periods {
                let label = self.new_field(&period.label, window, cx);
                let mut events = Vec::with_capacity(period.events.len());
                for event in &period.events {
                    events.push(self.new_field(event, window, cx));
                }
                periods.push(PeriodRow { label, events });
            }
            sections.push(SectionRow { name, periods });
        }
        TimelineForm {
            document,
            title,
            sections,
        }
    }

    pub(super) fn timeline(&self) -> Option<&TimelineForm> {
        match &self.form {
            Form::Timeline(form) => Some(form),
            _ => None,
        }
    }

    pub(super) fn timeline_mut(&mut self) -> Option<&mut TimelineForm> {
        match &mut self.form {
            Form::Timeline(form) => Some(form),
            _ => None,
        }
    }

    /// Reads the fields into the document before a structural change, which
    /// moves the document and the rows in step.
    fn sync_timeline(&mut self, cx: &mut Context<Self>) {
        let Some(document) = self.timeline().map(|form| form.current_document(cx)) else {
            return;
        };
        if let Some(form) = self.timeline_mut() {
            form.document = document;
        }
    }

    fn timeline_changed(&mut self, cx: &mut Context<Self>) {
        self.schedule_preview(cx);
        cx.notify();
    }

    pub(super) fn add_timeline_section(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_timeline(cx);
        let Some(form) = self.timeline() else {
            return;
        };
        // Mermaid draws the periods of two sections with one name in both, so a
        // new one gets a name no other section has.
        let taken = form
            .document
            .sections
            .iter()
            .filter_map(|section| section.name.as_deref())
            .collect::<Vec<_>>();
        let base = i18n::tr(NEW_SECTION_NAME);
        let name = std::iter::once(base.to_string())
            .chain((2..).map(|number| format!("{base} {number}")))
            .find(|name| !taken.contains(&name.as_str()))
            .unwrap_or_else(|| base.to_string());
        let field = self.new_field(&name, window, cx);
        let Some(form) = self.timeline_mut() else {
            return;
        };
        form.document.sections.push(TimelineSection {
            name: Some(name),
            periods: Vec::new(),
        });
        form.sections.push(SectionRow {
            name: field,
            periods: Vec::new(),
        });
        self.timeline_changed(cx);
    }

    pub(super) fn remove_timeline_section(&mut self, index: usize, cx: &mut Context<Self>) {
        self.sync_timeline(cx);
        let Some(form) = self.timeline_mut() else {
            return;
        };
        if index >= form.sections.len() || index >= form.document.sections.len() {
            return;
        }
        form.document.sections.remove(index);
        form.sections.remove(index);
        self.timeline_changed(cx);
    }

    pub(super) fn move_timeline_section(
        &mut self,
        index: usize,
        forward: bool,
        cx: &mut Context<Self>,
    ) {
        self.sync_timeline(cx);
        let Some(form) = self.timeline_mut() else {
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
        self.timeline_changed(cx);
    }

    pub(super) fn add_timeline_period(
        &mut self,
        section: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sync_timeline(cx);
        let Some(form) = self.timeline() else {
            return;
        };
        // A timeline with no `section` line still draws its periods; the first
        // period added to one opens the nameless band they live in.
        let needs_band = form.document.sections.is_empty() && section == 0;
        if !needs_band && section >= form.document.sections.len() {
            return;
        }
        let band = needs_band.then(|| self.new_field("", window, cx));
        let label = self.new_field(i18n::tr(NEW_PERIOD_LABEL), window, cx);
        let event = self.new_field(new_event(), window, cx);
        let Some(form) = self.timeline_mut() else {
            return;
        };
        if let Some(name) = band {
            form.document.sections.push(TimelineSection {
                name: None,
                periods: Vec::new(),
            });
            form.sections.push(SectionRow {
                name,
                periods: Vec::new(),
            });
        }
        let (Some(periods), Some(rows)) = (
            form.document.sections.get_mut(section),
            form.sections.get_mut(section),
        ) else {
            return;
        };
        periods.periods.push(TimelinePeriod {
            label: i18n::tr(NEW_PERIOD_LABEL).to_string(),
            events: vec![new_event().to_string()],
        });
        rows.periods.push(PeriodRow {
            label: label.clone(),
            events: vec![event],
        });
        label.focus_handle(cx).focus(window, cx);
        self.timeline_changed(cx);
    }

    pub(super) fn remove_timeline_period(
        &mut self,
        section: usize,
        index: usize,
        cx: &mut Context<Self>,
    ) {
        self.sync_timeline(cx);
        let Some(form) = self.timeline_mut() else {
            return;
        };
        let (Some(periods), Some(rows)) = (
            form.document.sections.get_mut(section),
            form.sections.get_mut(section),
        ) else {
            return;
        };
        if index >= periods.periods.len() || index >= rows.periods.len() {
            return;
        }
        periods.periods.remove(index);
        rows.periods.remove(index);
        self.timeline_changed(cx);
    }

    /// Moves a period within its section, or into the section next to it when
    /// it is already at the end it is moving towards.
    pub(super) fn move_timeline_period(
        &mut self,
        section: usize,
        index: usize,
        forward: bool,
        cx: &mut Context<Self>,
    ) {
        self.sync_timeline(cx);
        let Some(form) = self.timeline_mut() else {
            return;
        };
        let length = match (
            form.document.sections.get(section),
            form.sections.get(section),
        ) {
            (Some(periods), Some(rows)) if periods.periods.len() == rows.periods.len() => {
                periods.periods.len()
            }
            _ => return,
        };
        if index >= length {
            return;
        }
        if let Some(other) = neighbour(index, forward, length) {
            if let Some(periods) = form.document.sections.get_mut(section) {
                periods.periods.swap(index, other);
            }
            if let Some(rows) = form.sections.get_mut(section) {
                rows.periods.swap(index, other);
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
            let Some(period) = form
                .document
                .sections
                .get_mut(section)
                .map(|periods| periods.periods.remove(index))
            else {
                return;
            };
            let Some(row) = form
                .sections
                .get_mut(section)
                .map(|rows| rows.periods.remove(index))
            else {
                return;
            };
            if let (Some(periods), Some(rows)) = (
                form.document.sections.get_mut(target),
                form.sections.get_mut(target),
            ) {
                if forward {
                    periods.periods.insert(0, period);
                    rows.periods.insert(0, row);
                } else {
                    periods.periods.push(period);
                    rows.periods.push(row);
                }
            }
        }
        self.timeline_changed(cx);
    }

    pub(super) fn add_timeline_event(
        &mut self,
        section: usize,
        period: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sync_timeline(cx);
        if self
            .timeline()
            .and_then(|form| form.sections.get(section)?.periods.get(period))
            .is_none()
        {
            return;
        }
        let field = self.new_field(new_event(), window, cx);
        let Some(form) = self.timeline_mut() else {
            return;
        };
        let (Some(document_period), Some(row)) = (
            form.document
                .sections
                .get_mut(section)
                .and_then(|section| section.periods.get_mut(period)),
            form.sections
                .get_mut(section)
                .and_then(|section| section.periods.get_mut(period)),
        ) else {
            return;
        };
        document_period.events.push(new_event().to_string());
        row.events.push(field.clone());
        field.focus_handle(cx).focus(window, cx);
        self.timeline_changed(cx);
    }

    pub(super) fn remove_timeline_event(
        &mut self,
        section: usize,
        period: usize,
        event: usize,
        cx: &mut Context<Self>,
    ) {
        self.sync_timeline(cx);
        let Some(form) = self.timeline_mut() else {
            return;
        };
        let (Some(document_period), Some(row)) = (
            form.document
                .sections
                .get_mut(section)
                .and_then(|section| section.periods.get_mut(period)),
            form.sections
                .get_mut(section)
                .and_then(|section| section.periods.get_mut(period)),
        ) else {
            return;
        };
        if event >= document_period.events.len() || event >= row.events.len() {
            return;
        }
        document_period.events.remove(event);
        row.events.remove(event);
        self.timeline_changed(cx);
    }

    pub(super) fn render_timeline(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(form) = self.timeline() else {
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
                            .is_none_or(|title| is_valid_timeline_title(title)),
                        window,
                        cx,
                    )
                    .flex_1()
                    .child(form.title.clone()),
                )
                .into_any_element(),
            self.render_timeline_sections(form, &current, window, cx),
        ]
    }

    fn render_timeline_sections(
        &self,
        form: &TimelineForm,
        current: &TimelineDocument,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut list = v_flex()
            .w_full()
            .gap_2()
            .child(section_title(i18n::t!("Sections and Periods")));

        for index in 0..form.sections.len() {
            let (Some(row), Some(section)) =
                (form.sections.get(index), current.sections.get(index))
            else {
                continue;
            };
            let mut periods = v_flex().w_full().gap_2().pl_4();
            for period_index in 0..row.periods.len() {
                let (Some(period_row), Some(period)) = (
                    row.periods.get(period_index),
                    section.periods.get(period_index),
                ) else {
                    continue;
                };
                periods = periods.child(self.render_timeline_period(
                    index,
                    period_index,
                    row.periods.len(),
                    form.sections.len(),
                    period_row,
                    period,
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
                                        is_valid_timeline_section_name(name)
                                    }),
                                    window,
                                    cx,
                                )
                                .flex_1()
                                .child(row.name.clone()),
                            )
                            .child(row_controls_labelled(
                                "mermaid-timeline-section",
                                index,
                                !section_may_move(&current.sections, index, false),
                                !section_may_move(&current.sections, index, true),
                                i18n::t!("Delete Section and Its Periods"),
                                cx.listener(move |this, _, _window, cx| {
                                    this.move_timeline_section(index, false, cx)
                                }),
                                cx.listener(move |this, _, _window, cx| {
                                    this.move_timeline_section(index, true, cx)
                                }),
                                cx.listener(move |this, _, _window, cx| {
                                    this.remove_timeline_section(index, cx)
                                }),
                            )),
                    )
                    .child(periods)
                    .child(div().pl_4().child(add_button(
                        SharedString::from(format!("mermaid-timeline-add-period-{index}")),
                        i18n::t!("Add Period"),
                        cx.listener(move |this, _, window, cx| {
                            this.add_timeline_period(index, window, cx)
                        }),
                    ))),
            );
        }

        if form.sections.is_empty() {
            list = list.child(add_button(
                "mermaid-timeline-add-first-period",
                i18n::t!("Add Period"),
                cx.listener(|this, _, window, cx| this.add_timeline_period(0, window, cx)),
            ));
        }

        list.child(add_button(
            "mermaid-timeline-add-section",
            i18n::t!("Add Section"),
            cx.listener(|this, _, window, cx| this.add_timeline_section(window, cx)),
        ))
        .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn render_timeline_period(
        &self,
        section: usize,
        index: usize,
        length: usize,
        section_count: usize,
        row: &PeriodRow,
        period: &TimelinePeriod,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut events = v_flex().w_full().gap_1().pl_4();
        for (event_index, field) in row.events.iter().enumerate() {
            let is_valid = period
                .events
                .get(event_index)
                .is_some_and(|event| is_valid_timeline_event(event));
            events = events.child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .child(
                        field_frame(field, LABEL_FIELD_WIDTH, is_valid, window, cx)
                            .flex_1()
                            .child(field.clone()),
                    )
                    .child(
                        IconButton::new(
                            SharedString::from(format!(
                                "mermaid-timeline-event-remove-{section}-{index}-{event_index}"
                            )),
                            IconName::Trash,
                        )
                        .shape(IconButtonShape::Square)
                        .icon_size(IconSize::XSmall)
                        .tooltip(Tooltip::text(i18n::t!("Delete Event")))
                        .on_click(cx.listener(
                            move |this, _, _window, cx| {
                                this.remove_timeline_event(section, index, event_index, cx)
                            },
                        )),
                    ),
            );
        }
        events = events.child(add_button(
            SharedString::from(format!("mermaid-timeline-add-event-{section}-{index}")),
            i18n::t!("Add Event"),
            cx.listener(move |this, _, window, cx| {
                this.add_timeline_event(section, index, window, cx)
            }),
        ));

        v_flex()
            .w_full()
            .gap_1()
            .child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .child(section_title(i18n::t!("Period")))
                    .child(
                        field_frame(
                            &row.label,
                            LABEL_FIELD_WIDTH,
                            is_valid_timeline_period(&period.label),
                            window,
                            cx,
                        )
                        .flex_1()
                        .child(row.label.clone()),
                    )
                    .child(row_controls(
                        &format!("mermaid-timeline-period-{section}"),
                        index,
                        index == 0 && section == 0,
                        index + 1 >= length && section + 1 >= section_count,
                        cx.listener(move |this, _, _window, cx| {
                            this.move_timeline_period(section, index, false, cx)
                        }),
                        cx.listener(move |this, _, _window, cx| {
                            this.move_timeline_period(section, index, true, cx)
                        }),
                        cx.listener(move |this, _, _window, cx| {
                            this.remove_timeline_period(section, index, cx)
                        }),
                    )),
            )
            .child(events)
            .into_any_element()
    }
}

#[cfg(test)]
#[path = "timeline_tests.rs"]
mod timeline_tests;
