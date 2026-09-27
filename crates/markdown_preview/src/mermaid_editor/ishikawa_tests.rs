use super::*;
use editor::test::editor_test_context::EditorTestContext;
use gpui::TestAppContext;
use indoc::indoc;
use mermaid_render::ishikawa_model::{emit_ishikawa, parse_ishikawa};
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

fn cause_field(modal: &MermaidEditor, index: usize) -> Entity<Editor> {
    modal
        .ishikawa()
        .and_then(|form| form.rows.get(index))
        .map(|row| row.text.clone())
        .unwrap_or_else(|| panic!("cause {index} should have a row"))
}

fn effect_field(modal: &MermaidEditor) -> Entity<Editor> {
    modal
        .ishikawa()
        .map(|form| form.effect.clone())
        .expect("the form is an Ishikawa form")
}

const TEMPLATE_FENCE: &str = indoc! {"
    ```mermaid
    ishikawa-betaˇ
        %% 最初の行が結果、字下げした行が要因
        不良品が多い
            人
                経験が浅い
            方法
                手順書が古い
            材料
                仕入れ先のばらつき
    ```
"};

/// The template `markdown::InsertMermaidDiagram` writes is read from the action
/// rather than copied here, so that a template which drifts away from what the
/// model emits turns this red.
#[test]
fn the_insert_template_is_the_source_the_model_writes_back() {
    let template = editor::actions::MermaidDiagramKind::Ishikawa.template();
    let fish = parse_ishikawa(template)
        .unwrap_or_else(|error| panic!("the insert template should open: {error}"));
    assert_eq!(emit_ishikawa(&fish), template);
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
        "an unedited Ishikawa diagram must be written back exactly as it was"
    );
}

/// The effect is edited in its own field, and the causes' fields travel with
/// their rows through moves, additions and removals.
#[gpui::test]
async fn the_effect_and_the_causes_are_edited_in_place(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            effect_field(modal).update(cx, |field, cx| field.set_text("納期遅れ", window, cx));
            // 材料 goes above 方法, then 方法 and its cause are removed.
            modal.edit_ishikawa(4, OutlineEdit::MoveUp, window, cx);
            modal.edit_ishikawa(4, OutlineEdit::Remove, window, cx);
            // A finer cause under 人 and a new main bone after it.
            modal.edit_ishikawa(0, OutlineEdit::AddChild, window, cx);
            cause_field(modal, 2).update(cx, |field, cx| field.set_text("疲労", window, cx));
            modal.add_ishikawa_main_bone(window, cx);
            cause_field(modal, 5).update(cx, |field, cx| field.set_text("  設備  ", window, cx));
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        indoc! {"
            ```mermaid
            ishikawa-beta
                %% 最初の行が結果、字下げした行が要因
                納期遅れ
                    人
                        経験が浅い
                        疲労
                    材料
                        仕入れ先のばらつき
                    設備
            ```
        "},
    );
}

/// Causes need an effect to hang on, and a cause that would read back as a
/// comment cannot be written, so both are reported and nothing is written.
#[gpui::test]
async fn causes_without_an_effect_and_comment_like_causes_are_refused(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;
    let before = cx.update_editor(|editor, _window, cx| editor.text(cx));

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            effect_field(modal).update(cx, |field, cx| field.set_text("", window, cx));
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
            .is_some_and(|message| message.contains("結果")),
        "the modal should say why it refused, got {message:?}"
    );

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            effect_field(modal).update(cx, |field, cx| field.set_text("不良品が多い", window, cx));
            cause_field(modal, 1).update(cx, |field, cx| field.set_text("%% メモ", window, cx));
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
            .is_some_and(|message| message.contains("%% メモ")),
        "the modal should say why it refused, got {message:?}"
    );
}
