use super::*;
use editor::test::editor_test_context::EditorTestContext;
use gpui::TestAppContext;
use indoc::indoc;
use mermaid_render::block_model::{emit_block, parse_block};
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

/// The row of the block whose id field holds `id`.
fn row_index(modal: &MermaidEditor, id: &str, cx: &App) -> usize {
    modal
        .block_form()
        .and_then(|form| {
            form.entries
                .iter()
                .position(|row| row.id.read(cx).text(cx).trim() == id)
        })
        .unwrap_or_else(|| panic!("a row should hold `{id}`"))
}

fn set_field(field: &Entity<Editor>, text: &str, window: &mut Window, cx: &mut App) {
    field.update(cx, |field, cx| field.set_text(text, window, cx));
}

const TEMPLATE_FENCE: &str = indoc! {"
    ```mermaid
    block-betaˇ
        %% columns で列の数を決め、space は空きマス、--> で矢印
        columns 3
        input[\"入力\"] process[\"処理\"] output[\"出力\"]
        space log[\"記録\"] space
        input --> process
        process --> output
        process --> log
    ```
"};

/// The template `markdown::InsertMermaidDiagram` writes is read from the action
/// rather than copied here, so that a template which drifts away from what the
/// model emits turns this red.
#[test]
fn the_insert_template_is_the_source_the_model_writes_back() {
    let template = editor::actions::MermaidDiagramKind::Block.template();
    let document = parse_block(template)
        .unwrap_or_else(|error| panic!("the insert template should open: {error}"));
    assert_eq!(emit_block(&document), template);
    assert_eq!(document.structure_error(), None);
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
        "an unedited block diagram must be written back exactly as it was"
    );
}

/// Renaming a block carries into its edges, a new block goes at the end of
/// the diagram, and a block moved into a group is written inside it.
#[gpui::test]
async fn blocks_edges_and_groups_are_written_back(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            let process = row_index(modal, "process", cx);
            let Some((id, label, width)) = modal.block_form().and_then(|form| {
                form.entries
                    .get(process)
                    .map(|row| (row.id.clone(), row.label.clone(), row.width.clone()))
            }) else {
                panic!("the process row should have fields");
            };
            set_field(&id, "処理", window, cx);
            set_field(&label, "加工する", window, cx);
            set_field(&width, "2", window, cx);

            modal.add_block_item(BlockItemKind::Group, window, cx);
            modal.add_block_item(BlockItemKind::Node, window, cx);
            let added = modal.block_form().map(|form| form.entries.len() - 1);
            if let Some(added) = added {
                modal.indent_block_entry(added, cx);
            }
            modal.add_block_edge(window, cx);
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        indoc! {"
            ```mermaid
            block-beta
                %% columns で列の数を決め、space は空きマス、--> で矢印
                columns 3
                input[\"入力\"] 処理[\"加工する\"]:2 output[\"出力\"]
                space log[\"記録\"] space
                block:group
                    node[\"新しいブロック\"]
                end
                input --> 処理
                処理 --> output
                処理 --> log
                input --> 処理
            ```
        "},
    );
}

/// Deleting a block takes the edges that name it along; a duplicate id is
/// reported and nothing is written.
#[gpui::test]
async fn what_would_be_drawn_differently_is_refused(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;
    let before = cx.update_editor(|editor, _window, cx| editor.text(cx));

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            let output = row_index(modal, "output", cx);
            let id = modal
                .block_form()
                .and_then(|form| form.entries.get(output))
                .map(|row| row.id.clone());
            if let Some(id) = id {
                set_field(&id, "input", window, cx);
            }
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
            .is_some_and(|message| message.contains("`input` が 2 つ")),
        "the modal should name the duplicate id, got {message:?}"
    );

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            let output = row_index(modal, "input", cx);
            // The second `input` row, which is the renamed `output`.
            let output = modal
                .block_form()
                .and_then(|form| {
                    form.entries
                        .iter()
                        .enumerate()
                        .skip(output + 1)
                        .find(|(_, row)| row.id.read(cx).text(cx).trim() == "input")
                        .map(|(index, _)| index)
                })
                .unwrap_or(output);
            modal.remove_block_entry(output, cx);
            let process = row_index(modal, "process", cx);
            modal.remove_block_entry(process, cx);
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });
    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        indoc! {"
            ```mermaid
            block-beta
                %% columns で列の数を決め、space は空きマス、--> で矢印
                columns 3
                input[\"入力\"]
                space log[\"記録\"] space
            ```
        "},
    );
}

/// A rename that is still being typed — to an id another block has, or to no
/// id at all — is not carried into the edges when a button reads the fields,
/// so that typing the id back leaves every edge on the block it was on.
#[gpui::test]
async fn a_rename_through_a_duplicate_id_leaves_the_edges_alone(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            let fields = |modal: &MermaidEditor, id: &str, cx: &App| {
                let index = row_index(modal, id, cx);
                modal
                    .block_form()
                    .and_then(|form| form.entries.get(index))
                    .map(|row| row.id.clone())
                    .unwrap_or_else(|| panic!("the `{id}` row should have an id field"))
            };
            let output = fields(modal, "output", cx);
            let process = fields(modal, "process", cx);
            set_field(&output, "input", window, cx);
            set_field(&process, "", window, cx);
            // Adding a row reads every field into the rows.
            modal.add_block_item(BlockItemKind::Space, window, cx);
            set_field(&output, "output", window, cx);
            set_field(&process, "process", window, cx);
            let last = modal.block_form().map(|form| form.entries.len() - 1);
            if let Some(last) = last {
                modal.remove_block_entry(last, cx);
            }
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        TEMPLATE_FENCE.replace('\u{2c7}', ""),
        "typing the ids back should leave the diagram as it was"
    );
}

/// Deleting the row of a block renamed to another block's id takes only its
/// own edges along.
#[gpui::test]
async fn deleting_a_row_renamed_to_a_duplicate_keeps_the_other_blocks_edges(
    cx: &mut TestAppContext,
) {
    let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            let output = row_index(modal, "output", cx);
            let id = modal
                .block_form()
                .and_then(|form| form.entries.get(output))
                .map(|row| row.id.clone());
            if let Some(id) = id {
                set_field(&id, "input", window, cx);
            }
            modal.remove_block_entry(output, cx);
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        indoc! {"
            ```mermaid
            block-beta
                %% columns で列の数を決め、space は空きマス、--> で矢印
                columns 3
                input[\"入力\"] process[\"処理\"]
                space log[\"記録\"] space
                input --> process
                process --> log
            ```
        "},
    );
}
