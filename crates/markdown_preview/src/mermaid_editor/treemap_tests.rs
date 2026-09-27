use super::*;
use editor::test::editor_test_context::EditorTestContext;
use gpui::TestAppContext;
use indoc::indoc;
use mermaid_render::mermaid_model::parse_document;
use mermaid_render::treemap_model::{emit_treemap, parse_treemap};
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

fn row(modal: &MermaidEditor, index: usize) -> &TreemapRow {
    modal
        .treemap()
        .and_then(|form| form.rows.get(index))
        .unwrap_or_else(|| panic!("item {index} should have a row"))
}

fn row_names(modal: &MermaidEditor, cx: &App) -> Vec<(usize, String)> {
    modal
        .treemap()
        .map(|form| {
            form.rows
                .iter()
                .map(|row| (row.entry.depth, row.name.read(cx).text(cx)))
                .collect()
        })
        .unwrap_or_default()
}

const TEMPLATE_FENCE: &str = indoc! {"
    ```mermaid
    treemap-betaˇ
        %% 字下げで入れ子。末端の項目に : で値を付ける
        \"開発費\"
            \"設計\": 30
            \"実装\": 50
        \"運用費\"
            \"保守\": 15
            \"監視\": 5
    ```
"};

/// The template `markdown::InsertMermaidDiagram` writes is read from the action
/// rather than copied here, so that a template which drifts away from what the
/// model emits turns this red.
#[test]
fn the_insert_template_is_the_source_the_model_writes_back() {
    let template = editor::actions::MermaidDiagramKind::Treemap.template();
    let treemap = parse_treemap(template)
        .unwrap_or_else(|error| panic!("the insert template should open: {error}"));
    assert_eq!(emit_treemap(&treemap), template);
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
        "an unedited treemap must be written back exactly as it was"
    );
}

/// The fields travel with their items through a move, a class picked for an
/// item follows the class when it is renamed, and the fence comes out as the
/// forest the rows describe.
#[gpui::test]
async fn rows_and_classes_follow_their_items(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            // 運用費 goes first, taking its leaves with it.
            modal.move_treemap_item(3, false, cx);
            assert_eq!(
                row_names(modal, cx),
                vec![
                    (0, "運用費".to_string()),
                    (1, "保守".to_string()),
                    (1, "監視".to_string()),
                    (0, "開発費".to_string()),
                    (1, "設計".to_string()),
                    (1, "実装".to_string()),
                ]
            );
            let value = row(modal, 1).value.clone();
            value.update(cx, |value, cx| value.set_text("1,200", window, cx));
            // A leaf takes nothing under it, so these do nothing.
            modal.add_treemap_child(1, window, cx);
            modal.indent_treemap_item(2, cx);

            modal.add_treemap_class_def(window, cx);
            modal.set_treemap_class(3, Some("class1".to_string()), cx);
            let name = modal
                .treemap()
                .and_then(|form| form.class_defs.first())
                .map(|class_def| class_def.name.clone())
                .expect("the class was added");
            name.update(cx, |name, cx| name.set_text("hot", window, cx));
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        indoc! {"
            ```mermaid
            treemap-beta
                %% 字下げで入れ子。末端の項目に : で値を付ける
                \"運用費\"
                    \"保守\": 1,200
                    \"監視\": 5
                \"開発費\":::hot
                    \"設計\": 30
                    \"実装\": 50
                classDef hot fill:#f96;
            ```
        "},
    );
}

/// A value on an item that holds others would make Mermaid put them beside it,
/// so the treemap is not written and the modal says why. Removing a class
/// takes it off the items that used it.
#[gpui::test]
async fn a_value_over_a_section_is_refused(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;
    let before = cx.update_editor(|editor, _window, cx| editor.text(cx));

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            let value = row(modal, 0).value.clone();
            value.update(cx, |value, cx| value.set_text("100", window, cx));
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });
    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        before,
        "a treemap that cannot be written back must not be written"
    );
    let message = cx.update(|_window, cx| modal.read(cx).message.clone());
    assert!(
        message
            .as_deref()
            .is_some_and(|message| message.contains("値のある項目 `開発費` の下")),
        "the modal should say why it refused, got {message:?}"
    );

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            let value = row(modal, 0).value.clone();
            value.update(cx, |value, cx| value.set_text("", window, cx));
            modal.add_treemap_class_def(window, cx);
            modal.set_treemap_class(0, Some("class1".to_string()), cx);
            modal.remove_treemap_class_def(0, cx);
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });
    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        before,
        "a class that was added and removed again leaves nothing behind"
    );
}

/// A class name typed over another definition's on the way to a new one must
/// not hand that class's items over when a button is pressed in between: they
/// wait until the name is unique again and then follow the rename.
#[gpui::test]
async fn a_passing_duplicate_class_name_moves_no_item(cx: &mut TestAppContext) {
    let fence = indoc! {"
        ```mermaid
        treemap-betaˇ
            \"開発費\"
                \"設計\": 30:::hot
                \"実装\": 50:::cold
            classDef hot fill:#f96;
            classDef cold fill:#69f;
        ```
    "};
    let (mut cx, modal, _app_state) = open_on(fence, cx).await;
    let before = cx.update_editor(|editor, _window, cx| editor.text(cx));

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            let name = modal
                .treemap()
                .and_then(|form| form.class_defs.first())
                .map(|class_def| class_def.name.clone())
                .expect("the fence defines two classes");
            name.update(cx, |name, cx| name.set_text("cold", window, cx));
            modal.move_treemap_class_def(0, true, cx);
            modal.move_treemap_class_def(1, false, cx);
            name.update(cx, |name, cx| name.set_text("warm", window, cx));
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        before
            .replace(":::hot", ":::warm")
            .replace("classDef hot", "classDef warm")
    );
}
