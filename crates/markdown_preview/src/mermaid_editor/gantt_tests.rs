//! The form's side of the gantt leftovers: a deletion that writes down dates
//! or is refused with a reason, lines the form does not show staying where
//! they were, and a rename reaching `click`.

use super::*;
use editor::test::editor_test_context::EditorTestContext;
use gpui::TestAppContext;
use indoc::indoc;
use mermaid_render::mermaid_model::parse_document;
use std::sync::Arc;

fn init_test(cx: &mut TestAppContext) -> Arc<workspace::AppState> {
    cx.update(|cx| {
        let state = workspace::AppState::test(cx);
        editor::init(cx);
        crate::init(cx);
        state
    })
}

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

fn row_count(modal: &MermaidEditor) -> Vec<usize> {
    modal
        .gantt()
        .map(|form| {
            form.sections
                .iter()
                .map(|section| section.tasks.len())
                .collect()
        })
        .unwrap_or_default()
}

/// Deleting the task another one starts `after` gives that one the date it
/// used to start on, even when the deleted task ended after a length.
#[gpui::test]
async fn deleting_a_task_writes_down_where_it_ended(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(
        indoc! {"
            ```mermaid
            ganttˇ
                dateFormat YYYY-MM-DD
                section 設計
                要件定義 :done, req, 2026-01-06, 5d
                %% 設計のメモ
                基本設計 :active, design, after req, 7d
                section 実装
                実装 :impl, after design, 10d
            ```
        "},
        cx,
    )
    .await;

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            modal.remove_gantt_task(0, 1, window, cx);
            let end = modal
                .gantt()
                .and_then(|form| form.sections.get(1))
                .and_then(|section| section.tasks.first())
                .map(|row| row.start.read(cx).text(cx));
            assert_eq!(
                end.as_deref(),
                Some("2026-01-18"),
                "the start field has to show the date it was rewritten to"
            );
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        indoc! {"
            ```mermaid
            gantt
                dateFormat YYYY-MM-DD
                section 設計
                要件定義 :done, req, 2026-01-06, 5d
                %% 設計のメモ
                section 実装
                実装 :impl, 2026-01-18, 10d
            ```
        "},
    );
}

/// A deletion the chart's `dateFormat` cannot write down is refused: the rows
/// stay, the reason is shown, and nothing is written.
#[gpui::test]
async fn a_deletion_that_would_move_a_task_is_refused(cx: &mut TestAppContext) {
    let fence = indoc! {"
        ```mermaid
        ganttˇ
            dateFormat MMM D YYYY
            A :a, Jan 1 2026, 3d
            B :b, after a, 2d
            C :c, after b, 1d
        ```
    "};
    let (mut cx, modal, _app_state) = open_on(fence, cx).await;
    let before = cx.update_editor(|editor, _window, cx| editor.text(cx));

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            modal.remove_gantt_task(0, 1, window, cx);
        })
    });
    let (message, rows) = cx.update(|_window, cx| {
        let modal = modal.read(cx);
        (modal.message.clone(), row_count(modal))
    });
    assert!(
        message
            .as_deref()
            .is_some_and(|message| message.contains("削除できません") && message.contains("`C`")),
        "the reason should name the task that would move: {message:?}"
    );
    assert_eq!(rows, vec![3], "no row may go when the model refused");

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });
    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        before
    );
}

const COMMENTED_FENCE: &str = indoc! {"
    ```mermaid
    ganttˇ
        dateFormat YYYY-MM-DD
        A :a, 2026-01-05, 3d
        %% A のメモ
        todayMarker off
        B :b, 2026-01-06, 2d
    ```
"};

/// A comment between two tasks is written back where it was.
#[gpui::test]
async fn a_comment_between_tasks_keeps_its_place(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(COMMENTED_FENCE, cx).await;
    let before = cx.update_editor(|editor, _window, cx| editor.text(cx));

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });
    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        before,
        "an unedited chart must be written back exactly as it was"
    );
}

/// The comment belongs to the task above it and moves with it.
#[gpui::test]
async fn a_comment_moves_with_the_task_above_it(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(COMMENTED_FENCE, cx).await;
    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            modal.move_gantt_task(0, 0, true, window, cx);
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });
    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        indoc! {"
            ```mermaid
            gantt
                dateFormat YYYY-MM-DD
                B :b, 2026-01-06, 2d
                A :a, 2026-01-05, 3d
                %% A のメモ
                todayMarker off
            ```
        "},
    );
}

/// Renaming an id in its field reaches the `click` that names it.
#[gpui::test]
async fn renaming_an_id_follows_into_click(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(
        indoc! {"
            ```mermaid
            ganttˇ
                dateFormat YYYY-MM-DD
                A :a, 2026-01-05, 3d
                click a href \"https://example.com\"
            ```
        "},
        cx,
    )
    .await;

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            let id = modal
                .gantt()
                .and_then(|form| form.sections.first())
                .and_then(|section| section.tasks.first())
                .map(|row| row.id.clone())
                .expect("the task should have a row");
            id.update(cx, |id, cx| id.set_text("req", window, cx));
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        indoc! {"
            ```mermaid
            gantt
                dateFormat YYYY-MM-DD
                A :req, 2026-01-05, 3d
                click req href \"https://example.com\"
            ```
        "},
    );
}

#[test]
fn the_tag_menu_names_the_tags_a_task_carries() {
    assert_eq!(tag_menu_label(&[]), SharedString::from("タグ"));
    assert_eq!(
        tag_menu_label(&[GanttTag::Done, GanttTag::Crit]),
        SharedString::from("完了・重要")
    );
}
