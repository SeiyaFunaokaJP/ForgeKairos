use super::*;
use editor::test::editor_test_context::EditorTestContext;
use gpui::TestAppContext;
use indoc::indoc;
use mermaid_render::mermaid_model::parse_document;
use mermaid_render::sankey_model::{emit_sankey, parse_sankey};
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

fn node_field(modal: &MermaidEditor, index: usize) -> Entity<Editor> {
    modal
        .sankey()
        .and_then(|form| form.nodes.get(index))
        .map(|row| row.field.clone())
        .unwrap_or_else(|| panic!("node {index} should have a row"))
}

fn value_field(modal: &MermaidEditor, index: usize) -> Entity<Editor> {
    modal
        .sankey()
        .and_then(|form| form.flows.get(index))
        .map(|row| row.value.clone())
        .unwrap_or_else(|| panic!("flow {index} should have a row"))
}

const TEMPLATE_FENCE: &str = indoc! {"
    ```mermaid
    sankey-betaˇ
        %% 1 行が 流れ元,流れ先,量
        電力,家庭,30
        電力,工場,50
        ガス,家庭,20
        ガス,工場,10
    ```
"};

/// The template `markdown::InsertMermaidDiagram` writes is read from the action
/// rather than copied here, so that a template which drifts away from what the
/// model emits turns this red.
#[test]
fn the_insert_template_is_the_source_the_model_writes_back() {
    let template = editor::actions::MermaidDiagramKind::Sankey.template();
    let sankey = parse_sankey(template)
        .unwrap_or_else(|error| panic!("the insert template should open: {error}"));
    assert_eq!(emit_sankey(&sankey), template);
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
        "an unedited Sankey diagram must be written back exactly as it was"
    );
}

/// Renaming a node reaches every flow that names it, including a flow whose
/// end is picked afterwards and one added afterwards.
#[gpui::test]
async fn renames_values_ends_and_deletions_are_written_back(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            node_field(modal, 0).update(cx, |field, cx| field.set_text("電気", window, cx));
            value_field(modal, 1).update(cx, |field, cx| field.set_text("55", window, cx));
            modal.set_sankey_end(3, false, "家庭".to_string(), cx);
            modal.add_sankey_flow(window, cx);
            modal.add_sankey_node(window, cx);
            let new_node = modal
                .sankey()
                .and_then(|form| form.nodes.last())
                .map(|row| row.field.clone());
            if let Some(field) = new_node {
                field.update(cx, |field, cx| field.set_text("蓄電池", window, cx));
            }
            modal.set_sankey_end(4, false, "蓄電池".to_string(), cx);
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        indoc! {"
            ```mermaid
            sankey-beta
                %% 1 行が 流れ元,流れ先,量
                電気,家庭,30
                電気,工場,55
                ガス,家庭,20
                ガス,家庭,10
                電気,蓄電池,10
            ```
        "},
    );
}

#[gpui::test]
async fn deleting_a_node_deletes_its_flows(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;
    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            modal.remove_sankey_node(3, cx);
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });
    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        indoc! {"
            ```mermaid
            sankey-beta
                %% 1 行が 流れ元,流れ先,量
                電力,家庭,30
                電力,工場,50
            ```
        "},
    );
}

/// A loop, a value that is not a number and a diagram with no flow left are
/// all reported, and nothing is written.
#[gpui::test]
async fn what_mermaid_cannot_draw_is_refused(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;
    let before = cx.update_editor(|editor, _window, cx| editor.text(cx));

    let confirm_and_read_message = |cx: &mut EditorTestContext| {
        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                modal.confirm(&menu::SecondaryConfirm, window, cx)
            })
        });
        cx.update(|_window, cx| modal.read(cx).message.clone())
    };

    cx.update(|_window, cx| {
        modal.update(cx, |modal, cx| {
            modal.set_sankey_end(1, true, "工場".to_string(), cx);
        })
    });
    let message = confirm_and_read_message(&mut cx);
    assert!(
        message
            .as_deref()
            .is_some_and(|message| message.contains("循環")),
        "a flow from 工場 to itself should be refused, got {message:?}"
    );
    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        before
    );

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            modal.set_sankey_end(1, true, "電力".to_string(), cx);
            value_field(modal, 0).update(cx, |field, cx| field.set_text("-3", window, cx));
        })
    });
    let message = confirm_and_read_message(&mut cx);
    assert!(
        message
            .as_deref()
            .is_some_and(|message| message.contains("-3")),
        "a negative value should be refused, got {message:?}"
    );
    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        before
    );

    cx.update(|_window, cx| {
        modal.update(cx, |modal, cx| {
            for _ in 0..4 {
                modal.remove_sankey_flow(0, cx);
            }
        })
    });
    let message = confirm_and_read_message(&mut cx);
    assert!(
        message
            .as_deref()
            .is_some_and(|message| message.contains("流れを 1 つ以上")),
        "an empty diagram should be refused, got {message:?}"
    );
    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        before
    );
}

/// A name typed over another node's on the way to a new one must not merge
/// the two nodes when a button is pressed in between: only confirming does,
/// and a name typed further follows the rename.
#[gpui::test]
async fn a_passing_duplicate_name_merges_no_node(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;
    let before = cx.update_editor(|editor, _window, cx| editor.text(cx));

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            let name = node_field(modal, 0);
            name.update(cx, |name, cx| name.set_text("ガス", window, cx));
            modal.add_sankey_node(window, cx);
            modal.remove_sankey_node(4, cx);
            name.update(cx, |name, cx| name.set_text("電気", window, cx));
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        before.replace("電力", "電気")
    );
}
