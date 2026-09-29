//! The XY chart half of the Mermaid editor's form.
//!
//! An XY chart is a title, two axes and a list of series, each a row of
//! numbers that lines up with the x axis's categories. The form shows the
//! numbers as one table with a row per category and a column per series, so
//! that adding, removing, renaming or moving a category carries every series'
//! value for it along. The table runs down the form rather than across it,
//! because a chart usually has many more categories than series and the form
//! is narrower than it is tall.
//!
//! A series may have more values than there are categories (Mermaid leaves
//! them out) or fewer (Mermaid draws gaps); the extra rows are shown without a
//! category, and a blank cell at the end of a column is a value left out.

use std::rc::Rc;

use editor::Editor;
use gpui::{AnyElement, App, Context, Entity, Focusable as _, Rems, Window};
use mermaid_render::flowchart_model::{PreservedLine, PreservedPosition};
use mermaid_render::xychart_model::{
    XyChartDocument, XyOrientation, XyRange, XySeries, XySeriesKind, XyText, XyTextStyle,
    XyXAxisData, is_valid_xychart_number, is_valid_xychart_text,
};
use ui::{ContextMenu, DropdownMenu, DropdownStyle, IconPosition, prelude::*};
use util::ResultExt as _;

use super::{
    Form, LABEL_FIELD_WIDTH, MermaidEditor, NUMBER_FIELD_WIDTH, add_button, field_frame, neighbour,
    row_controls, section_title,
};

/// What new rows and columns are filled with, so that they show in the
/// drawing before anything has been typed into them.
const NEW_VALUE: &str = "0";

fn new_category(number: usize) -> String {
    i18n::t!("Category {number}")
}

/// What the x axis is divided into, which decides whether the table's rows
/// carry a category.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum XMode {
    None,
    Categories,
    Range,
}

impl XMode {
    const ALL: [XMode; 3] = [XMode::Categories, XMode::Range, XMode::None];

    fn label(self) -> &'static str {
        match self {
            Self::Categories => i18n::t!("Categories"),
            Self::Range => i18n::t!("Numeric Range"),
            Self::None => i18n::t!("None (1, 2, 3…)"),
        }
    }
}

pub(super) struct XyChartForm {
    /// The header, the texts' spellings and the preserved lines. The series
    /// and the categories are read from the rows below instead.
    pub(super) document: XyChartDocument,
    pub(super) title: Entity<Editor>,
    pub(super) x_title: Entity<Editor>,
    pub(super) x_mode: XMode,
    pub(super) x_min: Entity<Editor>,
    pub(super) x_max: Entity<Editor>,
    pub(super) y_title: Entity<Editor>,
    pub(super) y_min: Entity<Editor>,
    pub(super) y_max: Entity<Editor>,
    pub(super) series: Vec<SeriesColumn>,
    pub(super) rows: Vec<ValueRow>,
}

pub(super) struct SeriesColumn {
    pub(super) kind: XySeriesKind,
    pub(super) name: Entity<Editor>,
    name_style: XyTextStyle,
    leading_lines: Vec<String>,
}

pub(super) struct ValueRow {
    /// `None` for a row past the last category, or for every row while the x
    /// axis has no categories.
    pub(super) category: Option<CategoryCell>,
    /// One field per series, in the order of [`XyChartForm::series`].
    pub(super) cells: Vec<Entity<Editor>>,
}

pub(super) struct CategoryCell {
    pub(super) field: Entity<Editor>,
    style: XyTextStyle,
}

impl XyChartForm {
    pub(super) fn fields(&self) -> Vec<Entity<Editor>> {
        let mut fields = vec![self.title.clone(), self.x_title.clone()];
        if self.x_mode == XMode::Range {
            fields.push(self.x_min.clone());
            fields.push(self.x_max.clone());
        }
        fields.extend([self.y_title.clone(), self.y_min.clone(), self.y_max.clone()]);
        fields.extend(self.series.iter().map(|series| series.name.clone()));
        for row in &self.rows {
            if let Some(category) = &row.category {
                fields.push(category.field.clone());
            }
            fields.extend(row.cells.iter().cloned());
        }
        fields
    }

    /// The XY chart the form currently describes.
    pub(super) fn current_document(&self, cx: &App) -> XyChartDocument {
        let mut document = self.document.clone();
        document.title = text_value(&self.title, style_of(self.document.title.as_ref()), cx);
        document.x_axis.title = text_value(
            &self.x_title,
            style_of(self.document.x_axis.title.as_ref()),
            cx,
        );
        document.x_axis.data = match self.x_mode {
            XMode::None => XyXAxisData::None,
            XMode::Categories => XyXAxisData::Categories(
                self.rows
                    .iter()
                    .filter_map(|row| row.category.as_ref())
                    .map(|category| XyText::new(field_text(&category.field, cx), category.style))
                    .collect(),
            ),
            XMode::Range => XyXAxisData::Range {
                min: field_text(&self.x_min, cx),
                max: field_text(&self.x_max, cx),
            },
        };
        document.y_axis.title = text_value(
            &self.y_title,
            style_of(self.document.y_axis.title.as_ref()),
            cx,
        );
        let (min, max) = (field_text(&self.y_min, cx), field_text(&self.y_max, cx));
        document.y_axis.range = if min.is_empty() && max.is_empty() {
            None
        } else {
            Some(XyRange { min, max })
        };
        document.series = self
            .series
            .iter()
            .enumerate()
            .map(|(column, series)| XySeries {
                kind: series.kind,
                name: text_value(&series.name, series.name_style, cx),
                values: self.column_values(column, cx),
                leading_lines: series.leading_lines.clone(),
            })
            .collect();
        document
    }

    /// A series' values, read down its column. Blank cells at the bottom are
    /// values the series does not have; a blank one above a value is a gap the
    /// document reports.
    fn column_values(&self, column: usize, cx: &App) -> Vec<String> {
        let mut values = self
            .rows
            .iter()
            .map(|row| {
                row.cells
                    .get(column)
                    .map(|cell| field_text(cell, cx))
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>();
        while values.last().is_some_and(String::is_empty) {
            values.pop();
        }
        values
    }

    fn category_count(&self) -> usize {
        self.rows
            .iter()
            .filter(|row| row.category.is_some())
            .count()
    }
}

/// The first thing wrong with what the user typed, in the words shown above the
/// form. Every check lives in `XyChartDocument::structure_error`, which the
/// caller consults right after this; the red frames come from the
/// `is_valid_xychart_*` predicates directly.
pub(super) fn problem(_document: &XyChartDocument) -> Option<SharedString> {
    None
}

fn field_text(field: &Entity<Editor>, cx: &App) -> String {
    field.read(cx).text(cx).trim().to_string()
}

fn style_of(text: Option<&XyText>) -> XyTextStyle {
    text.map_or(XyTextStyle::Bare, |text| text.style)
}

/// A field's text in the spelling it was opened with, or `None` when it is
/// empty, which leaves the statement out.
fn text_value(field: &Entity<Editor>, style: XyTextStyle, cx: &App) -> Option<XyText> {
    let text = field_text(field, cx);
    (!text.is_empty()).then(|| XyText::new(text, style))
}

fn optional_text_is_valid(text: Option<&XyText>) -> bool {
    text.is_none_or(is_valid_xychart_text)
}

/// Whether the row at `index` may trade places with the one beside it: a row
/// with a category only with another such row, so that the categories stay
/// ahead of the values past them.
fn row_may_move(rows: &[ValueRow], index: usize, forward: bool) -> bool {
    let Some(other) = neighbour(index, forward, rows.len()) else {
        return false;
    };
    match (rows.get(index), rows.get(other)) {
        (Some(row), Some(other)) => row.category.is_some() == other.category.is_some(),
        _ => false,
    }
}

pub(super) type Pick<T> =
    Rc<dyn Fn(&mut MermaidEditor, T, &mut Window, &mut Context<MermaidEditor>)>;

/// A dropdown of `choices` with `current` ticked, calling `on_pick` with the
/// value picked.
pub(super) fn render_choice_menu<T: Clone + PartialEq + 'static>(
    id: SharedString,
    trigger: SharedString,
    choices: Vec<(SharedString, T)>,
    current: T,
    on_pick: impl Fn(&mut MermaidEditor, T, &mut Window, &mut Context<MermaidEditor>) + 'static,
    window: &mut Window,
    cx: &mut Context<MermaidEditor>,
) -> AnyElement {
    let on_pick: Pick<T> = Rc::new(on_pick);
    let this = cx.entity().downgrade();
    let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
        for (label, value) in choices {
            let this = this.clone();
            let on_pick = on_pick.clone();
            let toggled = value == current;
            menu = menu.toggleable_entry(
                label,
                toggled,
                IconPosition::Start,
                None,
                move |window, cx| {
                    let value = value.clone();
                    let on_pick = on_pick.clone();
                    this.update(cx, |this, cx| on_pick(this, value, window, cx))
                        .log_err();
                },
            );
        }
        menu
    });
    DropdownMenu::new(id, trigger, menu)
        .style(DropdownStyle::Outlined)
        .trigger_size(ButtonSize::Compact)
        .into_any_element()
}

fn orientation_label(orientation: Option<XyOrientation>) -> &'static str {
    orientation.map_or(i18n::t!("Not Set (Vertical)"), XyOrientation::label)
}

impl MermaidEditor {
    pub(super) fn new_xychart_form(
        &mut self,
        document: XyChartDocument,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> XyChartForm {
        let text_of =
            |text: Option<&XyText>| text.map(|text| text.text.clone()).unwrap_or_default();
        let title = self.new_field(&text_of(document.title.as_ref()), window, cx);
        let x_title = self.new_field(&text_of(document.x_axis.title.as_ref()), window, cx);
        let (x_mode, x_range) = match &document.x_axis.data {
            XyXAxisData::None => (XMode::None, None),
            XyXAxisData::Categories(_) => (XMode::Categories, None),
            XyXAxisData::Range { min, max } => (XMode::Range, Some((min.clone(), max.clone()))),
        };
        let (x_min_text, x_max_text) = x_range.unwrap_or_default();
        let x_min = self.new_field(&x_min_text, window, cx);
        let x_max = self.new_field(&x_max_text, window, cx);
        let y_title = self.new_field(&text_of(document.y_axis.title.as_ref()), window, cx);
        let (y_min_text, y_max_text) = document
            .y_axis
            .range
            .as_ref()
            .map(|range| (range.min.clone(), range.max.clone()))
            .unwrap_or_default();
        let y_min = self.new_field(&y_min_text, window, cx);
        let y_max = self.new_field(&y_max_text, window, cx);

        let mut series = Vec::with_capacity(document.series.len());
        for item in &document.series {
            let name = self.new_field(&text_of(item.name.as_ref()), window, cx);
            series.push(SeriesColumn {
                kind: item.kind,
                name,
                name_style: style_of(item.name.as_ref()),
                leading_lines: item.leading_lines.clone(),
            });
        }

        let categories = document.x_axis.categories().to_vec();
        let row_count = document
            .series
            .iter()
            .map(|series| series.values.len())
            .chain([categories.len()])
            .max()
            .unwrap_or_default();
        let mut rows = Vec::with_capacity(row_count);
        for index in 0..row_count {
            let category = match categories.get(index) {
                Some(category) => Some(CategoryCell {
                    field: self.new_field(&category.text, window, cx),
                    style: category.style,
                }),
                None => None,
            };
            let mut cells = Vec::with_capacity(document.series.len());
            for item in &document.series {
                let value = item.values.get(index).cloned().unwrap_or_default();
                cells.push(self.new_field(&value, window, cx));
            }
            rows.push(ValueRow { category, cells });
        }

        XyChartForm {
            document,
            title,
            x_title,
            x_mode,
            x_min,
            x_max,
            y_title,
            y_min,
            y_max,
            series,
            rows,
        }
    }

    pub(super) fn xychart(&self) -> Option<&XyChartForm> {
        match &self.form {
            Form::XyChart(form) => Some(form),
            _ => None,
        }
    }

    pub(super) fn xychart_mut(&mut self) -> Option<&mut XyChartForm> {
        match &mut self.form {
            Form::XyChart(form) => Some(form),
            _ => None,
        }
    }

    fn xychart_changed(&mut self, cx: &mut Context<Self>) {
        self.schedule_preview(cx);
        cx.notify();
    }

    pub(super) fn set_xychart_orientation(
        &mut self,
        orientation: Option<XyOrientation>,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = self.xychart_mut() else {
            return;
        };
        form.document.orientation = orientation;
        self.xychart_changed(cx);
    }

    /// Switches what the x axis is divided into. Switching to categories names
    /// every row that has none yet, so that no value is left without one;
    /// switching away drops the names and keeps the values.
    pub(super) fn set_xychart_x_mode(
        &mut self,
        mode: XMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = self.xychart() else {
            return;
        };
        if form.x_mode == mode {
            return;
        }
        let row_count = form.rows.len();
        let series_count = form.series.len();
        let needs_range = mode == XMode::Range
            && form.x_min.read(cx).text(cx).trim().is_empty()
            && form.x_max.read(cx).text(cx).trim().is_empty();
        let (x_min, x_max) = (form.x_min.clone(), form.x_max.clone());

        let mut names = Vec::new();
        let mut first_row = Vec::new();
        if mode == XMode::Categories {
            for index in 0..row_count.max(1) {
                names.push(self.new_field(&new_category(index + 1), window, cx));
            }
            if row_count == 0 {
                for _ in 0..series_count {
                    first_row.push(self.new_field(NEW_VALUE, window, cx));
                }
            }
        }
        if needs_range {
            x_min.update(cx, |field, cx| field.set_text("1", window, cx));
            let last = row_count.max(2).to_string();
            x_max.update(cx, |field, cx| field.set_text(last, window, cx));
        }

        let Some(form) = self.xychart_mut() else {
            return;
        };
        form.x_mode = mode;
        if mode == XMode::Categories {
            if form.rows.is_empty() {
                form.rows.push(ValueRow {
                    category: None,
                    cells: first_row,
                });
            }
            for (row, field) in form.rows.iter_mut().zip(names) {
                if row.category.is_none() {
                    row.category = Some(CategoryCell {
                        field,
                        style: XyTextStyle::Quoted,
                    });
                }
            }
        } else {
            for row in &mut form.rows {
                row.category = None;
            }
        }
        self.xychart_changed(cx);
    }

    pub(super) fn add_xychart_series(
        &mut self,
        kind: XySeriesKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = self.xychart() else {
            return;
        };
        // A new series gets a value in every row that is drawn, so that it
        // shows at once; the rows past the categories stay blank.
        let drawn_rows = if form.x_mode == XMode::Categories {
            form.category_count()
        } else {
            form.rows.len().max(1)
        };
        let existing_rows = form.rows.len();
        let row_count = existing_rows.max(drawn_rows);
        let column = form.series.len();
        let name = self.new_field("", window, cx);
        let mut cells = Vec::with_capacity(row_count);
        for index in 0..row_count {
            let value = if index < drawn_rows { NEW_VALUE } else { "" };
            cells.push(self.new_field(value, window, cx));
        }
        // A row this change adds needs a blank cell for every series before it.
        let mut new_rows = Vec::new();
        for _ in existing_rows..row_count {
            let mut padding = Vec::with_capacity(column);
            for _ in 0..column {
                padding.push(self.new_field("", window, cx));
            }
            new_rows.push(ValueRow {
                category: None,
                cells: padding,
            });
        }
        let Some(form) = self.xychart_mut() else {
            return;
        };
        form.rows.extend(new_rows);
        for (row, cell) in form.rows.iter_mut().zip(cells) {
            row.cells.push(cell);
        }
        form.series.push(SeriesColumn {
            kind,
            name: name.clone(),
            name_style: XyTextStyle::Bare,
            leading_lines: Vec::new(),
        });
        name.focus_handle(cx).focus(window, cx);
        self.xychart_changed(cx);
    }

    /// Removes a series and its column. The comments written for it are kept
    /// below the chart rather than dropped with it.
    pub(super) fn remove_xychart_series(&mut self, column: usize, cx: &mut Context<Self>) {
        let Some(form) = self.xychart_mut() else {
            return;
        };
        if column >= form.series.len() {
            return;
        }
        let series = form.series.remove(column);
        for text in series.leading_lines {
            form.document.preserved_lines.push(PreservedLine {
                position: PreservedPosition::Bottom,
                text,
            });
        }
        for row in &mut form.rows {
            if column < row.cells.len() {
                row.cells.remove(column);
            }
        }
        self.xychart_changed(cx);
    }

    pub(super) fn move_xychart_series(
        &mut self,
        column: usize,
        forward: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = self.xychart_mut() else {
            return;
        };
        let Some(other) = neighbour(column, forward, form.series.len()) else {
            return;
        };
        form.series.swap(column, other);
        for row in &mut form.rows {
            if column < row.cells.len() && other < row.cells.len() {
                row.cells.swap(column, other);
            }
        }
        self.xychart_changed(cx);
    }

    pub(super) fn set_xychart_series_kind(
        &mut self,
        column: usize,
        kind: XySeriesKind,
        cx: &mut Context<Self>,
    ) {
        let Some(series) = self
            .xychart_mut()
            .and_then(|form| form.series.get_mut(column))
        else {
            return;
        };
        series.kind = kind;
        self.xychart_changed(cx);
    }

    /// Adds a row at the end of the table. With categories on the x axis the
    /// row is a new category — or, when a series has values past the last
    /// category, the first of those rows gets the name, since that is the
    /// value a new category would be drawn with.
    pub(super) fn add_xychart_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = self.xychart() else {
            return;
        };
        let with_category = form.x_mode == XMode::Categories;
        let category_count = form.category_count();
        let claims_extra_row = with_category && category_count < form.rows.len();
        let series_count = form.series.len();
        let name =
            with_category.then(|| self.new_field(&new_category(category_count + 1), window, cx));
        let mut cells = Vec::new();
        if !claims_extra_row {
            for _ in 0..series_count {
                cells.push(self.new_field("", window, cx));
            }
        }
        let Some(form) = self.xychart_mut() else {
            return;
        };
        let category = name.clone().map(|field| CategoryCell {
            field,
            style: XyTextStyle::Quoted,
        });
        if claims_extra_row {
            if let Some(row) = form.rows.get_mut(category_count) {
                row.category = category;
            }
        } else {
            form.rows.push(ValueRow { category, cells });
        }
        let focus = name.or_else(|| form.rows.last().and_then(|row| row.cells.first().cloned()));
        if let Some(field) = focus {
            field.focus_handle(cx).focus(window, cx);
        }
        self.xychart_changed(cx);
    }

    /// Removes a row: the category and every series' value for it.
    pub(super) fn remove_xychart_row(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(form) = self.xychart_mut() else {
            return;
        };
        if index >= form.rows.len() {
            return;
        }
        form.rows.remove(index);
        self.xychart_changed(cx);
    }

    pub(super) fn move_xychart_row(&mut self, index: usize, forward: bool, cx: &mut Context<Self>) {
        let Some(form) = self.xychart_mut() else {
            return;
        };
        if !row_may_move(&form.rows, index, forward) {
            return;
        }
        let Some(other) = neighbour(index, forward, form.rows.len()) else {
            return;
        };
        form.rows.swap(index, other);
        self.xychart_changed(cx);
    }

    pub(super) fn render_xychart(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(form) = self.xychart() else {
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
                        optional_text_is_valid(current.title.as_ref()),
                        window,
                        cx,
                    )
                    .flex_1()
                    .child(form.title.clone()),
                )
                .into_any_element(),
            self.render_xychart_axes(form, &current, window, cx),
            self.render_xychart_series(form, &current, window, cx),
            self.render_xychart_table(form, &current, window, cx),
        ]
    }

    fn render_xychart_axes(
        &self,
        form: &XyChartForm,
        current: &XyChartDocument,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (x_min_valid, x_max_valid) = match &current.x_axis.data {
            XyXAxisData::Range { min, max } => {
                (is_valid_xychart_number(min), is_valid_xychart_number(max))
            }
            _ => (true, true),
        };
        let mut x_row = h_flex()
            .w_full()
            .gap_1()
            .child(section_title(i18n::t!("X Axis")))
            .child(
                field_frame(
                    &form.x_title,
                    LABEL_FIELD_WIDTH,
                    optional_text_is_valid(current.x_axis.title.as_ref()),
                    window,
                    cx,
                )
                .child(form.x_title.clone()),
            )
            .child(section_title(i18n::t!("Scale")))
            .child(render_choice_menu(
                "mermaid-xy-x-mode".into(),
                form.x_mode.label().into(),
                XMode::ALL
                    .into_iter()
                    .map(|mode| (SharedString::from(mode.label()), mode))
                    .collect(),
                form.x_mode,
                |this, mode, window, cx| this.set_xychart_x_mode(mode, window, cx),
                window,
                cx,
            ));
        if form.x_mode == XMode::Range {
            x_row = x_row
                .child(
                    field_frame(&form.x_min, NUMBER_FIELD_WIDTH, x_min_valid, window, cx)
                        .child(form.x_min.clone()),
                )
                .child(Label::new("→").size(LabelSize::Small))
                .child(
                    field_frame(&form.x_max, NUMBER_FIELD_WIDTH, x_max_valid, window, cx)
                        .child(form.x_max.clone()),
                );
        }

        let (y_min_valid, y_max_valid) =
            current.y_axis.range.as_ref().map_or((true, true), |range| {
                (
                    is_valid_xychart_number(&range.min),
                    is_valid_xychart_number(&range.max),
                )
            });
        let y_row = h_flex()
            .w_full()
            .gap_1()
            .child(section_title(i18n::t!("Y Axis")))
            .child(
                field_frame(
                    &form.y_title,
                    LABEL_FIELD_WIDTH,
                    optional_text_is_valid(current.y_axis.title.as_ref()),
                    window,
                    cx,
                )
                .child(form.y_title.clone()),
            )
            .child(section_title(i18n::t!("Range")))
            .child(
                field_frame(&form.y_min, NUMBER_FIELD_WIDTH, y_min_valid, window, cx)
                    .child(form.y_min.clone()),
            )
            .child(Label::new("→").size(LabelSize::Small))
            .child(
                field_frame(&form.y_max, NUMBER_FIELD_WIDTH, y_max_valid, window, cx)
                    .child(form.y_max.clone()),
            )
            .child(
                Label::new(i18n::t!("(automatic when empty)"))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            );

        v_flex()
            .w_full()
            .gap_1()
            .child(section_title(i18n::t!(
                "Axes (an empty name is not written)"
            )))
            .child(x_row)
            .child(y_row)
            .into_any_element()
    }

    fn render_xychart_series(
        &self,
        form: &XyChartForm,
        current: &XyChartDocument,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut list = v_flex().w_full().gap_1().child(section_title(i18n::t!(
            "Series (in drawing order; names do not appear in the diagram)"
        )));
        let count = form.series.len();
        for (column, series) in form.series.iter().enumerate() {
            let name_valid = current
                .series
                .get(column)
                .is_none_or(|series| optional_text_is_valid(series.name.as_ref()));
            list = list.child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .child(
                        Label::new(format!("{}", column + 1))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(render_choice_menu(
                        SharedString::from(format!("mermaid-xy-series-kind-{column}")),
                        series.kind.label().into(),
                        XySeriesKind::ALL
                            .into_iter()
                            .map(|kind| (SharedString::from(kind.label()), kind))
                            .collect(),
                        series.kind,
                        move |this, kind, _window, cx| {
                            this.set_xychart_series_kind(column, kind, cx)
                        },
                        window,
                        cx,
                    ))
                    .child(
                        field_frame(&series.name, LABEL_FIELD_WIDTH, name_valid, window, cx)
                            .flex_1()
                            .child(series.name.clone()),
                    )
                    .child(row_controls(
                        "mermaid-xy-series",
                        column,
                        column == 0,
                        column + 1 >= count,
                        cx.listener(move |this, _, _window, cx| {
                            this.move_xychart_series(column, false, cx)
                        }),
                        cx.listener(move |this, _, _window, cx| {
                            this.move_xychart_series(column, true, cx)
                        }),
                        cx.listener(move |this, _, _window, cx| {
                            this.remove_xychart_series(column, cx)
                        }),
                    )),
            );
        }
        list.child(
            h_flex()
                .gap_1()
                .child(add_button(
                    "mermaid-xy-add-bar",
                    i18n::t!("Add Bar"),
                    cx.listener(|this, _, window, cx| {
                        this.add_xychart_series(XySeriesKind::Bar, window, cx)
                    }),
                ))
                .child(add_button(
                    "mermaid-xy-add-line",
                    i18n::t!("Add Line"),
                    cx.listener(|this, _, window, cx| {
                        this.add_xychart_series(XySeriesKind::Line, window, cx)
                    }),
                )),
        )
        .into_any_element()
    }

    fn render_xychart_table(
        &self,
        form: &XyChartForm,
        current: &XyChartDocument,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let with_categories = form.x_mode == XMode::Categories;
        let category_count = form.category_count();
        let series_count = form.series.len();
        // The scroll container needs to be told how wide its content is, or it
        // measures nothing to scroll.
        let width =
            Rems(LABEL_FIELD_WIDTH.0 + series_count as f32 * (NUMBER_FIELD_WIDTH.0 + 0.25) + 5.0);

        let mut header = h_flex()
            .gap_1()
            .child(div().w(LABEL_FIELD_WIDTH).flex_none().child(section_title(
                if with_categories {
                    i18n::t!("Category")
                } else {
                    i18n::t!("Number")
                },
            )));
        for (column, series) in form.series.iter().enumerate() {
            header = header.child(div().w(NUMBER_FIELD_WIDTH).flex_none().child(section_title(
                i18n::tc!(
                    "xychart series heading",
                    "{kind} {number}",
                    kind = series.kind.label(),
                    number = column + 1
                ),
            )));
        }

        let mut table = v_flex().w(width).gap_1().child(header);
        let last = form.rows.len().saturating_sub(1);
        for (index, row) in form.rows.iter().enumerate() {
            let leading = match &row.category {
                Some(category) => {
                    let valid = current
                        .x_axis
                        .categories()
                        .get(index)
                        .is_none_or(is_valid_xychart_text);
                    field_frame(&category.field, LABEL_FIELD_WIDTH, valid, window, cx)
                        .child(category.field.clone())
                        .into_any_element()
                }
                None => div()
                    .w(LABEL_FIELD_WIDTH)
                    .flex_none()
                    .child(
                        Label::new(if with_categories {
                            i18n::t!("(no category; not drawn)").to_string()
                        } else {
                            format!("{}", index + 1)
                        })
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    )
                    .into_any_element(),
            };
            let mut line = h_flex().gap_1().child(leading);
            for (column, cell) in row.cells.iter().enumerate() {
                let valid = current
                    .series
                    .get(column)
                    .and_then(|series| series.values.get(index))
                    .is_none_or(|value| is_valid_xychart_number(value));
                line = line.child(
                    field_frame(cell, NUMBER_FIELD_WIDTH, valid, window, cx).child(cell.clone()),
                );
            }
            line = line.child(row_controls(
                "mermaid-xy-row",
                index,
                !row_may_move(&form.rows, index, false),
                index >= last || !row_may_move(&form.rows, index, true),
                cx.listener(move |this, _, _window, cx| this.move_xychart_row(index, false, cx)),
                cx.listener(move |this, _, _window, cx| this.move_xychart_row(index, true, cx)),
                cx.listener(move |this, _, _window, cx| this.remove_xychart_row(index, cx)),
            ));
            table = table.child(line);
        }

        let mut section = v_flex()
            .w_full()
            .gap_1()
            .child(section_title(i18n::t!(
                "Values (deleting a row also deletes its value in every series)"
            )))
            .child(
                div()
                    .id("mermaid-xy-table")
                    .w_full()
                    .overflow_x_scroll()
                    .child(table),
            );
        if with_categories && category_count < form.rows.len() {
            section = section.child(
                Label::new(i18n::t!(
                    "Values beyond the number of categories are not drawn. Adding a category gives those values a name."
                ))
                .size(LabelSize::XSmall)
                .color(Color::Muted),
            );
        }
        section
            .child(add_button(
                "mermaid-xy-add-row",
                if with_categories {
                    i18n::t!("Add Category")
                } else {
                    i18n::t!("Add Row")
                },
                cx.listener(|this, _, window, cx| this.add_xychart_row(window, cx)),
            ))
            .into_any_element()
    }

    pub(super) fn render_xychart_toolbar(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(form) = self.xychart() else {
            return div().into_any_element();
        };
        let current = form.document.orientation;
        let choices = [None]
            .into_iter()
            .chain(XyOrientation::ALL.map(Some))
            .map(|orientation| {
                (
                    SharedString::from(orientation_label(orientation)),
                    orientation,
                )
            })
            .collect();
        h_flex()
            .gap_2()
            .child(section_title(i18n::t!("Orientation")))
            .child(render_choice_menu(
                "mermaid-xy-orientation".into(),
                orientation_label(current).into(),
                choices,
                current,
                |this, orientation, _window, cx| this.set_xychart_orientation(orientation, cx),
                window,
                cx,
            ))
            .into_any_element()
    }
}

#[cfg(test)]
#[path = "xychart_tests.rs"]
mod xychart_tests;
