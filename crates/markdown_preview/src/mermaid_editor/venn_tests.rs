use super::*;
use editor::test::editor_test_context::EditorTestContext;
use gpui::TestAppContext;
use indoc::indoc;
use mermaid_render::mermaid_model::parse_document;
use mermaid_render::venn_model::{emit_venn, parse_venn};
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

fn id_field(modal: &MermaidEditor, index: usize) -> Entity<Editor> {
    modal
        .venn()
        .and_then(|form| form.areas.get(index))
        .and_then(|row| row.id.clone())
        .unwrap_or_else(|| panic!("area {index} should be a set with an id field"))
}

const TEMPLATE_FENCE: &str = indoc! {r#"
    ```mermaid
    venn-betaˇ
        %% set で集合、union で重なりに名前を付ける
        title 担当の重なり
        set Front["フロントエンド"]
        set Back["バックエンド"]
        union Front,Back["API"]
    ```
"#};

/// The template `markdown::InsertMermaidDiagram` writes is read from the action
/// rather than copied here, so that a template which drifts away from what the
/// model emits turns this red.
#[test]
fn the_insert_template_is_the_source_the_model_writes_back() {
    let check = |template: &str| {
        let venn = parse_venn(template)
            .unwrap_or_else(|error| panic!("the insert template should open: {error}"));
        assert_eq!(emit_venn(&venn), template);
    };
    check(editor::actions::MermaidDiagramKind::Venn.template());

    // The fences these tests open are the Japanese template, which is what a
    // user with the Japanese catalog inserts.
    let _japanese = i18n::test_support::LocaleOverride::new(Some(
        i18n_catalog::test_support::bundled_catalog("ja"),
    ));
    let template = editor::actions::MermaidDiagramKind::Venn.template();
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
        "an unedited Venn diagram must be written back exactly as it was"
    );
}

/// Renaming a set carries into the overlap, a new set lands after the others
/// and can be picked into the overlap, and a Japanese id is quoted.
#[gpui::test]
async fn sets_and_overlaps_are_written_back(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            id_field(modal, 0).update(cx, |field, cx| field.set_text("画面", window, cx));
            modal.add_venn_set(window, cx);
            modal.toggle_venn_member(3, "S1".to_string(), cx);
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        indoc! {r#"
            ```mermaid
            venn-beta
                %% set で集合、union で重なりに名前を付ける
                title 担当の重なり
                set "画面"["フロントエンド"]
                set Back["バックエンド"]
                set S1
                union "画面",Back,S1["API"]
            ```
        "#},
    );
}

#[gpui::test]
async fn deleting_a_set_deletes_its_overlaps(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            modal.remove_venn_area(1, cx);
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        indoc! {r#"
            ```mermaid
            venn-beta
                %% set で集合、union で重なりに名前を付ける
                title 担当の重なり
                set Front["フロントエンド"]
            ```
        "#},
    );
}

/// An overlap cannot lose its second set, and two sets with one id are
/// reported; nothing is written.
#[gpui::test]
async fn what_mermaid_cannot_draw_is_refused(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;
    let before = cx.update_editor(|editor, _window, cx| editor.text(cx));

    cx.update(|_window, cx| {
        modal.update(cx, |modal, cx| {
            modal.toggle_venn_member(2, "Back".to_string(), cx);
        })
    });
    let message = cx.update(|_window, cx| modal.read(cx).message.clone());
    assert!(
        message
            .as_deref()
            .is_some_and(|message| message.contains("at least two sets")),
        "an overlap of one set should be refused, got {message:?}"
    );

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            id_field(modal, 1).update(cx, |field, cx| field.set_text("Front", window, cx));
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });
    let message = cx.update(|_window, cx| modal.read(cx).message.clone());
    assert!(
        message
            .as_deref()
            .is_some_and(|message| message.contains("is written twice")),
        "two sets with one id should be refused, got {message:?}"
    );
    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        before
    );
}

/// An id typed over another set's on the way to a new one must not hand that
/// set's overlaps over when a button is pressed in between: they wait until
/// the id is unique again and then follow the rename.
#[gpui::test]
async fn a_passing_duplicate_id_moves_no_overlap(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;
    let before = cx.update_editor(|editor, _window, cx| editor.text(cx));

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            let id = id_field(modal, 0);
            id.update(cx, |id, cx| id.set_text("Back", window, cx));
            modal.add_venn_set(window, cx);
            modal.remove_venn_area(2, cx);
            id.update(cx, |id, cx| id.set_text("Web", window, cx));
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        before
            .replace("set Front[", "set Web[")
            .replace("union Front,Back", "union Web,Back")
    );
}
