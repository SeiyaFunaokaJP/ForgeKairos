//! The quadrant chart half of the Mermaid editor's form.
//!
//! A quadrant chart is a title, two axes with a label at each end, a label per
//! quadrant and a list of points, so the form is those fields in that order.
//! The four quadrant labels are laid out two by two the way the chart places
//! them, and each point is a row with its coordinates and a second line for
//! its class and styles. `classDef` lines are kept as they are written.

use editor::Editor;
use gpui::{AnyElement, App, Context, Entity, Focusable as _, Window};
use mermaid_render::quadrant_model::{
    QuadrantAxis, QuadrantDocument, QuadrantPoint, is_valid_quadrant_axis, is_valid_quadrant_class,
    is_valid_quadrant_coordinate, is_valid_quadrant_label, is_valid_quadrant_point_name,
    is_valid_quadrant_title, normalized_quadrant_styles,
};
use ui::{IconButtonShape, Tooltip, prelude::*};

use super::{
    Form, ID_FIELD_WIDTH, LABEL_FIELD_WIDTH, MermaidEditor, NUMBER_FIELD_WIDTH, add_button,
    field_frame, neighbour, row_controls, section_title,
};

/// The values new points are added with, so that they are visible in the
/// middle of the drawing before anything has been typed into them.
const NEW_POINT_NAME: &str = i18n::msg!("New point");
const NEW_POINT_COORDINATE: &str = "0.5";

/// The quadrants in the order the form lays them out, left to right and top
/// to bottom, with the number Mermaid gives each.
const QUADRANT_LAYOUT: [[(usize, &str); 2]; 2] = [
    [
        (1, i18n::msg!("Top Left (2)")),
        (0, i18n::msg!("Top Right (1)")),
    ],
    [
        (2, i18n::msg!("Bottom Left (3)")),
        (3, i18n::msg!("Bottom Right (4)")),
    ],
];

pub(super) struct QuadrantForm {
    pub(super) document: QuadrantDocument,
    pub(super) title: Entity<Editor>,
    pub(super) x_axis: AxisRow,
    pub(super) y_axis: AxisRow,
    pub(super) quadrants: [Entity<Editor>; 4],
    pub(super) points: Vec<PointRow>,
}

pub(super) struct AxisRow {
    pub(super) low: Entity<Editor>,
    pub(super) high: Entity<Editor>,
}

pub(super) struct PointRow {
    pub(super) name: Entity<Editor>,
    pub(super) x: Entity<Editor>,
    pub(super) y: Entity<Editor>,
    pub(super) class: Entity<Editor>,
    pub(super) styles: Entity<Editor>,
}

impl QuadrantForm {
    pub(super) fn fields(&self) -> Vec<Entity<Editor>> {
        let mut fields = vec![
            self.title.clone(),
            self.x_axis.low.clone(),
            self.x_axis.high.clone(),
            self.y_axis.low.clone(),
            self.y_axis.high.clone(),
        ];
        for row in QUADRANT_LAYOUT {
            for (index, _) in row {
                if let Some(field) = self.quadrants.get(index) {
                    fields.push(field.clone());
                }
            }
        }
        for point in &self.points {
            fields.extend([
                point.name.clone(),
                point.x.clone(),
                point.y.clone(),
                point.class.clone(),
                point.styles.clone(),
            ]);
        }
        fields
    }

    /// The quadrant chart the form currently describes: the list of points
    /// held in `document` with the text fields' current contents written over
    /// it.
    pub(super) fn current_document(&self, cx: &App) -> QuadrantDocument {
        let mut document = self.document.clone();
        document.title = field_value(&self.title, cx);
        document.x_axis = axis_from_fields(&self.x_axis, document.x_axis.as_ref(), cx);
        document.y_axis = axis_from_fields(&self.y_axis, document.y_axis.as_ref(), cx);
        for (label, field) in document.quadrants.iter_mut().zip(&self.quadrants) {
            *label = field_value(field, cx);
        }
        for (point, row) in document.points.iter_mut().zip(&self.points) {
            point.name = field_text(&row.name, cx);
            point.x = field_text(&row.x, cx);
            point.y = field_text(&row.y, cx);
            point.class = field_value(&row.class, cx);
            let styles = field_text(&row.styles, cx);
            point.styles = normalized_quadrant_styles(&styles).unwrap_or(styles);
        }
        document
    }
}

/// The first thing wrong with what the user typed, in the words shown above the
/// form. Every check lives in `QuadrantDocument::structure_error`, which the
/// caller consults right after this; the red frames come from the
/// `is_valid_quadrant_*` predicates directly.
pub(super) fn problem(_document: &QuadrantDocument) -> Option<SharedString> {
    None
}

fn field_text(field: &Entity<Editor>, cx: &App) -> String {
    field.read(cx).text(cx).trim().to_string()
}

/// A field's text, or `None` when it is empty, which Mermaid draws as nothing.
fn field_value(field: &Entity<Editor>, cx: &App) -> Option<String> {
    let text = field_text(field, cx);
    (!text.is_empty()).then_some(text)
}

/// The axis two fields describe. An empty far end is no `-->`, except on an
/// axis written with a `-->` and nothing after it: Mermaid draws that one as an
/// arrow after the near end's label, and a field the user left empty must not
/// quietly take the arrow away.
fn axis_from_fields(
    row: &AxisRow,
    previous: Option<&QuadrantAxis>,
    cx: &App,
) -> Option<QuadrantAxis> {
    let low = field_text(&row.low, cx);
    let high = field_value(&row.high, cx).or_else(|| {
        previous
            .and_then(|axis| axis.high.clone())
            .filter(String::is_empty)
    });
    (!low.is_empty() || high.is_some()).then_some(QuadrantAxis { low, high })
}

impl MermaidEditor {
    pub(super) fn new_quadrant_form(
        &mut self,
        document: QuadrantDocument,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> QuadrantForm {
        let title = self.new_field(
            document.title.clone().unwrap_or_default().as_str(),
            window,
            cx,
        );
        let x_axis = self.new_quadrant_axis_row(document.x_axis.as_ref(), window, cx);
        let y_axis = self.new_quadrant_axis_row(document.y_axis.as_ref(), window, cx);
        let quadrants = [0, 1, 2, 3].map(|index| {
            let text = document
                .quadrants
                .get(index)
                .cloned()
                .flatten()
                .unwrap_or_default();
            self.new_field(&text, window, cx)
        });
        let mut points = Vec::with_capacity(document.points.len());
        for point in &document.points {
            points.push(self.new_quadrant_point_row(point, window, cx));
        }
        QuadrantForm {
            document,
            title,
            x_axis,
            y_axis,
            quadrants,
            points,
        }
    }

    fn new_quadrant_axis_row(
        &mut self,
        axis: Option<&QuadrantAxis>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AxisRow {
        AxisRow {
            low: self.new_field(axis.map_or("", |axis| axis.low.as_str()), window, cx),
            high: self.new_field(
                axis.and_then(|axis| axis.high.as_deref())
                    .unwrap_or_default(),
                window,
                cx,
            ),
        }
    }

    fn new_quadrant_point_row(
        &mut self,
        point: &QuadrantPoint,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> PointRow {
        PointRow {
            name: self.new_field(&point.name, window, cx),
            x: self.new_field(&point.x, window, cx),
            y: self.new_field(&point.y, window, cx),
            class: self.new_field(point.class.as_deref().unwrap_or_default(), window, cx),
            styles: self.new_field(&point.styles, window, cx),
        }
    }

    pub(super) fn quadrant(&self) -> Option<&QuadrantForm> {
        match &self.form {
            Form::QuadrantChart(form) => Some(form),
            _ => None,
        }
    }

    pub(super) fn quadrant_mut(&mut self) -> Option<&mut QuadrantForm> {
        match &mut self.form {
            Form::QuadrantChart(form) => Some(form),
            _ => None,
        }
    }

    /// Reads the fields into the document before a structural change, which
    /// moves the document and the rows in step.
    fn sync_quadrant(&mut self, cx: &mut Context<Self>) {
        let Some(document) = self.quadrant().map(|form| form.current_document(cx)) else {
            return;
        };
        if let Some(form) = self.quadrant_mut() {
            form.document = document;
        }
    }

    fn quadrant_changed(&mut self, cx: &mut Context<Self>) {
        self.schedule_preview(cx);
        cx.notify();
    }

    pub(super) fn add_quadrant_point(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_quadrant(cx);
        if self.quadrant().is_none() {
            return;
        }
        let point = QuadrantPoint {
            name: i18n::tr(NEW_POINT_NAME).to_string(),
            class: None,
            x: NEW_POINT_COORDINATE.to_string(),
            y: NEW_POINT_COORDINATE.to_string(),
            styles: String::new(),
        };
        let row = self.new_quadrant_point_row(&point, window, cx);
        let focus = row.name.clone();
        let Some(form) = self.quadrant_mut() else {
            return;
        };
        form.document.points.push(point);
        form.points.push(row);
        focus.focus_handle(cx).focus(window, cx);
        self.quadrant_changed(cx);
    }

    pub(super) fn remove_quadrant_point(&mut self, index: usize, cx: &mut Context<Self>) {
        self.sync_quadrant(cx);
        let Some(form) = self.quadrant_mut() else {
            return;
        };
        if index >= form.points.len() || index >= form.document.points.len() {
            return;
        }
        form.document.points.remove(index);
        form.points.remove(index);
        self.quadrant_changed(cx);
    }

    pub(super) fn move_quadrant_point(
        &mut self,
        index: usize,
        forward: bool,
        cx: &mut Context<Self>,
    ) {
        self.sync_quadrant(cx);
        let Some(form) = self.quadrant_mut() else {
            return;
        };
        let Some(other) = neighbour(index, forward, form.points.len()) else {
            return;
        };
        if other >= form.document.points.len() {
            return;
        }
        form.document.points.swap(index, other);
        form.points.swap(index, other);
        self.quadrant_changed(cx);
    }

    /// Takes away the arrow of an axis written with `-->` and nothing after
    /// it. The empty far end field cannot say this by itself, because leaving
    /// it empty is how the arrow is kept.
    pub(super) fn remove_quadrant_open_arrow(&mut self, is_y_axis: bool, cx: &mut Context<Self>) {
        self.sync_quadrant(cx);
        let Some(form) = self.quadrant_mut() else {
            return;
        };
        let axis = if is_y_axis {
            &mut form.document.y_axis
        } else {
            &mut form.document.x_axis
        };
        let Some(current) = axis.as_mut() else {
            return;
        };
        if current.high.as_deref() != Some("") {
            return;
        }
        current.high = None;
        if current.low.is_empty() {
            *axis = None;
        }
        self.quadrant_changed(cx);
    }

    pub(super) fn render_quadrant(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(form) = self.quadrant() else {
            return Vec::new();
        };
        let current = form.current_document(cx);

        let mut quadrants = v_flex().w_full().gap_1().child(section_title(i18n::t!(
            "Quadrant Labels (1–4 are Mermaid's quadrant numbers)"
        )));
        for row in QUADRANT_LAYOUT {
            let mut line = h_flex().w_full().gap_1();
            for (index, label) in row {
                let (Some(field), Some(value)) =
                    (form.quadrants.get(index), current.quadrants.get(index))
                else {
                    continue;
                };
                line = line.child(
                    h_flex()
                        .flex_1()
                        .gap_1()
                        .child(section_title(i18n::tr(label)))
                        .child(
                            field_frame(
                                field,
                                LABEL_FIELD_WIDTH,
                                value
                                    .as_ref()
                                    .is_none_or(|value| is_valid_quadrant_label(value)),
                                window,
                                cx,
                            )
                            .flex_1()
                            .child(field.clone()),
                        ),
                );
            }
            quadrants = quadrants.child(line);
        }

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
                            .is_none_or(|title| is_valid_quadrant_title(title)),
                        window,
                        cx,
                    )
                    .flex_1()
                    .child(form.title.clone()),
                )
                .into_any_element(),
            v_flex()
                .w_full()
                .gap_1()
                .child(section_title(i18n::t!(
                    "Axis Labels (an empty axis is not drawn)"
                )))
                .child(self.render_quadrant_axis(
                    i18n::t!("x-axis, left → right"),
                    false,
                    &form.x_axis,
                    current.x_axis.as_ref(),
                    window,
                    cx,
                ))
                .child(self.render_quadrant_axis(
                    i18n::t!("y-axis, bottom → top"),
                    true,
                    &form.y_axis,
                    current.y_axis.as_ref(),
                    window,
                    cx,
                ))
                .into_any_element(),
            quadrants.into_any_element(),
            self.render_quadrant_points(form, &current, window, cx),
        ]
    }

    fn render_quadrant_axis(
        &self,
        label: &'static str,
        is_y_axis: bool,
        row: &AxisRow,
        axis: Option<&QuadrantAxis>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let is_valid = axis.is_none_or(is_valid_quadrant_axis);
        let has_open_arrow = axis.is_some_and(|axis| axis.high.as_deref() == Some(""));
        h_flex()
            .w_full()
            .gap_1()
            .child(section_title(label))
            .child(
                field_frame(&row.low, LABEL_FIELD_WIDTH, is_valid, window, cx)
                    .flex_1()
                    .child(row.low.clone()),
            )
            .child(Label::new("→").size(LabelSize::Small).color(Color::Muted))
            .child(
                field_frame(&row.high, LABEL_FIELD_WIDTH, is_valid, window, cx)
                    .flex_1()
                    .child(row.high.clone()),
            )
            .when(has_open_arrow, |this| {
                this.child(
                    IconButton::new(
                        if is_y_axis {
                            "mermaid-quadrant-y-axis-remove-arrow"
                        } else {
                            "mermaid-quadrant-x-axis-remove-arrow"
                        },
                        IconName::Close,
                    )
                    .shape(IconButtonShape::Square)
                    .icon_size(IconSize::XSmall)
                    .tooltip(Tooltip::text(i18n::t!(
                        "Remove the arrow drawn after the label (the `-->` with nothing after it)"
                    )))
                    .on_click(cx.listener(move |this, _, _window, cx| {
                        this.remove_quadrant_open_arrow(is_y_axis, cx)
                    })),
                )
            })
            .into_any_element()
    }

    fn render_quadrant_points(
        &self,
        form: &QuadrantForm,
        current: &QuadrantDocument,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut list = v_flex().w_full().gap_2().child(section_title(i18n::t!(
            "Points (name, x, y; coordinates run from 0 to 1)"
        )));
        let length = form.points.len();
        for (index, row) in form.points.iter().enumerate() {
            let Some(point) = current.points.get(index) else {
                continue;
            };
            let styles_valid =
                point.styles.is_empty() || normalized_quadrant_styles(&point.styles).is_some();
            list = list.child(
                v_flex()
                    .w_full()
                    .gap_1()
                    .child(
                        h_flex()
                            .w_full()
                            .gap_1()
                            .child(
                                field_frame(
                                    &row.name,
                                    LABEL_FIELD_WIDTH,
                                    is_valid_quadrant_point_name(&point.name),
                                    window,
                                    cx,
                                )
                                .flex_1()
                                .child(row.name.clone()),
                            )
                            .child(
                                field_frame(
                                    &row.x,
                                    NUMBER_FIELD_WIDTH,
                                    is_valid_quadrant_coordinate(&point.x),
                                    window,
                                    cx,
                                )
                                .child(row.x.clone()),
                            )
                            .child(
                                field_frame(
                                    &row.y,
                                    NUMBER_FIELD_WIDTH,
                                    is_valid_quadrant_coordinate(&point.y),
                                    window,
                                    cx,
                                )
                                .child(row.y.clone()),
                            )
                            .child(row_controls(
                                "mermaid-quadrant-point",
                                index,
                                index == 0,
                                index + 1 >= length,
                                cx.listener(move |this, _, _window, cx| {
                                    this.move_quadrant_point(index, false, cx)
                                }),
                                cx.listener(move |this, _, _window, cx| {
                                    this.move_quadrant_point(index, true, cx)
                                }),
                                cx.listener(move |this, _, _window, cx| {
                                    this.remove_quadrant_point(index, cx)
                                }),
                            )),
                    )
                    .child(
                        h_flex()
                            .w_full()
                            .gap_1()
                            .pl_4()
                            .child(section_title(i18n::t!("Class")))
                            .child(
                                field_frame(
                                    &row.class,
                                    ID_FIELD_WIDTH,
                                    point.class.as_deref().is_none_or(is_valid_quadrant_class),
                                    window,
                                    cx,
                                )
                                .child(row.class.clone()),
                            )
                            .child(section_title(i18n::t!("Style")))
                            .child(
                                field_frame(
                                    &row.styles,
                                    LABEL_FIELD_WIDTH,
                                    styles_valid,
                                    window,
                                    cx,
                                )
                                .flex_1()
                                .child(row.styles.clone()),
                            ),
                    ),
            );
        }
        list.child(add_button(
            "mermaid-quadrant-add-point",
            i18n::t!("Add Point"),
            cx.listener(|this, _, window, cx| this.add_quadrant_point(window, cx)),
        ))
        .into_any_element()
    }

    pub(super) fn render_quadrant_toolbar(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> AnyElement {
        div().into_any_element()
    }
}

#[cfg(test)]
#[path = "quadrant_tests.rs"]
mod quadrant_tests;
