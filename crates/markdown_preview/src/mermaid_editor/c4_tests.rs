use super::*;
use editor::test::editor_test_context::EditorTestContext;
use gpui::TestAppContext;
use indoc::indoc;
use mermaid_render::c4_model::{emit_c4, parse_c4};
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

fn row(modal: &MermaidEditor, index: usize) -> &C4Row {
    modal
        .c4()
        .and_then(|form| form.rows.get(index))
        .and_then(Option::as_ref)
        .unwrap_or_else(|| panic!("item {index} should have a row"))
}

const TEMPLATE_FENCE: &str = indoc! {r#"
    ```mermaid
    C4Contextˇ
        %% Person は利用者、System は自システム、System_Ext は外部、Rel は関係
        title 受注システムの全体像
        Person(customer, "顧客", "商品を注文する")
        System(shop, "受注システム", "注文を受け付ける")
        System_Ext(mail, "メール配信", "通知を送る")
        Rel(customer, shop, "注文する")
        Rel(shop, mail, "通知を依頼する")
    ```
"#};

/// The template `markdown::InsertMermaidDiagram` writes is read from the action
/// rather than copied here, so that a template which drifts away from what the
/// model emits turns this red.
#[test]
fn the_insert_template_is_the_source_the_model_writes_back() {
    let template = editor::actions::MermaidDiagramKind::C4.template();
    let document = parse_c4(template)
        .unwrap_or_else(|error| panic!("the insert template should open: {error}"));
    assert_eq!(emit_c4(&document), template);
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
        "an unedited C4 diagram must be written back exactly as it was"
    );
}

/// A rename reaches the relationships, a new element can change its kind and
/// move into a new boundary, and a new relationship is pointed at it.
#[gpui::test]
async fn elements_boundaries_and_relationships_are_written_back(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            let alias = row(modal, 3).alias.clone();
            alias.update(cx, |alias, cx| alias.set_text("store", window, cx));
            modal.add_c4_element(None, window, cx);
            modal.set_c4_macro(5, "Person_Ext", window, cx);
            let description = row(modal, 5).description.clone();
            description.update(cx, |field, cx| field.set_text("外部の担当者", window, cx));
            modal.add_c4_boundary(window, cx);
            modal.set_c4_parent(5, Some(6), cx);
            modal.add_c4_relation(window, cx);
            modal.set_c4_relation_end(10, false, "element1", cx);
            let label = row(modal, 10).label.clone();
            label.update(cx, |label, cx| label.set_text("連絡する", window, cx));
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        indoc! {r#"
            ```mermaid
            C4Context
                %% Person は利用者、System は自システム、System_Ext は外部、Rel は関係
                title 受注システムの全体像
                Person(customer, "顧客", "商品を注文する")
                System(store, "受注システム", "注文を受け付ける")
                System_Ext(mail, "メール配信", "通知を送る")
                Boundary(boundary1, "新しい境界") {
                    Person_Ext(element1, "新しい要素", "外部の担当者")
                }
                Rel(customer, store, "注文する")
                Rel(store, mail, "通知を依頼する")
                Rel(customer, element1, "連絡する")
            ```
        "#},
    );
}

/// Deleting an element takes the relationships that join it along, since one
/// left pointing at nothing stops the whole diagram from being drawn.
#[gpui::test]
async fn removing_an_element_removes_its_relationships(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            modal.remove_c4_item(2, cx);
            modal.move_c4_item(3, false, cx);
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        indoc! {r#"
            ```mermaid
            C4Context
                %% Person は利用者、System は自システム、System_Ext は外部、Rel は関係
                title 受注システムの全体像
                System_Ext(mail, "メール配信", "通知を送る")
                System(shop, "受注システム", "注文を受け付ける")
                Rel(shop, mail, "通知を依頼する")
            ```
        "#},
    );
}

/// An alias given twice would merge two boxes into one, and a title with a
/// comment marker would be cut short; both are refused and nothing is written.
#[gpui::test]
async fn what_would_be_drawn_differently_is_refused(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;
    let before = cx.update_editor(|editor, _window, cx| editor.text(cx));

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            let alias = row(modal, 4).alias.clone();
            alias.update(cx, |alias, cx| alias.set_text("customer", window, cx));
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
            .is_some_and(|message| message.contains("2 回宣言")),
        "the modal should name the repeated alias, got {message:?}"
    );

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            let alias = row(modal, 4).alias.clone();
            alias.update(cx, |alias, cx| alias.set_text("mail", window, cx));
            let title = modal.c4().map(|form| form.title.clone());
            if let Some(title) = title {
                title.update(cx, |title, cx| title.set_text("全体 %% 下書き", window, cx));
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
            .is_some_and(|message| message.contains("タイトル")),
        "the modal should point at the title, got {message:?}"
    );
}

/// An alias typed over another element's, then a move (which reads the fields
/// into the document), then the alias typed back: the relationships of both
/// elements end where they started, rather than the one left with the shared
/// alias losing its relationships to the one renamed back.
#[gpui::test]
async fn a_passing_duplicate_alias_moves_no_relationship(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;
    let before = cx.update_editor(|editor, _window, cx| editor.text(cx));

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            let alias = row(modal, 4).alias.clone();
            alias.update(cx, |alias, cx| alias.set_text("customer", window, cx));
            modal.move_c4_item(2, true, cx);
            alias.update(cx, |alias, cx| alias.set_text("mail", window, cx));
            modal.move_c4_item(3, false, cx);
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        before
    );
}
