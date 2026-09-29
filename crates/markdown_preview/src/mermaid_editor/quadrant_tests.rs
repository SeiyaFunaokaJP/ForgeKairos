use super::*;
use editor::test::editor_test_context::EditorTestContext;
use gpui::TestAppContext;
use indoc::indoc;
use mermaid_render::mermaid_model::parse_document;
use mermaid_render::quadrant_model::{emit_quadrant, parse_quadrant};
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

fn point_row(modal: &MermaidEditor, index: usize) -> &PointRow {
    modal
        .quadrant()
        .and_then(|form| form.points.get(index))
        .unwrap_or_else(|| panic!("point {index} should have a row"))
}

const TEMPLATE_FENCE: &str = indoc! {"
    ```mermaid
    quadrantChartˇ
        %% 点は 名前: [横, 縦] で、どちらも 0〜1
        title 施策の優先度
        x-axis 労力が小さい --> 労力が大きい
        y-axis 効果が小さい --> 効果が大きい
        quadrant-1 計画して進める
        quadrant-2 すぐやる
        quadrant-3 後回し
        quadrant-4 見直す
        施策A: [0.3, 0.8]
        施策B: [0.7, 0.6]
        施策C: [0.2, 0.3]
    ```
"};

/// The template `markdown::InsertMermaidDiagram` writes is read from the action
/// rather than copied here, so that a template which drifts away from what the
/// model emits turns this red.
#[test]
fn the_insert_template_is_the_source_the_model_writes_back() {
    let check = |template: &str| {
        let chart = parse_quadrant(template)
            .unwrap_or_else(|error| panic!("the insert template should open: {error}"));
        assert_eq!(emit_quadrant(&chart), template);
    };
    check(editor::actions::MermaidDiagramKind::QuadrantChart.template());

    // The fences these tests open are the Japanese template, which is what a
    // user with the Japanese catalog inserts.
    let _japanese = i18n::test_support::LocaleOverride::new(Some(
        i18n_catalog::test_support::bundled_catalog("ja"),
    ));
    let template = editor::actions::MermaidDiagramKind::QuadrantChart.template();
    check(template);
    assert!(
        TEMPLATE_FENCE.replace('\u{2c7}', "").contains(template),
        "the fence these tests open should hold the insert template"
    );
}

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
        "an unedited quadrant chart must be written back exactly as it was"
    );
}

/// The axes, a quadrant label and the points are written back; a name that
/// would read as a keyword is quoted, and styles are written the way the
/// emitter spells them.
#[gpui::test]
async fn axes_quadrants_and_points_are_written_back(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            let Some((high, quadrant)) = modal
                .quadrant()
                .map(|form| (form.y_axis.high.clone(), form.quadrants[1].clone()))
            else {
                panic!("the modal should hold a quadrant chart form");
            };
            high.update(cx, |field, cx| field.set_text("", window, cx));
            quadrant.update(cx, |field, cx| field.set_text("今すぐ", window, cx));
            let x = point_row(modal, 0).x.clone();
            x.update(cx, |field, cx| field.set_text("0.35", window, cx));
            modal.remove_quadrant_point(2, cx);
            modal.move_quadrant_point(1, false, cx);
            modal.add_quadrant_point(window, cx);
            let row = point_row(modal, 2);
            let (name, class, styles) = (row.name.clone(), row.class.clone(), row.styles.clone());
            name.update(cx, |field, cx| field.set_text("title 案", window, cx));
            class.update(cx, |field, cx| field.set_text("注目", window, cx));
            styles.update(cx, |field, cx| {
                field.set_text("radius:12 ,color:#f00", window, cx)
            });
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    // The class name has to be ASCII, so that edit is refused first.
    let message = cx.update(|_window, cx| modal.read(cx).message.clone());
    assert!(
        message
            .as_deref()
            .is_some_and(|message| message.contains("class name")),
        "the modal should refuse the class name, got {message:?}"
    );

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            let class = point_row(modal, 2).class.clone();
            class.update(cx, |field, cx| field.set_text("hot", window, cx));
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        indoc! {r#"
            ```mermaid
            quadrantChart
                %% 点は 名前: [横, 縦] で、どちらも 0〜1
                title 施策の優先度
                x-axis 労力が小さい --> 労力が大きい
                y-axis 効果が小さい
                quadrant-1 計画して進める
                quadrant-2 今すぐ
                quadrant-3 後回し
                quadrant-4 見直す
                施策B: [0.7, 0.6]
                施策A: [0.35, 0.8]
                "title 案":::hot: [0.5, 0.5] radius: 12, color: #f00
            ```
        "#},
    );
}

/// A coordinate Mermaid cannot read is reported and nothing is written.
#[gpui::test]
async fn a_coordinate_out_of_range_is_refused(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;
    let before = cx.update_editor(|editor, _window, cx| editor.text(cx));

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            let y = point_row(modal, 1).y.clone();
            y.update(cx, |field, cx| field.set_text("1.5", window, cx));
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });
    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        before
    );
    let message = cx.update(|_window, cx| modal.read(cx).message.clone());
    assert!(
        message
            .as_deref()
            .is_some_and(|message| message.contains("`1.5`")),
        "the modal should name the coordinate it refused, got {message:?}"
    );
}

/// An axis written with `-->` and nothing after it is drawn with an arrow;
/// its empty far end field, left alone, keeps the arrow.
#[gpui::test]
async fn an_open_axis_survives_an_untouched_empty_field(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(
        indoc! {"
            ```mermaid
            quadrantChartˇ
                x-axis 到達範囲 -->
                A: [0.1, 0.2]
            ```
        "},
        cx,
    )
    .await;

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            let x = point_row(modal, 0).x.clone();
            x.update(cx, |field, cx| field.set_text("0.4", window, cx));
            modal.confirm(&menu::SecondaryConfirm, window, cx)
        })
    });

    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        indoc! {"
            ```mermaid
            quadrantChart
                x-axis 到達範囲 -->
                A: [0.4, 0.2]
            ```
        "},
    );
}
