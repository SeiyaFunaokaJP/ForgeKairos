use super::*;
use editor::test::editor_test_context::EditorTestContext;
use gpui::TestAppContext;
use mermaid_render::mermaid_model::parse_document;
use mermaid_render::xychart_model::{emit_xychart, parse_xychart};
use std::sync::Arc;

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

const TEMPLATE_FENCE: &str = "```mermaid\nxychart-betaˇ\n    %% bar は棒、line は折れ線。値の数は x-axis の項目の数と揃える\n    title \"月別の売上\"\n    x-axis [\"1月\", \"2月\", \"3月\", \"4月\"]\n    y-axis \"売上 (万円)\" 0 --> 100\n    bar [30, 45, 60, 80]\n    line [30, 45, 60, 80]\n```\n";

fn cell(modal: &MermaidEditor, row: usize, column: usize) -> Entity<Editor> {
    modal
        .xychart()
        .and_then(|form| form.rows.get(row))
        .and_then(|row| row.cells.get(column))
        .cloned()
        .unwrap_or_else(|| panic!("cell {row}/{column} should exist"))
}

fn category(modal: &MermaidEditor, row: usize) -> Entity<Editor> {
    modal
        .xychart()
        .and_then(|form| form.rows.get(row))
        .and_then(|row| row.category.as_ref())
        .map(|category| category.field.clone())
        .unwrap_or_else(|| panic!("row {row} should have a category"))
}

fn fence_text(cx: &mut EditorTestContext) -> String {
    cx.update_editor(|editor, _window, cx| editor.text(cx))
}

/// The template `markdown::InsertMermaidDiagram` writes is read from the action
/// rather than copied here, so that a template which drifts away from what the
/// model emits turns this red.
#[test]
fn the_insert_template_is_the_source_the_model_writes_back() {
    let check = |template: &str| {
        let chart = parse_xychart(template)
            .unwrap_or_else(|error| panic!("the insert template should open: {error}"));
        assert_eq!(emit_xychart(&chart), template);
    };
    check(editor::actions::MermaidDiagramKind::XyChart.template());

    // The fences these tests open are the Japanese template, which is what a
    // user with the Japanese catalog inserts.
    let _japanese = i18n::test_support::LocaleOverride::new(Some(
        i18n_catalog::test_support::bundled_catalog("ja"),
    ));
    let template = editor::actions::MermaidDiagramKind::XyChart.template();
    check(template);
    assert!(
        TEMPLATE_FENCE.replace('\u{2c7}', "").contains(template),
        "the fence these tests open should hold the insert template"
    );
}

#[gpui::test]
async fn confirming_the_template_unchanged_leaves_the_fence_alone(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;
    let before = fence_text(&mut cx);
    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            modal.confirm(&menu::SecondaryConfirm, window, cx)
        })
    });
    assert_eq!(
        fence_text(&mut cx),
        before,
        "an unedited chart must be written back exactly as it was"
    );
}

/// Renaming, adding and removing a category carries every series' value for
/// it, moving a series moves its whole column, and a new series gets a value
/// for every category.
#[gpui::test]
async fn categories_and_series_move_their_values_together(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            category(modal, 0).update(cx, |field, cx| field.set_text("一月", window, cx));
            modal.add_xychart_row(window, cx);
            cell(modal, 4, 0).update(cx, |field, cx| field.set_text("90", window, cx));
            cell(modal, 4, 1).update(cx, |field, cx| field.set_text("95", window, cx));
            modal.remove_xychart_row(1, cx);
            modal.move_xychart_series(1, false, cx);
            modal.add_xychart_series(XySeriesKind::Bar, window, cx);
            modal.set_xychart_orientation(Some(XyOrientation::Horizontal), cx);
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    assert_eq!(
        fence_text(&mut cx),
        "```mermaid\nxychart-beta horizontal\n    %% bar は棒、line は折れ線。値の数は x-axis の項目の数と揃える\n    title \"月別の売上\"\n    x-axis [\"一月\", \"3月\", \"4月\", \"Category 5\"]\n    y-axis \"売上 (万円)\" 0 --> 100\n    line [30, 60, 80, 95]\n    bar [30, 60, 80, 90]\n    bar [0, 0, 0, 0]\n```\n"
    );
}

/// Switching the x axis to a range drops the category names and keeps the
/// values, and the range starts out covering them.
#[gpui::test]
async fn switching_the_x_axis_to_a_range_keeps_the_values(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            modal.set_xychart_x_mode(XMode::Range, window, cx);
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    assert_eq!(
        fence_text(&mut cx),
        "```mermaid\nxychart-beta\n    %% bar は棒、line は折れ線。値の数は x-axis の項目の数と揃える\n    title \"月別の売上\"\n    x-axis 1 --> 4\n    y-axis \"売上 (万円)\" 0 --> 100\n    bar [30, 45, 60, 80]\n    line [30, 45, 60, 80]\n```\n"
    );
}

/// A gap in the middle of a series and a category with a quote in it cannot
/// be written; nothing is written and the reason is shown.
#[gpui::test]
async fn what_mermaid_cannot_read_is_refused(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;
    let before = fence_text(&mut cx);

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            cell(modal, 1, 0).update(cx, |field, cx| field.set_text("", window, cx));
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });
    assert_eq!(fence_text(&mut cx), before);
    let message = cx.update(|_window, cx| modal.read(cx).message.clone());
    assert!(
        message
            .as_deref()
            .is_some_and(|message| message.contains("value 2 is empty")),
        "the modal should name the gap, got {message:?}"
    );

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            cell(modal, 1, 0).update(cx, |field, cx| field.set_text("45", window, cx));
            category(modal, 2).update(cx, |field, cx| field.set_text("3\"月", window, cx));
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });
    assert_eq!(fence_text(&mut cx), before);
    let message = cx.update(|_window, cx| modal.read(cx).message.clone());
    assert!(
        message
            .as_deref()
            .is_some_and(|message| message.contains("3\"月")),
        "the modal should name the category, got {message:?}"
    );
}

/// Values past the last category are shown without one, and adding a
/// category names the first of them instead of adding a blank row.
#[gpui::test]
async fn a_new_category_names_the_first_value_past_the_categories(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(
        "```mermaid\nxychartˇ\n    x-axis [a, b]\n    bar [1, 2, 3]\n    line [5]\n```\n",
        cx,
    )
    .await;

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            assert_eq!(modal.xychart().map(|form| form.rows.len()), Some(3));
            assert!(
                modal
                    .xychart()
                    .and_then(|form| form.rows.get(2))
                    .is_some_and(|row| row.category.is_none())
            );
            modal.add_xychart_row(window, cx);
            assert_eq!(modal.xychart().map(|form| form.rows.len()), Some(3));
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    assert_eq!(
        fence_text(&mut cx),
        // The line's blank cells below its one value are values it does not
        // have, not gaps.
        "```mermaid\nxychart\n    x-axis [a, b, \"Category 3\"]\n    bar [1, 2, 3]\n    line [5]\n```\n"
    );
}

/// Without categories or a range on the x axis, Mermaid builds the axis from
/// the first series, and a first series of one value makes it lay out a
/// longer one forever. Emptying the cell that makes it so is refused before
/// anything is drawn or written.
#[gpui::test]
async fn a_chart_mermaid_never_finishes_is_refused(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(
        "```mermaid\nxychartˇ\n    bar [1, 2]\n    line [3, 4]\n```\n",
        cx,
    )
    .await;
    let before = fence_text(&mut cx);

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            cell(modal, 1, 0).update(cx, |field, cx| field.set_text("", window, cx));
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });
    assert_eq!(fence_text(&mut cx), before);
    let message = cx.update(|_window, cx| modal.read(cx).message.clone());
    assert!(
        message
            .as_deref()
            .is_some_and(|message| message.contains("never finishes drawing")),
        "the modal should say why, got {message:?}"
    );
}
