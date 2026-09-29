use super::*;
use editor::test::editor_test_context::EditorTestContext;
use gpui::TestAppContext;
use indoc::indoc;
use mermaid_render::mermaid_model::parse_document;
use mermaid_render::mindmap_model::{emit_mindmap, parse_mindmap};
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

fn row(modal: &MermaidEditor, index: usize) -> &MindmapRow {
    modal
        .mindmap()
        .and_then(|form| form.rows.get(index))
        .unwrap_or_else(|| panic!("node {index} should have a row"))
}

fn row_labels(modal: &MermaidEditor, cx: &App) -> Vec<(usize, String)> {
    modal
        .mindmap()
        .map(|form| {
            form.rows
                .iter()
                .map(|row| (row.entry.depth, row.label.read(cx).text(cx)))
                .collect()
        })
        .unwrap_or_default()
}

const TEMPLATE_FENCE: &str = indoc! {"
    ```mermaid
    mindmapˇ
      root((中心テーマ))
        分類A
          項目1
          項目2
        分類B
          項目3
    ```
"};

/// The template `markdown::InsertMermaidDiagram` writes is read from the action
/// rather than copied here, so that a template which drifts away from what the
/// model emits turns this red.
#[test]
fn the_insert_template_is_the_source_the_model_writes_back() {
    let check = |template: &str| {
        let map = parse_mindmap(template)
            .unwrap_or_else(|error| panic!("the insert template should open: {error}"));
        assert_eq!(emit_mindmap(&map), template);
    };
    check(editor::actions::MermaidDiagramKind::Mindmap.template());

    // The fences these tests open are the Japanese template, which is what a
    // user with the Japanese catalog inserts.
    let _japanese = i18n::test_support::LocaleOverride::new(Some(
        i18n_catalog::test_support::bundled_catalog("ja"),
    ));
    let template = editor::actions::MermaidDiagramKind::Mindmap.template();
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
        "an unedited mindmap must be written back exactly as it was"
    );
}

/// The fields travel with their nodes: after a move, an indent and a removal
/// every row still shows the text of the node it stands for, and the fence
/// comes out as the tree the rows describe.
#[gpui::test]
async fn rows_carry_their_fields_through_structural_edits(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            // 分類B goes above 分類A, taking 項目3 with it.
            modal.move_mindmap_node(4, false, cx);
            assert_eq!(
                row_labels(modal, cx),
                vec![
                    (0, "中心テーマ".to_string()),
                    (1, "分類B".to_string()),
                    (2, "項目3".to_string()),
                    (1, "分類A".to_string()),
                    (2, "項目1".to_string()),
                    (2, "項目2".to_string()),
                ]
            );
            // 項目2 goes under 項目1, then 分類B and its child are removed.
            modal.indent_mindmap_node(5, cx);
            modal.remove_mindmap_node(1, cx);
            // A new child of the root, renamed and given a shape and an icon.
            modal.add_mindmap_child(0, window, cx);
            let added = row(modal, 4);
            let (label, icon) = (added.label.clone(), added.icon.clone());
            label.update(cx, |label, cx| label.set_text("追加 ] した", window, cx));
            icon.update(cx, |icon, cx| icon.set_text("fa fa-star", window, cx));
            modal.set_mindmap_shape(4, MindmapShape::Square, cx);
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        indoc! {"
            ```mermaid
            mindmap
              root((中心テーマ))
                分類A
                  項目1
                    項目2
                [\"追加 ] した\"]
                ::icon(fa fa-star)
            ```
        "},
    );
}

/// A label the default shape cannot hold is reported and nothing is written;
/// the same label in brackets goes through.
#[gpui::test]
async fn a_label_the_shape_cannot_hold_is_refused(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;
    let before = cx.update_editor(|editor, _window, cx| editor.text(cx));

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            let label = row(modal, 1).label.clone();
            label.update(cx, |label, cx| label.set_text("分類 (A)", window, cx));
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });
    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        before,
        "a label that would be read back as a different node must not be written"
    );
    let message = cx.update(|_window, cx| modal.read(cx).message.clone());
    assert!(
        message
            .as_deref()
            .is_some_and(|message| message.contains("cannot be written in the default shape")),
        "the modal should say why it refused, got {message:?}"
    );

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            modal.set_mindmap_shape(1, MindmapShape::Rounded, cx);
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });
    let text = cx.update_editor(|editor, _window, cx| editor.text(cx));
    assert!(
        text.contains("\n    (\"分類 (A)\")\n"),
        "the label should be quoted inside its new brackets:\n{text}"
    );
}

/// The root can neither be removed nor given a sibling, and a node cannot be
/// outdented past the root's children, so the buttons that would do so are
/// no-ops rather than ways to build a second root.
#[gpui::test]
async fn the_root_stays_the_only_root(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;
    let before = cx.update_editor(|editor, _window, cx| editor.text(cx));

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            modal.remove_mindmap_node(0, cx);
            modal.add_mindmap_sibling(0, window, cx);
            modal.outdent_mindmap_node(1, cx);
            modal.indent_mindmap_node(0, cx);
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });
    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        before
    );
}
