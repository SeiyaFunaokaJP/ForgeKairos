use super::*;
use editor::test::editor_test_context::EditorTestContext;
use gpui::TestAppContext;
use indoc::indoc;
use mermaid_render::mermaid_model::parse_document;
use mermaid_render::requirement_model::{emit_requirement, parse_requirement};
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

fn requirement_row(modal: &MermaidEditor, index: usize) -> &RequirementRow {
    modal
        .requirement()
        .and_then(|form| form.requirements.get(index))
        .unwrap_or_else(|| panic!("requirement {index} should have a row"))
}

fn element_row(modal: &MermaidEditor, index: usize) -> &ElementRow {
    modal
        .requirement()
        .and_then(|form| form.elements.get(index))
        .unwrap_or_else(|| panic!("element {index} should have a row"))
}

const TEMPLATE_FENCE: &str = indoc! {"
    ```mermaid
    requirementDiagramˇ
        %% requirement は要求、element は満たす側。- satisfies -> で結ぶ
        requirement login {
            id: 1
            text: \"ログインできること\"
            risk: high
            verifymethod: test
        }
        element login_page {
            type: \"画面\"
        }
        login_page - satisfies -> login
    ```
"};

/// The template `markdown::InsertMermaidDiagram` writes is read from the action
/// rather than copied here, so that a template which drifts away from what the
/// model emits turns this red.
#[test]
fn the_insert_template_is_the_source_the_model_writes_back() {
    let template = editor::actions::MermaidDiagramKind::Requirement.template();
    let document = parse_requirement(template)
        .unwrap_or_else(|error| panic!("the insert template should open: {error}"));
    assert_eq!(emit_requirement(&document), template);
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
        "an unedited requirement diagram must be written back exactly as it was"
    );
}

/// Renaming a box carries into the relationship that names it, the menus
/// write the kind, risk and relationship type, and a new requirement and
/// relationship land after the existing ones.
#[gpui::test]
async fn fields_menus_and_renames_are_written_back(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            let name = requirement_row(modal, 0).name.clone();
            name.update(cx, |name, cx| name.set_text("ログイン", window, cx));
            let text = requirement_row(modal, 0).text.clone();
            text.update(cx, |text, cx| {
                text.set_text("#1 ログインできること", window, cx)
            });
            let element_name = element_row(modal, 0).name.clone();
            element_name.update(cx, |name, cx| name.set_text("ログイン画面", window, cx));
            let doc_ref = element_row(modal, 0).doc_ref.clone();
            doc_ref.update(cx, |doc_ref, cx| {
                doc_ref.set_text("docs/login.md", window, cx)
            });

            modal.update_requirement(0, cx, |requirement| {
                requirement.kind = RequirementKind::Functional;
                requirement.risk = Some(RequirementRisk::Medium);
                requirement.verify_method = None;
            });
            modal.add_requirement(window, cx);
            modal.add_requirement_relationship(cx);
            modal.update_requirement_relationship(1, cx, |relationship| {
                relationship.kind = RelationshipKind::Verifies;
                relationship.target = "要求".to_string();
            });
            modal.set_requirement_direction(Some(RequirementDirection::LeftToRight), cx);
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        indoc! {"
            ```mermaid
            requirementDiagram
                %% requirement は要求、element は満たす側。- satisfies -> で結ぶ
                direction LR
                functionalRequirement ログイン {
                    id: 1
                    text: \"#1 ログインできること\"
                    risk: medium
                }
                requirement 要求 {
                }
                element ログイン画面 {
                    type: \"画面\"
                    docref: docs/login.md
                }
                ログイン画面 - satisfies -> ログイン
                ログイン画面 - verifies -> 要求
            ```
        "},
    );
}

/// Deleting a box that a relationship names takes the relationship with it,
/// so the fence never names a box that is gone.
#[gpui::test]
async fn deleting_a_box_takes_its_relationships_along(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            // A rename typed but not yet confirmed is what the deletion has to
            // follow.
            let name = element_row(modal, 0).name.clone();
            name.update(cx, |name, cx| name.set_text("画面", window, cx));
            modal.remove_requirement_box(BoxList::Elements, 0, cx);
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        indoc! {"
            ```mermaid
            requirementDiagram
                %% requirement は要求、element は満たす側。- satisfies -> で結ぶ
                requirement login {
                    id: 1
                    text: \"ログインできること\"
                    risk: high
                    verifymethod: test
                }
            ```
        "},
    );
}

/// A name a relationship could not be written with, and two boxes sharing a
/// name, are reported and nothing is written.
#[gpui::test]
async fn names_mermaid_could_not_read_back_are_refused(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;
    let before = cx.update_editor(|editor, _window, cx| editor.text(cx));

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            let name = element_row(modal, 0).name.clone();
            name.update(cx, |name, cx| name.set_text("login-page", window, cx));
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
            .is_some_and(|message| message.contains("login-page")),
        "the modal should name the name it refused, got {message:?}"
    );

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            let name = element_row(modal, 0).name.clone();
            name.update(cx, |name, cx| name.set_text("login", window, cx));
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
            .is_some_and(|message| message.contains("二重")),
        "the modal should say the name is taken twice, got {message:?}"
    );
}

/// A name typed over another box's on the way to a new one must not hand that
/// box's relationships over when a button is pressed in between: they wait
/// until the name is unique again and then follow the rename.
#[gpui::test]
async fn a_passing_duplicate_name_moves_no_relationship(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;
    let before = cx.update_editor(|editor, _window, cx| editor.text(cx));

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            let name = element_row(modal, 0).name.clone();
            name.update(cx, |name, cx| name.set_text("login", window, cx));
            modal.add_requirement_element(window, cx);
            modal.remove_requirement_box(BoxList::Elements, 1, cx);
            name.update(cx, |name, cx| name.set_text("page", window, cx));
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        before.replace("login_page", "page")
    );
}
