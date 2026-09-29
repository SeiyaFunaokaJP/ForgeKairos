use super::*;
use editor::test::editor_test_context::EditorTestContext;
use gpui::TestAppContext;
use indoc::indoc;
use mermaid_render::mermaid_model::parse_document;
use mermaid_render::tree_view_model::{emit_tree_view, parse_tree_view};
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

fn name_field(modal: &MermaidEditor, index: usize) -> Entity<Editor> {
    modal
        .tree_view()
        .and_then(|form| form.rows.get(index))
        .map(|row| row.name.clone())
        .unwrap_or_else(|| panic!("name {index} should have a row"))
}

fn row_names(modal: &MermaidEditor, cx: &App) -> Vec<(usize, String)> {
    modal
        .tree_view()
        .map(|form| {
            form.rows
                .iter()
                .map(|row| (row.entry.depth, row.name.read(cx).text(cx)))
                .collect()
        })
        .unwrap_or_default()
}

const TEMPLATE_FENCE: &str = indoc! {r#"
    ```mermaid
    treeView-betaˇ
        %% 字下げで階層を表す
        "プロジェクト"
            "src"
                "main.rs"
                "lib.rs"
            "docs"
                "README.md"
    ```
"#};

/// The template `markdown::InsertMermaidDiagram` writes is read from the action
/// rather than copied here, so that a template which drifts away from what the
/// model emits turns this red.
#[test]
fn the_insert_template_is_the_source_the_model_writes_back() {
    let check = |template: &str| {
        let tree = parse_tree_view(template)
            .unwrap_or_else(|error| panic!("the insert template should open: {error}"));
        assert_eq!(emit_tree_view(&tree), template);
    };
    check(editor::actions::MermaidDiagramKind::TreeView.template());

    // The fences these tests open are the Japanese template, which is what a
    // user with the Japanese catalog inserts.
    let _japanese = i18n::test_support::LocaleOverride::new(Some(
        i18n_catalog::test_support::bundled_catalog("ja"),
    ));
    let template = editor::actions::MermaidDiagramKind::TreeView.template();
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
        "an unedited tree view must be written back exactly as it was"
    );
}

/// The fields travel with their names through moves, indents and removals,
/// the title is written before the first name, and a name that contains `"`
/// switches to `'` quotes.
#[gpui::test]
async fn rows_carry_their_fields_through_structural_edits(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            // docs goes above src, taking README.md with it.
            modal.edit_tree_view(4, OutlineEdit::MoveUp, window, cx);
            assert_eq!(
                row_names(modal, cx),
                vec![
                    (1, "プロジェクト".to_string()),
                    (2, "docs".to_string()),
                    (3, "README.md".to_string()),
                    (2, "src".to_string()),
                    (3, "main.rs".to_string()),
                    (3, "lib.rs".to_string()),
                ]
            );
            // lib.rs goes under main.rs, then docs and its child are removed.
            modal.edit_tree_view(5, OutlineEdit::Indent, window, cx);
            modal.edit_tree_view(1, OutlineEdit::Remove, window, cx);
            // src moves up to the top level, next to プロジェクト.
            modal.edit_tree_view(1, OutlineEdit::Outdent, window, cx);
            // A second top-level name, renamed to one with `"` in it.
            modal.add_tree_view_top_level(window, cx);
            let added = name_field(modal, 4);
            added.update(cx, |field, cx| field.set_text("say \"hi\"", window, cx));
            let title = modal.tree_view().map(|form| form.title.clone());
            if let Some(title) = title {
                title.update(cx, |field, cx| field.set_text("構成", window, cx));
            }
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        indoc! {r#"
            ```mermaid
            treeView-beta
                %% 字下げで階層を表す
                title 構成
                "プロジェクト"
                "src"
                    "main.rs"
                        "lib.rs"
                'say "hi"'
            ```
        "#},
    );
}

/// A name that holds both quotes cannot be written between either, so it is
/// reported and nothing is written.
#[gpui::test]
async fn a_name_with_both_quotes_is_refused(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;
    let before = cx.update_editor(|editor, _window, cx| editor.text(cx));

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            let field = name_field(modal, 1);
            field.update(cx, |field, cx| field.set_text("\"a\" と 'b'", window, cx));
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });
    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        before,
        "a name that would be read back differently must not be written"
    );
    let message = cx.update(|_window, cx| modal.read(cx).message.clone());
    assert!(
        message
            .as_deref()
            .is_some_and(|message| message.contains("cannot both be used")),
        "the modal should say why it refused, got {message:?}"
    );

    // A title with a trailing comment would lose it, so it is refused too.
    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            let field = name_field(modal, 1);
            field.update(cx, |field, cx| field.set_text("src", window, cx));
            let title = modal.tree_view().map(|form| form.title.clone());
            if let Some(title) = title {
                title.update(cx, |field, cx| field.set_text("構成 %% メモ", window, cx));
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
            .is_some_and(|message| message.contains("The title")),
        "the modal should say why it refused, got {message:?}"
    );
}

/// A bare `title` is the empty title: typing one into the field replaces it
/// rather than leaving two `title` lines, which the editor would refuse.
#[gpui::test]
async fn a_title_typed_over_a_bare_title_is_written_once(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(
        indoc! {r#"
            ```mermaid
            treeView-betaˇ
                title
                "a"
            ```
        "#},
        cx,
    )
    .await;

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            let title = modal.tree_view().map(|form| form.title.clone());
            if let Some(title) = title {
                title.update(cx, |field, cx| field.set_text("構成", window, cx));
            }
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        indoc! {r#"
            ```mermaid
            treeView-beta
                title 構成
                "a"
            ```
        "#},
    );
}
