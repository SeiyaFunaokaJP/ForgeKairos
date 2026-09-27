use super::*;
use editor::test::editor_test_context::EditorTestContext;
use gpui::TestAppContext;
use indoc::indoc;
use mermaid_render::kanban_model::{emit_kanban, parse_kanban};
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

fn card(modal: &MermaidEditor, column: usize, index: usize) -> &CardRow {
    modal
        .kanban()
        .and_then(|form| form.columns.get(column)?.cards.get(index))
        .unwrap_or_else(|| panic!("card {index} of column {column} should have a row"))
}

const TEMPLATE_FENCE: &str = indoc! {"
    ```mermaid
    kanbanˇ
        %% 字下げの浅い行が列、深い行がカード
        todo[未着手]
            spec[仕様を書く]
            screen[画面を作る]
        doing[作業中]
            api[API を作る]
        done[完了]
            setup[環境を整える]
    ```
"};

/// The template `markdown::InsertMermaidDiagram` writes is read from the action
/// rather than copied here, so that a template which drifts away from what the
/// model emits turns this red.
#[test]
fn the_insert_template_is_the_source_the_model_writes_back() {
    let template = editor::actions::MermaidDiagramKind::Kanban.template();
    let board = parse_kanban(template)
        .unwrap_or_else(|error| panic!("the insert template should open: {error}"));
    assert_eq!(emit_kanban(&board), template);
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
        "an unedited board must be written back exactly as it was"
    );
}

/// The fields travel with their cards: after a card changes columns and
/// another is added, every row still shows the text of the card it stands
/// for, and the details typed and picked end up in the card's `@{ … }`.
#[gpui::test]
async fn cards_carry_their_fields_across_columns(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            // 画面を作る goes to the end of 作業中.
            modal.move_kanban_card_across(0, 1, true, cx);
            assert_eq!(
                card(modal, 1, 1).title.read(cx).text(cx),
                "画面を作る".to_string()
            );
            let spec = card(modal, 0, 0);
            let (title, assigned) = (spec.title.clone(), spec.assigned.clone());
            title.update(cx, |title, cx| {
                title.set_text("仕様 [草案] を書く", window, cx)
            });
            assigned.update(cx, |assigned, cx| assigned.set_text("山田", window, cx));
            modal.set_kanban_priority(0, 0, Some("High"), cx);
            modal.add_kanban_card(2, window, cx);
            // The column moves right past 作業中, taking its card along.
            modal.move_kanban_column(0, true, cx);
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        indoc! {"
            ```mermaid
            kanban
                %% 字下げの浅い行が列、深い行がカード
                doing[作業中]
                    api[API を作る]
                    screen[画面を作る]
                todo[未着手]
                    spec[\"仕様 [草案] を書く\"]@{ assigned: 山田, priority: 'High' }
                done[完了]
                    setup[環境を整える]
                    card1[新しいカード]
            ```
        "},
    );
}

/// Two nodes with one id would draw each other's cards, so the board is not
/// written and the modal says why; a card's details that Mermaid would cut
/// short are refused the same way.
#[gpui::test]
async fn what_cannot_be_written_is_refused(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;
    let before = cx.update_editor(|editor, _window, cx| editor.text(cx));

    for (edit, reason) in [
        (
            (|modal: &MermaidEditor, window: &mut Window, cx: &mut App| {
                let id = card(modal, 1, 0).id.clone();
                id.update(cx, |id, cx| id.set_text("todo", window, cx));
            }) as fn(&MermaidEditor, &mut Window, &mut App),
            "`todo` が 2 か所",
        ),
        (
            |modal, window, cx| {
                let id = card(modal, 1, 0).id.clone();
                id.update(cx, |id, cx| id.set_text("api", window, cx));
                let ticket = card(modal, 1, 0).ticket.clone();
                ticket.update(cx, |ticket, cx| ticket.set_text("MC-1 } x", window, cx));
            },
            "`ticket` の値",
        ),
    ] {
        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                edit(modal, window, cx);
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });
        assert_eq!(
            cx.update_editor(|editor, _window, cx| editor.text(cx)),
            before,
            "a board that cannot be written back must not be written"
        );
        let message = cx.update(|_window, cx| modal.read(cx).message.clone());
        assert!(
            message
                .as_deref()
                .is_some_and(|message| message.contains(reason)),
            "the modal should say why it refused ({reason}), got {message:?}"
        );
    }
}

/// Nothing in a board names a card by id, so an id typed over another card's
/// on the way to a new one changes no other card when a button is pressed in
/// between.
#[gpui::test]
async fn a_passing_duplicate_id_changes_no_other_card(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;
    let before = cx.update_editor(|editor, _window, cx| editor.text(cx));

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            let id = card(modal, 0, 0).id.clone();
            id.update(cx, |id, cx| id.set_text("screen", window, cx));
            modal.move_kanban_card(0, 0, true, cx);
            modal.move_kanban_card(0, 1, false, cx);
            id.update(cx, |id, cx| id.set_text("write", window, cx));
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        before.replace("spec[", "write[")
    );
}
