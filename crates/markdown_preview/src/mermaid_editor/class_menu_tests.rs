use std::sync::Arc;

use editor::test::editor_test_context::EditorTestContext;
use gpui::{AppContext as _, Entity, TestAppContext};
use indoc::indoc;
use mermaid_render::mermaid_model::{MermaidDocument, parse_document};
use mermaid_render::render_to_svg;

use super::super::{Form, MermaidEditor, unsupported_kind_message};
use super::{class_candidates, declared_class_names};

fn init_test(cx: &mut TestAppContext) {
    cx.update(|cx| {
        workspace::AppState::test(cx);
        editor::init(cx);
        crate::init(cx);
    });
}

fn markdown_language() -> Arc<language::Language> {
    Arc::new(language::Language::new(
        language::LanguageConfig {
            name: "Markdown".into(),
            ..Default::default()
        },
        None,
    ))
}

/// An editor holding `state` with the modal opened on the fence the cursor
/// marker sits in.
async fn open_on(
    state: &str,
    cx: &mut TestAppContext,
) -> (EditorTestContext, Entity<MermaidEditor>) {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
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
    (cx, modal)
}

/// The body of the one fence in the buffer, exactly as it was written back.
fn fence_body(cx: &mut EditorTestContext) -> String {
    let text = cx.update_editor(|editor, _window, cx| editor.text(cx));
    let Some(body) = text
        .strip_prefix("```mermaid\n")
        .and_then(|rest| rest.strip_suffix("```\n"))
    else {
        panic!("the fence was not written back: {text}");
    };
    body.trim_end_matches('\n').to_string()
}

fn assert_renders(source: &str) {
    let svg = render_to_svg(source, &crate::markdown_export::print_mermaid_theme())
        .unwrap_or_else(|error| panic!("the written diagram should render: {error}\n{source}"));
    assert!(
        !svg.contains("Syntax error in text"),
        "the written diagram rendered an error:\n{source}"
    );
}

#[test]
fn the_menu_offers_declared_assigned_and_typed_names_once() {
    let declared = declared_class_names(
        [
            "    classDef warn,info fill:#f96;",
            "classDef bad!name fill:#fff",
            "    %% classDef hidden fill:#fff",
        ],
        |name| name.chars().all(char::is_alphanumeric) && !name.is_empty(),
    );
    assert_eq!(declared, vec!["warn".to_string(), "info".to_string()]);
    let candidates = class_candidates(
        declared,
        &["info".to_string(), "done".to_string()],
        " typed ",
        |name| !name.is_empty(),
    );
    assert_eq!(candidates, vec!["warn", "info", "done", "typed"]);
}

#[test]
fn an_unsupported_kind_lists_the_kinds_that_open() {
    let message = unsupported_kind_message();
    assert!(
        message.contains("テキストのまま編集してください"),
        "{message}"
    );
    for kind in mermaid_render::mermaid_model::MermaidDiagramKind::ALL {
        assert!(message.contains(kind.label()), "{message}");
    }
}

/// A class given with `:::` keeps that spelling, a new one is written as a
/// `class` statement at the end of the diagram, and taking one away removes
/// it from wherever the fence gave it, including a state inside a composite
/// state.
#[gpui::test]
async fn ticking_a_state_class_writes_and_removes_it(cx: &mut TestAppContext) {
    let (mut cx, modal) = open_on(
        indoc! {"
            ```mermaid
            stateDiagram-v2ˇ
                [*] --> 待機:::warn
                state 処理 {
                    実行 --> 完了
                }
                待機 --> 処理
                class 実行,完了 done
                classDef warn fill:#f96
                classDef done fill:#9f6
            ```
        "},
        cx,
    )
    .await;

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            let Form::State(form) = &modal.form else {
                panic!("the fence holds a state diagram");
            };
            assert_eq!(form.class_candidates(cx), vec!["warn", "done"]);
            modal.toggle_state_class("待機", "warn", window, cx);
            modal.toggle_state_class("実行", "done", window, cx);
            modal.toggle_state_class("完了", "warn", window, cx);
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    let body = fence_body(&mut cx);
    assert_eq!(
        body,
        indoc! {"
            stateDiagram-v2
                [*] --> 待機
                state 処理 {
                    実行 --> 完了
                }
                待機 --> 処理
                class 完了 done
                classDef warn fill:#f96
                classDef done fill:#9f6
                class 完了 warn"}
    );
    assert_renders(&body);
}

/// An entity's classes are written inline, whichever way the fence gave
/// them, and a trailing comment rides through the modal with its row.
#[gpui::test]
async fn ticking_an_entity_class_writes_it_inline(cx: &mut TestAppContext) {
    let (mut cx, modal) = open_on(
        indoc! {"
            ```mermaid
            erDiagramˇ
                CUSTOMER ||--o{ ORDER : places %% 注文のメモ
                CUSTOMER {
                    int id PK %% 主キー
                }
                classDef warn fill:#f96
            ```
        "},
        cx,
    )
    .await;

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            let Some(form) = modal.er() else {
                panic!("the fence holds an ER diagram");
            };
            let class_input = form.class_input.clone();
            class_input.update(cx, |field, cx| field.set_text("hot", window, cx));
            let Some(form) = modal.er() else {
                panic!("the fence holds an ER diagram");
            };
            assert_eq!(form.class_candidates(cx), vec!["warn", "hot"]);
            modal.toggle_entity_class(0, "warn", cx);
            modal.toggle_entity_class(1, "hot", cx);
            modal.toggle_entity_class(1, "hot", cx);
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    let body = fence_body(&mut cx);
    assert_eq!(
        body,
        indoc! {"
            erDiagram
                CUSTOMER:::warn {
                    int id PK %% 主キー
                }
                ORDER
                CUSTOMER ||--o{ ORDER : places %% 注文のメモ
                classDef warn fill:#f96"}
    );
    assert_renders(&body);
    let Ok(MermaidDocument::Er(document)) = parse_document(&body) else {
        panic!("the written fence should reopen:\n{body}");
    };
    assert_eq!(
        document
            .entities
            .first()
            .map(|entity| entity.classes.clone()),
        Some(vec!["warn".to_string()])
    );
}

/// Taking a class away from `A` takes `A` out of a `class` statement's list
/// as a whole entry, leaving `AB`, whose name only starts the same, alone.
#[gpui::test]
async fn taking_a_state_class_away_leaves_a_longer_id_alone(cx: &mut TestAppContext) {
    let (mut cx, modal) = open_on(
        indoc! {"
            ```mermaid
            stateDiagram-v2ˇ
                A --> AB
                class AB,A done
                classDef done fill:#9f6
            ```
        "},
        cx,
    )
    .await;

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            modal.toggle_state_class("A", "done", window, cx);
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    let body = fence_body(&mut cx);
    assert_eq!(
        body,
        indoc! {"
            stateDiagram-v2
                A --> AB
                class AB done
                classDef done fill:#9f6"}
    );
    assert_renders(&body);
}
