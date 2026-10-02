use super::*;
use editor::test::editor_test_context::EditorTestContext;
use gpui::TestAppContext;
use indoc::indoc;
use mermaid_render::gitgraph_model::{emit_gitgraph, parse_gitgraph};
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

fn row(modal: &MermaidEditor, index: usize) -> &GitItemRow {
    modal
        .gitgraph()
        .and_then(|form| form.rows.get(index))
        .unwrap_or_else(|| panic!("row {index} should exist"))
}

const TEMPLATE_FENCE: &str = indoc! {r#"
    ```mermaid
    gitGraphˇ
        commit id: "初期化"
        branch feature
        checkout feature
        commit id: "機能を追加"
        checkout main
        merge feature
    ```
"#};

/// The template is read from the insert action rather than copied here, so
/// that one drifting away from what the model emits turns this red instead of
/// turning every inserted graph into a rewrite on its first confirm.
#[test]
fn the_insert_template_is_the_source_the_model_writes_back() {
    let check = |template: &str| {
        let graph = parse_gitgraph(template)
            .unwrap_or_else(|error| panic!("the insert template should open: {error}"));
        assert_eq!(emit_gitgraph(&graph), template);
        assert_eq!(graph.structure_error(), None);
    };
    check(editor::actions::MermaidDiagramKind::GitGraph.template());

    // The fences these tests open are the Japanese template, which is what a
    // user with the Japanese catalog inserts.
    let _japanese = i18n::test_support::LocaleOverride::new(Some(
        i18n_catalog::test_support::bundled_catalog("ja"),
    ));
    let template = editor::actions::MermaidDiagramKind::GitGraph.template();
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
        "an unedited git graph must be written back exactly as it was"
    );
}

/// A branch is named by the rows that check it out and merge it, and a commit
/// id by the cherry-picks that take it, so a rename in the owning row's field
/// has to reach them — including across a structural change made afterwards,
/// which reads the fields into the document first.
#[gpui::test]
async fn renames_follow_into_the_rows_that_name_them(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            let GitItemRow::Branch { name, .. } = row(modal, 1) else {
                panic!("row 1 should be the branch");
            };
            let name = name.clone();
            name.update(cx, |name, cx| name.set_text("開発", window, cx));
            let GitItemRow::Commit { id, .. } = row(modal, 3) else {
                panic!("row 3 should be a commit");
            };
            let id = id.clone();
            id.update(cx, |id, cx| id.set_text("追加", window, cx));
            modal.add_gitgraph_item(GitGraphItemKind::Commit, window, cx);
            modal.add_gitgraph_item(GitGraphItemKind::CherryPick, window, cx);
        })
    });
    let message = cx.update(|_window, cx| modal.read(cx).message.clone());
    assert_eq!(message, None);

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            let GitItemRow::Commit { id, .. } = row(modal, 0) else {
                panic!("row 0 should be a commit");
            };
            let id = id.clone();
            id.update(cx, |id, cx| id.set_text("最初", window, cx));
            // The cherry-pick named the commit before its rename; the rename
            // has to carry into it.
            let GitItemRow::Commit { id, .. } = row(modal, 3) else {
                panic!("row 3 should be a commit");
            };
            let id = id.clone();
            id.update(cx, |id, cx| id.set_text("追加2", window, cx));
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        indoc! {r#"
            ```mermaid
            gitGraph
                commit id: "最初"
                branch "開発"
                checkout "開発"
                commit id: "追加2"
                checkout main
                merge "開発"
                commit
                cherry-pick id: "追加2"
            ```
        "#},
    );
}

/// Taking out a branch that rows still check out and merge would leave them
/// naming nothing, so the form refuses it, says why, and changes nothing.
#[gpui::test]
async fn a_branch_still_in_use_is_not_removed(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;
    let before = cx.update_editor(|editor, _window, cx| editor.text(cx));

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            modal.remove_gitgraph_item(1, cx);
            modal.set_gitgraph_item_kind(1, GitGraphItemKind::Commit, window, cx);
        })
    });
    let (message, rows) = cx.update(|_window, cx| {
        let modal = modal.read(cx);
        (
            modal.message.clone(),
            modal.gitgraph().map_or(0, |form| form.rows.len()),
        )
    });
    assert!(
        message
            .as_deref()
            .is_some_and(|message| message.contains("is referenced by")),
        "the refusal should be explained, got {message:?}"
    );
    assert_eq!(rows, 6);

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            modal.confirm(&menu::SecondaryConfirm, window, cx)
        })
    });
    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        before
    );
}

/// An operation that Mermaid would reject keeps the fence as it was and says
/// which row is at fault; putting it right lets the graph be written.
#[gpui::test]
async fn an_operation_mermaid_rejects_is_not_written(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(
        indoc! {"
            ```mermaid
            gitGraph TB:ˇ
            ```
        "},
        cx,
    )
    .await;

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            modal.add_gitgraph_item(GitGraphItemKind::Branch, window, cx);
            modal.add_gitgraph_item(GitGraphItemKind::Commit, window, cx);
            modal.add_gitgraph_item(GitGraphItemKind::Merge, window, cx);
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });
    let message = cx.update(|_window, cx| modal.read(cx).message.clone());
    assert!(
        message
            .as_deref()
            .is_some_and(|message| message.contains("Operation 3 (merge)")),
        "the merge onto its own branch should be refused, got {message:?}"
    );
    assert!(
        cx.update_editor(|editor, _window, cx| editor.text(cx))
            .contains("gitGraph TB:\n```"),
        "a refused graph must leave the fence alone"
    );

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            modal.set_gitgraph_item_kind(2, GitGraphItemKind::Checkout, window, cx);
            modal.add_gitgraph_item(GitGraphItemKind::Commit, window, cx);
            modal.set_gitgraph_commit_type(3, Some(GitCommitType::Highlight), cx);
            modal.set_gitgraph_direction(Some(GitGraphDirection::BottomToTop), cx);
            modal.add_gitgraph_item(GitGraphItemKind::Merge, window, cx);
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        indoc! {"
            ```mermaid
            gitGraph BT:
                branch feature
                commit
                checkout main
                commit type: HIGHLIGHT
                merge feature
            ```
        "},
    );
}

/// Rows name branches by name, so a branch field passing through a name that
/// another branch already has — or through an empty field — must not be
/// settled into the document by a structural change made meanwhile: the rows
/// of both branches would then share one name, and the next rename would
/// carry all of them along.
#[gpui::test]
async fn a_rename_passing_through_a_taken_name_keeps_the_rows_apart(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;

    for passing_name in ["main", ""] {
        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                let GitItemRow::Branch { name, .. } = row(modal, 1) else {
                    panic!("row 1 should be the branch");
                };
                let name = name.clone();
                name.update(cx, |name, cx| name.set_text(passing_name, window, cx));
                modal.set_gitgraph_commit_type(0, Some(GitCommitType::Highlight), cx);
                modal.set_gitgraph_commit_type(0, None, cx);
            })
        });
    }
    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            let GitItemRow::Branch { name, .. } = row(modal, 1) else {
                panic!("row 1 should be the branch");
            };
            let name = name.clone();
            name.update(cx, |name, cx| name.set_text("開発", window, cx));
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    let message = cx.update(|_window, cx| modal.read(cx).message.clone());
    assert_eq!(message, None);
    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        indoc! {r#"
            ```mermaid
            gitGraph
                commit id: "初期化"
                branch "開発"
                checkout "開発"
                commit id: "機能を追加"
                checkout main
                merge "開発"
            ```
        "#},
    );
}

/// An id ending in `=` after a `<`, and a tag holding the next `>`: each is a
/// fine field, but on one line `merman` turns the `="` into `='` and reads the
/// tag into the id. The fence is left alone and the reason is shown.
#[gpui::test]
async fn fields_whose_quotes_merman_would_rewrite_are_not_written(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;
    let before = cx.update_editor(|editor, _window, cx| editor.text(cx));

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            let GitItemRow::Commit { id, tag } = row(modal, 0) else {
                panic!("row 0 should be a commit");
            };
            let (id, tag) = (id.clone(), tag.clone());
            id.update(cx, |id, cx| id.set_text("a<b=", window, cx));
            let Some(tag) = tag else {
                panic!("the commit should have a tag field");
            };
            tag.update(cx, |tag, cx| tag.set_text("c>", window, cx));
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
            .is_some_and(|message| message.contains("'")),
        "the modal should say the quotes would move, got {message:?}"
    );
}

#[test]
fn add_buttons_are_worded_whole_in_japanese() {
    let _japanese = i18n::test_support::LocaleOverride::new(Some(
        i18n_catalog::test_support::bundled_catalog("ja"),
    ));
    assert_eq!(
        GitGraphItemKind::ALL.map(add_gitgraph_item_label),
        [
            "コミットを追加",
            "ブランチを追加",
            "チェックアウトを追加",
            "マージを追加",
            "チェリーピックを追加",
        ]
    );
}
