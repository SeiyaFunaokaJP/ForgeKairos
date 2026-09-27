//! The radar chart half of the Mermaid editor's form.
//!
//! A radar chart is a set of axes and a set of curves, each curve holding one
//! value per axis. The form lists the curves on their own, and shows the
//! values as one table with a row per axis and a column per curve, so that
//! adding, removing, renaming or moving an axis carries every curve's value
//! for it along. A curve written with the axis names (`{a: 1, b: 2}`) keeps
//! that form, and a renamed axis renames its key in every such curve.
//!
//! A curve may have more values than there are axes (Mermaid leaves them out)
//! or fewer (Mermaid draws the missing ones at the minimum); the extra rows
//! are shown without an axis, and a blank cell at the end of a column is a
//! value left out.

use editor::Editor;
use gpui::{AnyElement, App, Context, Entity, Focusable as _, Rems, Window};
use mermaid_render::flowchart_model::{PreservedLine, PreservedPosition};
use mermaid_render::radar_model::{
    RadarAxis, RadarCurve, RadarDocument, RadarGraticule, is_valid_radar_id, is_valid_radar_label,
    is_valid_radar_number, is_valid_radar_title,
};
use ui::prelude::*;

use super::xychart::render_choice_menu;
use super::{
    Form, ID_FIELD_WIDTH, LABEL_FIELD_WIDTH, MermaidEditor, NUMBER_FIELD_WIDTH, add_button,
    field_frame, neighbour, row_controls, section_title,
};

const NEW_VALUE: &str = "0";

pub(super) struct RadarForm {
    /// The title's and the options' current values and the preserved lines.
    /// The axes and the curves are read from the rows below instead.
    pub(super) document: RadarDocument,
    pub(super) title: Entity<Editor>,
    pub(super) ticks: Entity<Editor>,
    pub(super) max: Entity<Editor>,
    pub(super) min: Entity<Editor>,
    pub(super) curves: Vec<CurveColumn>,
    pub(super) rows: Vec<AxisRow>,
}

pub(super) struct CurveColumn {
    pub(super) id: Entity<Editor>,
    pub(super) label: Entity<Editor>,
    pub(super) keyed: bool,
    leading_lines: Vec<String>,
}

pub(super) struct AxisRow {
    /// `None` for a row past the last axis.
    pub(super) axis: Option<AxisCells>,
    /// One field per curve, in the order of [`RadarForm::curves`].
    pub(super) cells: Vec<Entity<Editor>>,
}

pub(super) struct AxisCells {
    pub(super) id: Entity<Editor>,
    pub(super) label: Entity<Editor>,
}

impl RadarForm {
    pub(super) fn fields(&self) -> Vec<Entity<Editor>> {
        let mut fields = vec![
            self.title.clone(),
            self.min.clone(),
            self.max.clone(),
            self.ticks.clone(),
        ];
        for curve in &self.curves {
            fields.push(curve.id.clone());
            fields.push(curve.label.clone());
        }
        for row in &self.rows {
            if let Some(axis) = &row.axis {
                fields.push(axis.id.clone());
                fields.push(axis.label.clone());
            }
            fields.extend(row.cells.iter().cloned());
        }
        fields
    }

    /// The radar chart the form currently describes.
    pub(super) fn current_document(&self, cx: &App) -> RadarDocument {
        let mut document = self.document.clone();
        document.title = field_value(&self.title, cx);
        document.options.ticks = field_value(&self.ticks, cx);
        document.options.max = field_value(&self.max, cx);
        document.options.min = field_value(&self.min, cx);
        document.axes = self
            .rows
            .iter()
            .filter_map(|row| row.axis.as_ref())
            .map(|axis| RadarAxis {
                id: field_text(&axis.id, cx),
                label: field_value(&axis.label, cx),
            })
            .collect();
        document.curves = self
            .curves
            .iter()
            .enumerate()
            .map(|(column, curve)| RadarCurve {
                id: field_text(&curve.id, cx),
                label: field_value(&curve.label, cx),
                values: self.column_values(column, cx),
                keyed: curve.keyed,
                leading_lines: curve.leading_lines.clone(),
            })
            .collect();
        document
    }

    /// A curve's values, read down its column. Blank cells at the bottom are
    /// values the curve does not have; a blank one above a value is a gap the
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

    fn axis_count(&self) -> usize {
        self.rows.iter().filter(|row| row.axis.is_some()).count()
    }
}

/// The first thing wrong with what the user typed, in the words shown above the
/// form. Every check lives in `RadarDocument::structure_error`, which the
/// caller consults right after this; the red frames come from the
/// `is_valid_radar_*` predicates directly.
pub(super) fn problem(_document: &RadarDocument) -> Option<SharedString> {
    None
}

fn field_text(field: &Entity<Editor>, cx: &App) -> String {
    field.read(cx).text(cx).trim().to_string()
}

/// A field's text, or `None` when it is empty, which leaves the statement or
/// the label out.
fn field_value(field: &Entity<Editor>, cx: &App) -> Option<String> {
    let text = field_text(field, cx);
    (!text.is_empty()).then_some(text)
}

fn optional_number_is_valid(value: Option<&String>) -> bool {
    value.is_none_or(|value| is_valid_radar_number(value))
}

/// Whether the row at `index` may trade places with the one beside it: an axis
/// only with another axis, so that the axes stay ahead of the values past
/// them.
fn row_may_move(rows: &[AxisRow], index: usize, forward: bool) -> bool {
    let Some(other) = neighbour(index, forward, rows.len()) else {
        return false;
    };
    match (rows.get(index), rows.get(other)) {
        (Some(row), Some(other)) => row.axis.is_some() == other.axis.is_some(),
        _ => false,
    }
}

/// The first `prefix` + number that none of `taken` is.
fn unused_id(prefix: &str, taken: &[String]) -> String {
    (1..)
        .map(|number| format!("{prefix}{number}"))
        .find(|id| !taken.contains(id))
        .unwrap_or_else(|| prefix.to_string())
}

fn graticule_label(graticule: Option<RadarGraticule>) -> &'static str {
    graticule.map_or("指定なし (円)", RadarGraticule::label)
}

fn legend_label(shown: Option<bool>) -> &'static str {
    match shown {
        None => "指定なし (表示)",
        Some(true) => "表示",
        Some(false) => "非表示",
    }
}

fn keyed_label(keyed: bool) -> &'static str {
    if keyed { "軸名付き" } else { "軸の順" }
}

impl MermaidEditor {
    pub(super) fn new_radar_form(
        &mut self,
        document: RadarDocument,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> RadarForm {
        let title = self.new_field(
            document.title.clone().unwrap_or_default().as_str(),
            window,
            cx,
        );
        let options = document.options.clone();
        let ticks = self.new_field(&options.ticks.unwrap_or_default(), window, cx);
        let max = self.new_field(&options.max.unwrap_or_default(), window, cx);
        let min = self.new_field(&options.min.unwrap_or_default(), window, cx);

        let mut curves = Vec::with_capacity(document.curves.len());
        for curve in &document.curves {
            let id = self.new_field(&curve.id, window, cx);
            let label = self.new_field(&curve.label.clone().unwrap_or_default(), window, cx);
            curves.push(CurveColumn {
                id,
                label,
                keyed: curve.keyed,
                leading_lines: curve.leading_lines.clone(),
            });
        }

        let row_count = document
            .curves
            .iter()
            .map(|curve| curve.values.len())
            .chain([document.axes.len()])
            .max()
            .unwrap_or_default();
        let mut rows = Vec::with_capacity(row_count);
        for index in 0..row_count {
            let axis = match document.axes.get(index) {
                Some(axis) => Some(AxisCells {
                    id: self.new_field(&axis.id, window, cx),
                    label: self.new_field(&axis.label.clone().unwrap_or_default(), window, cx),
                }),
                None => None,
            };
            let mut cells = Vec::with_capacity(document.curves.len());
            for curve in &document.curves {
                let value = curve.values.get(index).cloned().unwrap_or_default();
                cells.push(self.new_field(&value, window, cx));
            }
            rows.push(AxisRow { axis, cells });
        }

        RadarForm {
            document,
            title,
            ticks,
            max,
            min,
            curves,
            rows,
        }
    }

    pub(super) fn radar(&self) -> Option<&RadarForm> {
        match &self.form {
            Form::Radar(form) => Some(form),
            _ => None,
        }
    }

    pub(super) fn radar_mut(&mut self) -> Option<&mut RadarForm> {
        match &mut self.form {
            Form::Radar(form) => Some(form),
            _ => None,
        }
    }

    fn radar_changed(&mut self, cx: &mut Context<Self>) {
        self.schedule_preview(cx);
        cx.notify();
    }

    pub(super) fn set_radar_graticule(
        &mut self,
        graticule: Option<RadarGraticule>,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = self.radar_mut() else {
            return;
        };
        form.document.options.graticule = graticule;
        self.radar_changed(cx);
    }

    pub(super) fn set_radar_show_legend(&mut self, shown: Option<bool>, cx: &mut Context<Self>) {
        let Some(form) = self.radar_mut() else {
            return;
        };
        form.document.options.show_legend = shown;
        self.radar_changed(cx);
    }

    pub(super) fn set_radar_curve_keyed(
        &mut self,
        column: usize,
        keyed: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(curve) = self
            .radar_mut()
            .and_then(|form| form.curves.get_mut(column))
        else {
            return;
        };
        curve.keyed = keyed;
        self.radar_changed(cx);
    }

    /// Adds an axis at the end. When a curve has values past the last axis,
    /// the first of those rows becomes the axis, since that is the value a new
    /// axis would be drawn with.
    pub(super) fn add_radar_axis(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = self.radar() else {
            return;
        };
        let current = form.current_document(cx);
        let taken = current
            .axes
            .iter()
            .map(|axis| axis.id.clone())
            .collect::<Vec<_>>();
        let id_text = unused_id("axis", &taken);
        let label_text = format!("軸{}", taken.len() + 1);
        let axis_count = form.axis_count();
        let claims_extra_row = axis_count < form.rows.len();
        let row_index = form.rows.len();
        // A curve that has a value for every row so far gets one for the new
        // axis too; one that stops short stays short, so as not to open a gap.
        let fills = form
            .curves
            .iter()
            .enumerate()
            .map(|(column, curve)| curve.keyed || form.column_values(column, cx).len() == row_index)
            .collect::<Vec<_>>();

        let id = self.new_field(&id_text, window, cx);
        let label = self.new_field(&label_text, window, cx);
        let mut cells = Vec::new();
        if !claims_extra_row {
            for fill in fills {
                cells.push(self.new_field(if fill { NEW_VALUE } else { "" }, window, cx));
            }
        }
        let Some(form) = self.radar_mut() else {
            return;
        };
        let axis = Some(AxisCells {
            id,
            label: label.clone(),
        });
        if claims_extra_row {
            if let Some(row) = form.rows.get_mut(axis_count) {
                row.axis = axis;
            }
        } else {
            form.rows.push(AxisRow { axis, cells });
        }
        label.focus_handle(cx).focus(window, cx);
        self.radar_changed(cx);
    }

    /// Removes a row: the axis and every curve's value for it.
    pub(super) fn remove_radar_axis(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(form) = self.radar_mut() else {
            return;
        };
        if index >= form.rows.len() {
            return;
        }
        form.rows.remove(index);
        self.radar_changed(cx);
    }

    pub(super) fn move_radar_axis(&mut self, index: usize, forward: bool, cx: &mut Context<Self>) {
        let Some(form) = self.radar_mut() else {
            return;
        };
        if !row_may_move(&form.rows, index, forward) {
            return;
        }
        let Some(other) = neighbour(index, forward, form.rows.len()) else {
            return;
        };
        form.rows.swap(index, other);
        self.radar_changed(cx);
    }

    pub(super) fn add_radar_curve(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = self.radar() else {
            return;
        };
        let current = form.current_document(cx);
        let taken = current
            .curves
            .iter()
            .map(|curve| curve.id.clone())
            .collect::<Vec<_>>();
        let id_text = unused_id("curve", &taken);
        let axis_count = form.axis_count();
        let row_count = form.rows.len();

        let id = self.new_field(&id_text, window, cx);
        let label = self.new_field("", window, cx);
        let mut cells = Vec::with_capacity(row_count);
        for index in 0..row_count {
            let value = if index < axis_count { NEW_VALUE } else { "" };
            cells.push(self.new_field(value, window, cx));
        }
        let Some(form) = self.radar_mut() else {
            return;
        };
        for (row, cell) in form.rows.iter_mut().zip(cells) {
            row.cells.push(cell);
        }
        form.curves.push(CurveColumn {
            id,
            label: label.clone(),
            keyed: false,
            leading_lines: Vec::new(),
        });
        label.focus_handle(cx).focus(window, cx);
        self.radar_changed(cx);
    }

    /// Removes a curve and its column. The comments written for it are kept
    /// below the chart rather than dropped with it.
    pub(super) fn remove_radar_curve(&mut self, column: usize, cx: &mut Context<Self>) {
        let Some(form) = self.radar_mut() else {
            return;
        };
        if column >= form.curves.len() {
            return;
        }
        let curve = form.curves.remove(column);
        for text in curve.leading_lines {
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
        self.radar_changed(cx);
    }

    pub(super) fn move_radar_curve(
        &mut self,
        column: usize,
        forward: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = self.radar_mut() else {
            return;
        };
        let Some(other) = neighbour(column, forward, form.curves.len()) else {
            return;
        };
        form.curves.swap(column, other);
        for row in &mut form.rows {
            if column < row.cells.len() && other < row.cells.len() {
                row.cells.swap(column, other);
            }
        }
        self.radar_changed(cx);
    }

    pub(super) fn render_radar(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(form) = self.radar() else {
            return Vec::new();
        };
        let current = form.current_document(cx);
        let options = &current.options;
        vec![
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
                            .is_none_or(|title| is_valid_radar_title(title)),
                        window,
                        cx,
                    )
                    .flex_1()
                    .child(form.title.clone()),
                )
                .into_any_element(),
            h_flex()
                .w_full()
                .gap_1()
                .child(section_title("最小"))
                .child(
                    field_frame(
                        &form.min,
                        NUMBER_FIELD_WIDTH,
                        optional_number_is_valid(options.min.as_ref()),
                        window,
                        cx,
                    )
                    .child(form.min.clone()),
                )
                .child(section_title("最大"))
                .child(
                    field_frame(
                        &form.max,
                        NUMBER_FIELD_WIDTH,
                        optional_number_is_valid(options.max.as_ref()),
                        window,
                        cx,
                    )
                    .child(form.max.clone()),
                )
                .child(section_title("目盛りの数"))
                .child(
                    field_frame(
                        &form.ticks,
                        NUMBER_FIELD_WIDTH,
                        optional_number_is_valid(options.ticks.as_ref()),
                        window,
                        cx,
                    )
                    .child(form.ticks.clone()),
                )
                .child(
                    Label::new("(空なら既定)")
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .into_any_element(),
            self.render_radar_curves(form, &current, window, cx),
            self.render_radar_table(form, &current, window, cx),
        ]
    }

    fn render_radar_curves(
        &self,
        form: &RadarForm,
        current: &RadarDocument,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut list = v_flex().w_full().gap_1().child(section_title(
            "系列 (ID と凡例の名前。名前が空なら ID を表示)",
        ));
        let count = form.curves.len();
        for (column, curve) in form.curves.iter().enumerate() {
            let document_curve = current.curves.get(column);
            let id_valid = document_curve.is_none_or(|document_curve| {
                is_valid_radar_id(&document_curve.id)
                    && current
                        .curves
                        .iter()
                        .filter(|other| other.id == document_curve.id)
                        .count()
                        == 1
            });
            let label_valid = document_curve
                .and_then(|curve| curve.label.as_ref())
                .is_none_or(|label| is_valid_radar_label(label));
            list = list.child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .child(
                        field_frame(&curve.id, ID_FIELD_WIDTH, id_valid, window, cx)
                            .child(curve.id.clone()),
                    )
                    .child(
                        field_frame(&curve.label, LABEL_FIELD_WIDTH, label_valid, window, cx)
                            .flex_1()
                            .child(curve.label.clone()),
                    )
                    .child(render_choice_menu(
                        SharedString::from(format!("mermaid-radar-curve-keyed-{column}")),
                        keyed_label(curve.keyed).into(),
                        [false, true]
                            .into_iter()
                            .map(|keyed| (SharedString::from(keyed_label(keyed)), keyed))
                            .collect(),
                        curve.keyed,
                        move |this, keyed, _window, cx| {
                            this.set_radar_curve_keyed(column, keyed, cx)
                        },
                        window,
                        cx,
                    ))
                    .child(row_controls(
                        "mermaid-radar-curve",
                        column,
                        column == 0,
                        column + 1 >= count,
                        cx.listener(move |this, _, _window, cx| {
                            this.move_radar_curve(column, false, cx)
                        }),
                        cx.listener(move |this, _, _window, cx| {
                            this.move_radar_curve(column, true, cx)
                        }),
                        cx.listener(move |this, _, _window, cx| {
                            this.remove_radar_curve(column, cx)
                        }),
                    )),
            );
        }
        list.child(add_button(
            "mermaid-radar-add-curve",
            "系列を追加",
            cx.listener(|this, _, window, cx| this.add_radar_curve(window, cx)),
        ))
        .into_any_element()
    }

    fn render_radar_table(
        &self,
        form: &RadarForm,
        current: &RadarDocument,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let axis_count = form.axis_count();
        let curve_count = form.curves.len();
        // The scroll container needs to be told how wide its content is, or it
        // measures nothing to scroll.
        let width = Rems(
            ID_FIELD_WIDTH.0
                + LABEL_FIELD_WIDTH.0
                + curve_count as f32 * (NUMBER_FIELD_WIDTH.0 + 0.25)
                + 5.5,
        );

        let mut header = h_flex()
            .gap_1()
            .child(
                div()
                    .w(ID_FIELD_WIDTH)
                    .flex_none()
                    .child(section_title("軸の ID")),
            )
            .child(
                div()
                    .w(LABEL_FIELD_WIDTH)
                    .flex_none()
                    .child(section_title("ラベル")),
            );
        for (column, curve) in current.curves.iter().enumerate() {
            let name = curve.label.clone().unwrap_or_else(|| curve.id.clone());
            header = header.child(
                div()
                    .w(NUMBER_FIELD_WIDTH)
                    .flex_none()
                    .overflow_hidden()
                    .child(section_title(if name.is_empty() {
                        format!("{}", column + 1)
                    } else {
                        name
                    })),
            );
        }

        let mut table = v_flex().w(width).gap_1().child(header);
        let last = form.rows.len().saturating_sub(1);
        for (index, row) in form.rows.iter().enumerate() {
            let mut line = h_flex().gap_1();
            match &row.axis {
                Some(axis) => {
                    let document_axis = current.axes.get(index);
                    let id_valid = document_axis.is_none_or(|document_axis| {
                        is_valid_radar_id(&document_axis.id)
                            && current
                                .axes
                                .iter()
                                .filter(|other| other.id == document_axis.id)
                                .count()
                                == 1
                    });
                    let label_valid = document_axis
                        .and_then(|axis| axis.label.as_ref())
                        .is_none_or(|label| is_valid_radar_label(label));
                    line = line
                        .child(
                            field_frame(&axis.id, ID_FIELD_WIDTH, id_valid, window, cx)
                                .child(axis.id.clone()),
                        )
                        .child(
                            field_frame(&axis.label, LABEL_FIELD_WIDTH, label_valid, window, cx)
                                .child(axis.label.clone()),
                        );
                }
                None => {
                    line = line.child(
                        div()
                            .w(Rems(ID_FIELD_WIDTH.0 + LABEL_FIELD_WIDTH.0 + 0.25))
                            .flex_none()
                            .child(
                                Label::new("(軸なし・描かれません)")
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            ),
                    );
                }
            }
            for (column, cell) in row.cells.iter().enumerate() {
                let valid = current.curves.get(column).is_none_or(|curve| {
                    match curve.values.get(index) {
                        Some(value) => is_valid_radar_number(value),
                        // A curve written with axis names needs a value for
                        // every axis.
                        None => !(curve.keyed && index < axis_count),
                    }
                });
                line = line.child(
                    field_frame(cell, NUMBER_FIELD_WIDTH, valid, window, cx).child(cell.clone()),
                );
            }
            line = line.child(row_controls(
                "mermaid-radar-axis",
                index,
                !row_may_move(&form.rows, index, false),
                index >= last || !row_may_move(&form.rows, index, true),
                cx.listener(move |this, _, _window, cx| this.move_radar_axis(index, false, cx)),
                cx.listener(move |this, _, _window, cx| this.move_radar_axis(index, true, cx)),
                cx.listener(move |this, _, _window, cx| this.remove_radar_axis(index, cx)),
            ));
            table = table.child(line);
        }

        let mut section = v_flex()
            .w_full()
            .gap_1()
            .child(section_title(
                "軸と値 (軸を消すと全系列のその値も消えます。末尾の空欄は最小値として描かれます)",
            ))
            .child(
                div()
                    .id("mermaid-radar-table")
                    .w_full()
                    .overflow_x_scroll()
                    .child(table),
            );
        if axis_count < form.rows.len() {
            section = section.child(
                Label::new("軸の数より多い値は描かれません。軸を追加するとその値に軸が付きます。")
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            );
        }
        section
            .child(add_button(
                "mermaid-radar-add-axis",
                "軸を追加",
                cx.listener(|this, _, window, cx| this.add_radar_axis(window, cx)),
            ))
            .into_any_element()
    }

    pub(super) fn render_radar_toolbar(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(form) = self.radar() else {
            return div().into_any_element();
        };
        let graticule = form.document.options.graticule;
        let shown = form.document.options.show_legend;
        h_flex()
            .gap_2()
            .child(section_title("目盛りの形"))
            .child(render_choice_menu(
                "mermaid-radar-graticule".into(),
                graticule_label(graticule).into(),
                [None]
                    .into_iter()
                    .chain(RadarGraticule::ALL.map(Some))
                    .map(|choice| (SharedString::from(graticule_label(choice)), choice))
                    .collect(),
                graticule,
                |this, choice, _window, cx| this.set_radar_graticule(choice, cx),
                window,
                cx,
            ))
            .child(section_title("凡例"))
            .child(render_choice_menu(
                "mermaid-radar-legend".into(),
                legend_label(shown).into(),
                [None, Some(true), Some(false)]
                    .into_iter()
                    .map(|choice| (SharedString::from(legend_label(choice)), choice))
                    .collect(),
                shown,
                |this, choice, _window, cx| this.set_radar_show_legend(choice, cx),
                window,
                cx,
            ))
            .into_any_element()
    }
}

#[cfg(test)]
#[path = "radar_tests.rs"]
mod radar_tests;
