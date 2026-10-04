use super::*;
use editor::test::editor_test_context::EditorTestContext;
use gpui::TestAppContext;
use indoc::indoc;
use mermaid_render::flowchart_model::parse_flowchart;
use mermaid_render::pie_model::PieDocument;
use mermaid_render::sequence_model::SequenceItem;
use mermaid_render::state_model::{StateDocument, StateItem};
use std::rc::Rc;
use std::sync::Arc;

#[gpui::test]
async fn adding_a_node_writes_the_fence_back(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(
        indoc! {"
            ```mermaid
            flowchart TD
              Aˇ[開始] --> B[終了]
            ```
        "},
        cx,
    )
    .await;

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            modal.add_node(window, cx);
            let Some(row) = flowchart(modal).nodes.last() else {
                panic!("the new node should have a row");
            };
            let (id, label) = (row.id.clone(), row.label.clone());
            id.update(cx, |id, cx| id.set_text("C", window, cx));
            label.update(cx, |label, cx| label.set_text("追加", window, cx));
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    let text = cx.update_editor(|editor, _window, cx| editor.text(cx));
    assert_eq!(
        text,
        indoc! {"
            ```mermaid
            flowchart TB
                A[開始]
                B[終了]
                C[追加]
                A --> B
            ```
        "},
    );
}

#[gpui::test]
async fn a_duplicate_id_is_refused_and_the_fence_is_left_alone(cx: &mut TestAppContext) {
    let source = indoc! {"
        ```mermaid
        flowchart TD
          Aˇ[開始] --> B[終了]
        ```
    "};
    let (mut cx, modal, _app_state) = open_on(source, cx).await;
    let before = cx.update_editor(|editor, _window, cx| editor.text(cx));

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            let Some(row) = flowchart(modal).nodes.last() else {
                panic!("the diagram should have two nodes");
            };
            let id = row.id.clone();
            id.update(cx, |id, cx| id.set_text("A", window, cx));
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        before,
        "a diagram with two nodes called A must not be written back"
    );
    let message = cx.update(|_window, cx| modal.read(cx).message.clone());
    assert_eq!(
        message.as_deref(),
        Some("The ID \"A\" is used more than once."),
        "the modal should say why it refused"
    );
}

#[gpui::test]
async fn an_empty_id_is_refused(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(
        indoc! {"
            ```mermaid
            flowchart TD
              Aˇ[開始] --> B[終了]
            ```
        "},
        cx,
    )
    .await;

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            let Some(row) = flowchart(modal).nodes.first() else {
                panic!("the diagram should have a node");
            };
            let id = row.id.clone();
            id.update(cx, |id, cx| id.set_text("  ", window, cx));
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    let message = cx.update(|_window, cx| modal.read(cx).message.clone());
    assert_eq!(message.as_deref(), Some("An element has an empty ID."));
}

#[gpui::test]
async fn a_group_collects_the_nodes_that_are_ticked(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(
        indoc! {"
            ```mermaid
            flowchart LR
              Aˇ --> B
            ```
        "},
        cx,
    )
    .await;

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            modal.add_subgraph(window, cx);
            let Some(row) = flowchart(modal).subgraphs.last() else {
                panic!("the new group should have a row");
            };
            let (id, title) = (row.id.clone(), row.title.clone());
            id.update(cx, |id, cx| id.set_text("g", window, cx));
            title.update(cx, |title, cx| title.set_text("まとまり", window, cx));
            modal.toggle_membership(0, "A", cx);
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        indoc! {"
            ```mermaid
            flowchart LR
                A
                B
                A --> B
                subgraph g [まとまり]
                    A
                end
            ```
        "},
    );
}

#[gpui::test]
async fn removing_a_node_takes_its_edges_and_memberships_with_it(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(
        indoc! {"
            ```mermaid
            flowchart TD
              Aˇ -->|一| B
              B -->|二| C
              subgraph g [まとまり]
                B
                C
              end
            ```
        "},
        cx,
    )
    .await;

    // The label fields are held beside the edges by index, so dropping the
    // middle node has to drop its two edges' fields as well.
    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            let removed = flowchart(modal)
                .nodes
                .iter()
                .position(|row| row.id.read(cx).text(cx) == "B")
                .expect("B should have a row");
            modal.remove_node(removed, cx);
            assert_eq!(flowchart(modal).document.edges.len(), 0);
            assert_eq!(flowchart(modal).edge_labels.len(), 0);
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        indoc! {"
            ```mermaid
            flowchart TB
                A
                C
                subgraph g [まとまり]
                    C
                end
            ```
        "},
    );
}

/// Everything but the node's own declaration names it by id, so a rename
/// that stopped at the declaration would leave the edges, the group and the
/// style statement pointing at an id Mermaid would draw as an extra box.
#[gpui::test]
async fn renaming_a_node_follows_into_everything_that_names_it(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(
        indoc! {"
            ```mermaid
            flowchart LR
              Aˇ[開始] --> B[終了]
              subgraph g [まとまり]
                A
              end
              style A fill:#f00
            ```
        "},
        cx,
    )
    .await;

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            let Some(row) = flowchart(modal).nodes.first() else {
                panic!("the diagram should have a node");
            };
            let id = row.id.clone();
            id.update(cx, |id, cx| id.set_text("Start", window, cx));
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        indoc! {"
            ```mermaid
            flowchart LR
                Start[開始]
                B[終了]
                Start --> B
                subgraph g [まとまり]
                    Start
                end
              style Start fill:#f00
            ```
        "},
    );
}

/// A `style` statement declares the node it names, so one left behind would
/// bring the deleted node back as an empty box.
#[gpui::test]
async fn deleting_a_node_takes_its_style_statement_with_it(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(
        indoc! {"
            ```mermaid
            flowchart LR
              Aˇ --> B
              style A fill:#f00
              style B fill:#0f0
            ```
        "},
        cx,
    )
    .await;

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            modal.remove_node(0, cx);
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        indoc! {"
            ```mermaid
            flowchart LR
                B
              style B fill:#0f0
            ```
        "},
    );
}

/// The modal holds a field editor per row, a subscription per field and a
/// pending preview task; none of them may hold the modal itself.
#[gpui::test]
async fn the_modal_and_its_fields_are_released(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(
        indoc! {"
            ```mermaid
            flowchart TD
              Aˇ[開始] --> B[終了]
              subgraph g [まとまり]
                A
              end
            ```
        "},
        cx,
    )
    .await;

    let released = modal.downgrade();
    let fields = cx.update(|_window, cx| {
        modal
            .read(cx)
            .fields()
            .iter()
            .map(|field| field.downgrade())
            .collect::<Vec<_>>()
    });
    assert!(!fields.is_empty(), "the form should have text fields");

    // Typing leaves a preview task in flight; dropping the modal before it
    // comes back must not keep it alive.
    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            let Some(row) = flowchart(modal).nodes.first() else {
                panic!("the diagram should have a node");
            };
            let label = row.label.clone();
            label.update(cx, |label, cx| label.set_text("途中", window, cx));
        })
    });
    drop(modal);
    cx.run_until_parked();

    released.assert_released();
    for field in fields {
        field.assert_released();
    }
}

/// Every kind builds a field editor per row; deleting a row has to let go of
/// its fields, otherwise a long editing session leaks one per deletion.
#[gpui::test]
async fn a_deleted_row_releases_its_fields(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(
        indoc! {"
            ```mermaid
            pieˇ
                \"設計\" : 30
                \"実装\" : 70
            ```
        "},
        cx,
    )
    .await;

    let removed = cx.update(|_window, cx| {
        let modal = modal.read(cx);
        let Form::Pie(form) = &modal.form else {
            panic!("the fence holds a pie chart");
        };
        let row = form.slices.first().expect("the chart has two slices");
        vec![row.label.downgrade(), row.value.downgrade()]
    });

    cx.update(|_window, cx| modal.update(cx, |modal, cx| modal.remove_slice(0, cx)));
    cx.run_until_parked();
    for field in removed {
        field.assert_released();
    }
}

#[gpui::test]
async fn removing_one_of_two_edges_keeps_the_other_label(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(
        indoc! {"
            ```mermaid
            flowchart LR
              Aˇ -->|一| B
              B -->|二| C
            ```
        "},
        cx,
    )
    .await;

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            modal.remove_edge(0, cx);
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        indoc! {"
            ```mermaid
            flowchart LR
                A
                B
                C
                B -->|二| C
            ```
        "},
    );
}

/// A group can be laid out in its own direction. The dropdown writes it onto
/// the document, and rebuilding the document from the form on confirm must
/// not drop it.
#[gpui::test]
async fn a_group_keeps_the_direction_it_was_given(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(
        indoc! {"
            ```mermaid
            flowchart TD
                subgraph g [グループ]
                    Aˇ --> B
                end
            ```
        "},
        cx,
    )
    .await;

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            let group = flowchart(modal)
                .document
                .subgraphs
                .first()
                .expect("the fence holds one group");
            assert_eq!(group.direction, None);
            modal.set_subgraph_direction(0, Some(FlowDirection::LeftToRight), cx);
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    // Read back through the parser rather than against a fixed layout, so
    // that this keeps testing the direction and not the emitter's spelling.
    let body = fence_body(&mut cx);
    let written = parse_flowchart(&body).expect("the written fence is still a flowchart");
    assert_eq!(
        written.subgraphs.first().and_then(|group| group.direction),
        Some(FlowDirection::LeftToRight),
        "{body}"
    );
}

/// A flowchart carries class membership on the node itself rather than in a
/// statement kept verbatim, so a `class A,AB cls` list follows a rename with no
/// line to rewrite — and `style`, which is kept, names one node only.
#[gpui::test]
async fn a_flowchart_class_list_follows_a_rename(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(
        indoc! {"
            ```mermaid
            flowchart LRˇ
                A[開始] --> AB[途中]
                AB --> B[終了]
                classDef 強調 fill:#f00
                class A,AB 強調
                style A fill:#f96
            ```
        "},
        cx,
    )
    .await;

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            let row = flowchart(modal)
                .nodes
                .first()
                .expect("A is the first node the diagram names");
            let id = row.id.clone();
            id.update(cx, |id, cx| id.set_text("X", window, cx));
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    let body = fence_body(&mut cx);
    let written = parse_flowchart(&body).expect("the written fence is still a flowchart");
    let classed = written
        .nodes
        .iter()
        .filter(|node| node.classes.iter().any(|class| class == "強調"))
        .map(|node| node.id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        classed,
        vec!["X", "AB"],
        "AB should have kept the class and its own id:\n{body}"
    );
    assert!(
        body.contains("style X fill:#f96"),
        "the kept `style` should have followed the rename:\n{body}"
    );
}

/// Opening a diagram and confirming it without touching anything must leave the
/// fence saying the same thing, whatever kind it is. This is the promise the
/// whole editor rests on.
#[gpui::test]
async fn confirming_without_a_change_keeps_every_kind_intact(cx: &mut TestAppContext) {
    for body in [
        "flowchart LR\n  A[開始] --> B[終了]",
        "pie showData title 内訳\n  \"設計\" : 30\n  \"実装\" : 70.5",
        indoc! {"
            stateDiagram-v2
              direction LR
              [*] --> 待機
              待機 --> 実行 : 開始
              state 実行 {
                [*] --> 前半
                --
                [*] --> 後半
              }
              実行 --> [*]
              note right of 待機 : 入力待ち
              classDef 強調 fill:#f00
        "},
        indoc! {"
            sequenceDiagram
              autonumber
              participant 利用者 as User
              actor App
              利用者->>App: 要求
              activate App
              alt 成功
                App-->>利用者: 応答
              else 失敗
                App--x利用者: 失敗
              end
              deactivate App
              Note over 利用者,App: 補足
        "},
        indoc! {"
            erDiagram
              direction LR
              顧客 {
                string 名前 PK \"主キー\"
                int 年齢
              }
              注文 {
                string id PK
              }
              顧客 ||--o{ 注文 : 発注
              顧客 }o..o| 注文 : 参照
        "},
        indoc! {"
            classDiagram
                %% <|-- は継承、*-- は合成、o-- は集約
                class Base {
                    +field: Type
                    +method()
                }
                class Derived {
                    +extra()
                }
                Base <|-- Derived
        "},
        indoc! {"
            gantt
                title 開発スケジュール
                dateFormat YYYY-MM-DD
                axisFormat %m/%d
                section 設計
                要件定義 :done, req, 2026-01-06, 5d
                基本設計 :active, design, after req, 7d
                section 実装
                実装 :impl, after design, 10d
        "},
        // The first five kinds again, spelled the way the upstream
        // documentation spells them: comments, the statements each model keeps
        // verbatim, and the groupings the form only shows.
        indoc! {"
            flowchart LR
              %% 注釈
              A[開始] --> B{判断}
              B -->|はい| C[続行]
              B -->|いいえ| D[中止]
              subgraph まとめ
                direction TB
                C
                D
              end
              classDef 強調 fill:#f9f,stroke:#333
              class A 強調
              style B fill:#bbf
        "},
        "pie\n  \"一\" : 1\n  \"二\" : 2\n  \"三\" : 3",
        indoc! {"
            stateDiagram-v2
              %% 注釈
              hide empty description
              [*] --> 静止
              静止 --> 移動
              移動 --> 静止
              移動 --> 破壊
              破壊 --> [*]
              state 分岐 <<choice>>
              note left of 静止
                複数行のノート
              end note
              classDef 危険 fill:#f00
              class 破壊 危険
        "},
        indoc! {"
            sequenceDiagram
              autonumber 10 5
              box 部門
              participant 甲
              participant 乙
              end
              participant 丙
              甲->>乙: 依頼
              乙-)丙: 転送
              loop 毎日
                丙-->>甲: 報告
              end
              opt 任意
                甲->>甲: 確認
              end
              Note right of 丙: 補足
        "},
        indoc! {"
            erDiagram
              顧客 ||--|{ 配送先 : has
              顧客 {
                string 名前
                string 住所 UK \"住所\"
              }
              配送先 {
                string 郵便番号
              }
              style 顧客 fill:#f96
        "},
    ] {
        let state = format!("```mermaid\nˇ{body}\n```\n");
        let (mut cx, modal, _app_state) = open_on(&state, cx).await;
        let before =
            parse_document(body).unwrap_or_else(|error| panic!("`{body}` should open: {error}"));

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                modal.confirm(&menu::SecondaryConfirm, window, cx)
            })
        });

        let written = fence_body(&mut cx);
        let after = parse_document(&written)
            .unwrap_or_else(|error| panic!("the written fence should reopen: {error}\n{written}"));
        assert_eq!(before, after, "confirming changed the diagram:\n{written}");
    }
}

#[gpui::test]
async fn a_pie_slice_is_added_edited_and_reordered(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(
        indoc! {"
            ```mermaid
            pie title 内訳ˇ
                \"設計\" : 30
            ```
        "},
        cx,
    )
    .await;

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            modal.add_slice(window, cx);
            let Form::Pie(form) = &modal.form else {
                panic!("the fence holds a pie chart");
            };
            let row = form.slices.last().expect("the new slice should have a row");
            let (label, value) = (row.label.clone(), row.value.clone());
            label.update(cx, |label, cx| label.set_text("実装", window, cx));
            value.update(cx, |value, cx| value.set_text("70", window, cx));
            modal.move_slice(1, false, cx);
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    assert_eq!(
        fence_body(&mut cx),
        indoc! {"
            pie title 内訳
                \"実装\" : 70
                \"設計\" : 30"},
    );
}

#[gpui::test]
async fn a_pie_value_that_is_not_a_number_is_refused(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(
        indoc! {"
            ```mermaid
            pieˇ
                \"設計\" : 30
            ```
        "},
        cx,
    )
    .await;
    let before = cx.update_editor(|editor, _window, cx| editor.text(cx));

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            let Form::Pie(form) = &modal.form else {
                panic!("the fence holds a pie chart");
            };
            let value = form
                .slices
                .first()
                .expect("the chart has a slice")
                .value
                .clone();
            value.update(cx, |value, cx| value.set_text("あ", window, cx));
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        before,
        "a chart with a value that is not a number must not be written back"
    );
    let message = cx.update(|_window, cx| modal.read(cx).message.clone());
    assert_eq!(
        message.as_deref(),
        Some("The value \"あ\" of \"設計\" is not valid (only decimal numbers of 0 or more)."),
        "the modal should say why it refused"
    );
}

/// A chart whose values are all zero has no shares to divide, and the form must
/// not print `NaN%` beside every row.
#[gpui::test]
fn a_pie_chart_of_zeroes_shows_no_share(_cx: &mut TestAppContext) {
    let document = PieDocument {
        title: None,
        show_data: false,
        slices: vec![
            mermaid_render::pie_model::PieSlice {
                label: "a".into(),
                value: "0".into(),
            },
            mermaid_render::pie_model::PieSlice {
                label: "b".into(),
                value: "0".into(),
            },
        ],
        preserved_lines: Vec::new(),
    };
    let total = document.total();
    assert_eq!(total, 0.0);
    for slice in &document.slices {
        assert_eq!(pie::share_label(slice, total), "—");
    }
}

/// Everything that names a state — a transition, a note, a `classDef`'s sibling
/// `class` line — has to follow the id when it is renamed.
#[gpui::test]
async fn renaming_a_state_follows_into_its_transitions_and_notes(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(
        indoc! {"
            ```mermaid
            stateDiagram-v2ˇ
                [*] --> 待機
                待機 --> [*]
                note right of 待機 : 入力待ち
                class 待機 強調
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
            let row = form
                .ids
                .iter()
                .find(|row| row.name == "待機")
                .expect("待機 is named into the diagram by its transitions");
            let id = row.field.clone();
            id.update(cx, |id, cx| id.set_text("Idle", window, cx));
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    let body = fence_body(&mut cx);
    assert!(!body.contains("待機"), "the old id survived:\n{body}");
    assert!(body.contains("class Idle 強調"), "{body}");
    let written: StateDocument = match parse_document(&body).expect("the fence reopens") {
        MermaidDocument::State(document) => document,
        other => panic!("expected a state diagram, got {other:?}"),
    };
    assert_eq!(
        written.state_ids(),
        vec!["Idle".to_string()],
        "the transitions should name the new id"
    );
}

/// Mermaid lets `class` and `style` address a comma separated list of states,
/// so a rename has to reach into the list and a deletion has to take one name
/// out of it rather than drop the whole statement.
#[gpui::test]
async fn a_state_in_a_comma_list_is_renamed_and_deleted_on_its_own(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(
        indoc! {"
            ```mermaid
            stateDiagram-v2ˇ
                [*] --> 待機
                待機 --> 待機中
                待機中 --> 実行
                実行 : 実行中
                実行 --> [*]
                class 待機, 待機中,実行 強調
                style 待機,実行 fill:#f96
                classDef 強調 fill:#ff0
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
            // The first entry of the list and one in the middle of it, so that
            // the entry the separators and their spaces sit around is renamed
            // as well as the one that starts the list.
            for (old, new) in [("待機", "Idle"), ("待機中", "Busy")] {
                let row = form
                    .ids
                    .iter()
                    .find(|row| row.name == old)
                    .unwrap_or_else(|| panic!("{old} is named into the diagram"));
                let field = row.field.clone();
                field.update(cx, |field, cx| field.set_text(new, window, cx));
            }
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    let body = fence_body(&mut cx);
    assert!(
        body.contains("class Idle, Busy,実行 強調"),
        "each entry should have been rewritten in place, spacing and all:\n{body}"
    );
    assert!(
        body.contains("style Idle,実行 fill:#f96"),
        "`style` takes a list too:\n{body}"
    );
    assert!(
        body.contains("classDef 強調 fill:#ff0"),
        "`classDef` names a class, not a state:\n{body}"
    );

    // Deleting one name out of a list leaves the others addressed.
    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            let index = match &modal.form {
                Form::State(form) => form
                    .document
                    .body
                    .items
                    .iter()
                    .position(|item| matches!(item, StateItem::State(entry) if entry.id == "実行"))
                    .expect("実行 is declared by a statement of its own"),
                _ => panic!("the fence holds a state diagram"),
            };
            modal.remove_state_item(Vec::new(), index, window, cx);
        })
    });

    let kept = cx.update(|_window, cx| match &modal.read(cx).form {
        Form::State(form) => form
            .document
            .body
            .items
            .iter()
            .filter_map(|item| match item {
                StateItem::Preserved(text) => Some(text.trim().to_string()),
                _ => None,
            })
            .collect::<Vec<_>>(),
        _ => panic!("the fence holds a state diagram"),
    });
    assert_eq!(
        kept,
        vec![
            "class Idle, Busy 強調".to_string(),
            "style Idle fill:#f96".to_string(),
            "classDef 強調 fill:#ff0".to_string(),
        ],
        "実行 should have left the lists without taking the others with it"
    );
}

/// A rename matches a whole entry of the list, so a state whose id is a prefix
/// of another's leaves that other one alone.
#[gpui::test]
async fn a_rename_does_not_reach_into_a_longer_id(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(
        indoc! {"
            ```mermaid
            stateDiagram-v2ˇ
                [*] --> A
                A --> AB
                AB --> [*]
                class A,AB 強調
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
            let row = form
                .ids
                .iter()
                .find(|row| row.name == "A")
                .expect("A is named into the diagram by its transitions");
            let field = row.field.clone();
            field.update(cx, |field, cx| field.set_text("X", window, cx));
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    let body = fence_body(&mut cx);
    assert!(
        body.contains("class X,AB 強調"),
        "AB should not have been touched:\n{body}"
    );
}

/// The `--` divider separates two concurrent regions, so the items around it
/// have to keep their order and the divider has to keep its place.
#[gpui::test]
async fn a_composite_state_keeps_its_contents_and_their_order(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(
        indoc! {"
            ```mermaid
            stateDiagram-v2ˇ
                state 実行 {
                    [*] --> 前半
                    --
                    [*] --> 後半
                }
            ```
        "},
        cx,
    )
    .await;

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            modal.add_state(vec![0], window, cx);
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    let body = fence_body(&mut cx);
    let written: StateDocument = match parse_document(&body).expect("the fence reopens") {
        MermaidDocument::State(document) => document,
        other => panic!("expected a state diagram, got {other:?}"),
    };
    let Some(StateItem::Composite(composite)) = written.body.items.first() else {
        panic!("the composite state should still be there:\n{body}");
    };
    let kinds = composite
        .body
        .items
        .iter()
        .map(|item| match item {
            StateItem::Transition(_) => "transition",
            StateItem::Divider => "divider",
            StateItem::State(_) => "state",
            _ => "other",
        })
        .collect::<Vec<_>>();
    assert_eq!(
        kinds,
        vec!["transition", "divider", "transition", "state"],
        "{body}"
    );
}

/// Deleting a state drops every statement that named it, wherever it sits —
/// including inside a composite state. The fields are held beside the
/// statements by position, so their rows have to go in step with them: a row
/// left behind is read onto the statement below it, which rewrites a label the
/// user never touched.
#[gpui::test]
async fn deleting_a_state_keeps_the_fields_inside_a_composite_lined_up(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(
        indoc! {"
            ```mermaid
            stateDiagram-v2ˇ
                A : 最初
                B --> A : 外側
                state G {
                    A --> C : 内側
                    C --> D : 残す
                }
            ```
        "},
        cx,
    )
    .await;

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            modal.remove_state_item(Vec::new(), 0, window, cx);
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    let body = fence_body(&mut cx);
    let MermaidDocument::State(written) = parse_document(&body).expect("the fence reopens") else {
        panic!("expected a state diagram:\n{body}");
    };
    let Some(StateItem::Composite(composite)) = written.body.items.first() else {
        panic!("the composite state should have survived:\n{body}");
    };
    let labels = composite
        .body
        .items
        .iter()
        .map(|item| match item {
            StateItem::Transition(transition) => transition.label.clone().unwrap_or_default(),
            other => format!("{other:?}"),
        })
        .collect::<Vec<_>>();
    assert_eq!(
        labels,
        vec!["残す".to_string()],
        "the transition that survived kept its own label:\n{body}"
    );
}

#[gpui::test]
async fn a_sequence_message_is_added_and_the_block_rules_are_enforced(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(
        indoc! {"
            ```mermaid
            sequenceDiagramˇ
                participant A
                participant B
            ```
        "},
        cx,
    )
    .await;

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            modal.add_sequence_item(sequence::AddKind::Message, window, cx);
            let Form::Sequence(form) = &modal.form else {
                panic!("the fence holds a sequence diagram");
            };
            let sequence::ItemRow::Text(field) =
                form.items.last().expect("the message should have a row")
            else {
                panic!("a message row holds one text field");
            };
            let field = field.clone();
            field.update(cx, |field, cx| field.set_text("依頼", window, cx));
            // An `end` with no block in front of it is exactly what
            // `structure_error` is there to catch.
            modal.add_sequence_item(sequence::AddKind::BlockEnd, window, cx);
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    let message = cx.update(|_window, cx| modal.read(cx).message.clone());
    assert_eq!(
        message.as_deref(),
        Some("There are more `end`s than blocks."),
        "the modal should refuse a diagram that cannot be written back"
    );

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            let last = match &modal.form {
                Form::Sequence(form) => form.document.items.len() - 1,
                _ => panic!("the fence holds a sequence diagram"),
            };
            modal.remove_sequence_item(last, cx);
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    let body = fence_body(&mut cx);
    let MermaidDocument::Sequence(written) = parse_document(&body).expect("the fence reopens")
    else {
        panic!("expected a sequence diagram:\n{body}");
    };
    assert!(
        matches!(
            written.items.first(),
            Some(SequenceItem::Message(message)) if message.text == "依頼"
        ),
        "{body}"
    );
}

/// A message names its ends by participant id, so a rename that stopped at the
/// declaration would leave the message pointing at a column that is gone.
#[gpui::test]
async fn renaming_a_participant_follows_into_its_messages(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(
        indoc! {"
            ```mermaid
            sequenceDiagramˇ
                participant A
                participant B
                A->>B: 依頼
                Note over A,B: 補足
            ```
        "},
        cx,
    )
    .await;

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            let Form::Sequence(form) = &modal.form else {
                panic!("the fence holds a sequence diagram");
            };
            let id = form
                .participants
                .first()
                .expect("the diagram has two participants")
                .id
                .clone();
            id.update(cx, |id, cx| id.set_text("利用者", window, cx));
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    let body = fence_body(&mut cx);
    let MermaidDocument::Sequence(written) = parse_document(&body).expect("the fence reopens")
    else {
        panic!(
            "expected a sequence diagram:
{body}"
        );
    };
    assert!(
        matches!(
            written.items.first(),
            Some(SequenceItem::Message(message)) if message.from == "利用者"
        ),
        "{body}"
    );
    assert!(
        matches!(
            written.items.get(1),
            Some(SequenceItem::Note(note)) if note.from == "利用者"
        ),
        "{body}"
    );
}

/// A relationship left pointing at a deleted entity would name it back into the
/// diagram as an empty box, which is what the flowchart form does too.
#[gpui::test]
async fn deleting_an_entity_takes_its_relationships_with_it(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(
        indoc! {"
            ```mermaid
            erDiagramˇ
                顧客 {
                    string 名前 PK
                }
                注文 {
                    string id PK
                }
                顧客 ||--o{ 注文 : 発注
            ```
        "},
        cx,
    )
    .await;

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            modal.remove_entity(0, cx);
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    let body = fence_body(&mut cx);
    let MermaidDocument::Er(written) = parse_document(&body).expect("the fence reopens") else {
        panic!(
            "expected an ER diagram:
{body}"
        );
    };
    assert!(written.relationships.is_empty(), "{body}");
    assert_eq!(
        written
            .entities
            .iter()
            .map(|entity| entity.name.as_str())
            .collect::<Vec<_>>(),
        vec!["注文"],
        "{body}"
    );
}

#[gpui::test]
async fn renaming_an_entity_follows_into_its_relationships(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(
        indoc! {"
            ```mermaid
            erDiagramˇ
                顧客 {
                    string 名前 PK
                }
                注文 {
                    string id PK
                }
                顧客 ||--o{ 注文 : 発注
            ```
        "},
        cx,
    )
    .await;

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            let Form::Er(form) = &modal.form else {
                panic!("the fence holds an ER diagram");
            };
            let name = form
                .entities
                .first()
                .expect("the diagram has two entities")
                .name
                .clone();
            name.update(cx, |name, cx| name.set_text("Customer", window, cx));
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    let body = fence_body(&mut cx);
    let MermaidDocument::Er(written) = parse_document(&body).expect("the fence reopens") else {
        panic!("expected an ER diagram:\n{body}");
    };
    assert_eq!(
        written
            .relationships
            .first()
            .map(|relationship| relationship.left.as_str()),
        Some("Customer"),
        "{body}"
    );
}

/// A `style` is one of the statements the ER model keeps verbatim, and it names
/// the entity it is about in its second word. Left alone, a rename would strand
/// it on a name the diagram no longer declares and a deletion would leave it
/// addressing nothing.
#[gpui::test]
async fn an_entity_style_follows_a_rename_and_goes_with_a_deletion(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(
        indoc! {"
            ```mermaid
            erDiagramˇ
                顧客 ||--o{ 注文 : 発注
                在庫 {
                    int 数
                }
                style 顧客 fill:#f96
                style 在庫 fill:#9f6
            ```
        "},
        cx,
    )
    .await;

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            let Form::Er(form) = &modal.form else {
                panic!("the fence holds an ER diagram");
            };
            let name = form
                .entities
                .first()
                .expect("顧客 is the first entity the diagram names")
                .name
                .clone();
            name.update(cx, |name, cx| name.set_text("Customer", window, cx));
            modal.remove_entity(2, cx);
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    let body = fence_body(&mut cx);
    assert!(
        body.contains("style Customer fill:#f96"),
        "the style should have followed the rename:\n{body}"
    );
    assert!(
        !body.contains("在庫"),
        "the deleted entity's style stayed behind:\n{body}"
    );
}

/// Mermaid lets `style` address a comma separated list of entities, so a rename
/// has to reach into the list and a deletion has to take one name out of it
/// rather than drop the style off the entities that are still there.
#[gpui::test]
async fn an_entity_in_a_comma_list_is_renamed_and_deleted_on_its_own(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(
        indoc! {"
            ```mermaid
            erDiagramˇ
                顧客 ||--o{ 注文 : 発注
                顧客注文 {
                    int 数
                }
                style 顧客, 注文,顧客注文 fill:#f96
            ```
        "},
        cx,
    )
    .await;

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            let Form::Er(form) = &modal.form else {
                panic!("the fence holds an ER diagram");
            };
            // The first entry of the list and one in the middle of it, so that
            // the entry the separators and their spaces sit around is renamed
            // as well as the one that starts the list. 顧客 also spells the
            // start of 顧客注文, which must be left alone.
            for (index, new) in [(0, "Customer"), (1, "Order")] {
                let name = form
                    .entities
                    .get(index)
                    .unwrap_or_else(|| panic!("the diagram declares entity {index}"))
                    .name
                    .clone();
                name.update(cx, |name, cx| name.set_text(new, window, cx));
            }
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    let body = fence_body(&mut cx);
    assert!(
        body.contains("style Customer, Order,顧客注文 fill:#f96"),
        "each entry should have been rewritten in place, spacing and all:\n{body}"
    );

    // The fence has already been written once, so the staleness guard would
    // refuse a second confirm; the document the form describes is what is read.
    cx.update(|_window, cx| {
        modal.update(cx, |modal, cx| {
            let index = match &modal.form {
                Form::Er(form) => form
                    .document
                    .entity_index("顧客注文")
                    .expect("顧客注文 is declared"),
                _ => panic!("the fence holds an ER diagram"),
            };
            modal.remove_entity(index, cx);
        })
    });

    let kept = cx.update(|_window, cx| match &modal.read(cx).form {
        Form::Er(form) => form
            .current_document(cx)
            .preserved_lines
            .iter()
            .map(|line| line.text.trim().to_string())
            .collect::<Vec<_>>(),
        _ => panic!("the fence holds an ER diagram"),
    });
    assert_eq!(
        kept,
        vec!["style Customer, Order fill:#f96".to_string()],
        "顧客注文 should have left the list without taking the others with it"
    );
}

/// An empty fence has no kind of its own, so the editor asks for one and writes
/// nothing until it has been picked.
#[gpui::test]
async fn an_empty_fence_asks_which_kind_to_start(cx: &mut TestAppContext) {
    let _state = init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
    let before = indoc! {"
        # 見出し

        本文ˇ
    "};
    cx.set_state(before);

    let editor = cx.editor.clone();
    let target = cx
        .update_editor(|editor, _window, cx| editor.new_mermaid_fence_target(cx))
        .expect("a Markdown document always has somewhere to start a diagram");
    assert_eq!(target.source, "");
    let modal = cx.update(|window, cx| {
        cx.new(|cx| MermaidEditor::new(editor.downgrade(), target, None, window, cx))
    });

    // Confirming before a kind is picked writes nothing at all.
    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            modal.confirm(&menu::SecondaryConfirm, window, cx)
        })
    });
    cx.assert_editor_state(before);
    let message = cx.update(|_window, cx| modal.read(cx).message.clone());
    assert_eq!(message.as_deref(), Some("Choose a diagram type."));

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            modal.set_document(MermaidDocument::empty(MermaidDiagramKind::Pie), window, cx);
            modal.add_slice(window, cx);
            let Form::Pie(form) = &modal.form else {
                panic!("the picker should have built a pie form");
            };
            let row = form.slices.last().expect("the new slice should have a row");
            let (label, value) = (row.label.clone(), row.value.clone());
            label.update(cx, |label, cx| label.set_text("設計", window, cx));
            value.update(cx, |value, cx| value.set_text("30", window, cx));
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    cx.assert_editor_state(indoc! {"
        # 見出し

        本文

        ```mermaid
        ˇpie
            \"設計\" : 30
        ```
    "});
    // The whole fence arrives in one edit, so one undo takes it back.
    cx.update_editor(|editor, window, cx| editor.undo(&editor::actions::Undo, window, cx));
    cx.assert_editor_state(before);
}

/// `sequenceDiagram` and `erDiagram` on their own are not diagrams Mermaid
/// draws. Picking either from the kind picker and confirming before anything is
/// in them has to be refused, or the fence would be left holding a diagram the
/// preview can only report an error about.
#[gpui::test]
async fn an_empty_sequence_or_er_is_not_written_back(cx: &mut TestAppContext) {
    for (kind, expected) in [
        (
            MermaidDiagramKind::Sequence,
            "Add at least one participant.",
        ),
        (MermaidDiagramKind::Er, "Add at least one entity."),
    ] {
        let _state = init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        let before = indoc! {"
            ```mermaid
            ˇ
            ```
        "};
        cx.set_state(before);

        let editor = cx.editor.clone();
        let target = cx
            .update_editor(|editor, _window, cx| editor.mermaid_fence_target(cx))
            .expect("the cursor is inside a mermaid fence");
        assert_eq!(target.source, "");
        let modal = cx.update(|window, cx| {
            cx.new(|cx| MermaidEditor::new(editor.downgrade(), target, None, window, cx))
        });

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                modal.set_document(MermaidDocument::empty(kind), window, cx);
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        cx.assert_editor_state(before);
        let message = cx.update(|_window, cx| modal.read(cx).message.clone());
        assert_eq!(message.as_deref(), Some(expected), "{kind:?}");
    }
}

/// The tab stays open for as long as the user needs, and the fence it read
/// may have been rewritten in another pane meanwhile. Writing over that would
/// throw away an edit nobody saw.
#[gpui::test]
async fn a_fence_that_changed_while_the_editor_was_open_is_not_overwritten(
    cx: &mut TestAppContext,
) {
    let (mut cx, modal, _app_state) = open_on(
        indoc! {"
            ```mermaid
            flowchart LRˇ
                A --> B
            ```
        "},
        cx,
    )
    .await;
    set_first_node_label("開始", &modal, &mut cx);

    // A surgical edit rather than a fresh buffer, so that the fence is still
    // the one the tab was opened on and only its body has moved on.
    rewrite_in_buffer("A --> B", "A --> C", &mut cx);
    let changed = indoc! {"
        ```mermaid
        flowchart LR
            A --> C
        ```
    "};

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            modal.confirm(&menu::SecondaryConfirm, window, cx)
        })
    });

    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        changed,
        "the diagram somebody else wrote must survive"
    );
    cx.update(|_window, cx| {
        let modal = modal.read(cx);
        assert_eq!(modal.message.as_deref(), Some(source_changed_message()));
        assert!(
            modal.has_conflict(cx),
            "the pane should ask before saving over it"
        );
        assert!(!modal.has_deleted_file(cx));
        assert!(modal.is_dirty(cx), "the edit in the form is kept");
    });
}

/// Saving the tab is what the pane does after its overwrite question was
/// answered with "Overwrite", so that writes over the changed fence.
#[gpui::test]
async fn saving_writes_over_a_fence_that_changed(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(
        indoc! {"
            ```mermaid
            flowchart LRˇ
                A --> B
            ```
        "},
        cx,
    )
    .await;
    set_first_node_label("開始", &modal, &mut cx);
    rewrite_in_buffer("A --> B", "A --> C", &mut cx);

    let Some(project) = cx.update_editor(|editor, _window, _cx| editor.project().cloned()) else {
        panic!("the test editor has a project");
    };
    let save = cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            modal.save(SaveOptions::default(), project, window, cx)
        })
    });
    save.await
        .expect("an overwrite writes over the changed fence");

    assert_eq!(
        fence_body(&mut cx),
        "flowchart LR
    A[開始]
    B
    A --> B"
    );
    cx.update(|_window, cx| {
        let modal = modal.read(cx);
        assert!(!modal.is_dirty(cx));
        assert!(!modal.has_conflict(cx));
    });
}

/// A tab without edits of its own has nothing to lose, so it shows what the
/// document now says rather than a warning.
#[gpui::test]
async fn an_unedited_tab_follows_the_fence(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(
        indoc! {"
            ```mermaid
            flowchart LRˇ
                A --> B
            ```
        "},
        cx,
    )
    .await;

    rewrite_in_buffer("A --> B", "A --> C", &mut cx);
    cx.run_until_parked();

    cx.update(|_window, cx| {
        let modal = modal.read(cx);
        assert_eq!(modal.node_ids(cx), vec!["A".to_string(), "C".to_string()]);
        assert_eq!(modal.source_state, SourceState::InStep);
        assert!(!modal.is_dirty(cx));
    });

    // And it now writes to the fence as it reads after the change.
    set_first_node_label("開始", &modal, &mut cx);
    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            modal.confirm(&menu::SecondaryConfirm, window, cx)
        })
    });
    assert_eq!(
        fence_body(&mut cx),
        "flowchart LR
    A[開始]
    C
    A --> C"
    );
}

/// Applying leaves the tab open, clean, and pointed at what it wrote, so that
/// a second edit applies too rather than being refused as a change made
/// elsewhere.
#[gpui::test]
async fn applying_keeps_the_tab_open_and_in_step(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(
        indoc! {"
            ```mermaid
            flowchart LRˇ
                A --> B
            ```
        "},
        cx,
    )
    .await;
    let events = record_events(&modal, &mut cx);
    cx.update(|_window, cx| assert!(!modal.read(cx).is_dirty(cx)));

    set_first_node_label("開始", &modal, &mut cx);
    cx.update(|_window, cx| assert!(modal.read(cx).is_dirty(cx)));
    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            modal.confirm(&menu::SecondaryConfirm, window, cx)
        })
    });
    cx.run_until_parked();
    assert_eq!(
        fence_body(&mut cx),
        "flowchart LR
    A[開始]
    B
    A --> B"
    );
    cx.update(|_window, cx| {
        let modal = modal.read(cx);
        assert!(!modal.is_dirty(cx));
        assert_eq!(modal.source_state, SourceState::InStep);
        assert_eq!(modal.message, None);
    });

    set_first_node_label("始まり", &modal, &mut cx);
    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            modal.confirm(&menu::SecondaryConfirm, window, cx)
        })
    });
    cx.run_until_parked();
    assert_eq!(
        fence_body(&mut cx),
        "flowchart LR
    A[始まり]
    B
    A --> B"
    );
    assert!(
        !events.borrow().contains(&"close"),
        "applying must not close the tab"
    );
}

const FENCE_BEFORE_A_PARAGRAPH: &str = indoc! {"
    ```mermaid
    flowchart LRˇ
        A --> B
    ```
    後ろの段落
"};
const DELETED_FENCE: &str = "```mermaid\nflowchart LR\n    A --> B\n```\n";

/// The fence going away closes a tab that has nothing to lose.
#[gpui::test]
async fn a_deleted_fence_closes_an_unedited_tab(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(FENCE_BEFORE_A_PARAGRAPH, cx).await;
    let events = record_events(&modal, &mut cx);
    rewrite_in_buffer(DELETED_FENCE, "", &mut cx);
    cx.run_until_parked();
    assert_eq!(events.borrow().last().copied(), Some("close"));
}

/// One that has edits stays, marked as having lost what it writes to.
#[gpui::test]
async fn a_deleted_fence_marks_an_edited_tab(cx: &mut TestAppContext) {
    let fence = DELETED_FENCE;
    let (mut cx, modal, _app_state) = open_on(FENCE_BEFORE_A_PARAGRAPH, cx).await;
    let events = record_events(&modal, &mut cx);
    set_first_node_label("開始", &modal, &mut cx);
    rewrite_in_buffer(fence, "", &mut cx);
    cx.run_until_parked();
    assert!(!events.borrow().contains(&"close"));
    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            assert_eq!(modal.source_state, SourceState::Gone);
            assert!(modal.has_conflict(cx));
            assert!(modal.has_deleted_file(cx));
            modal.confirm(&menu::SecondaryConfirm, window, cx);
            assert_eq!(modal.message.as_deref(), Some(source_gone_message()));
        })
    });
    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        "後ろの段落\n"
    );
}

/// A fence deleted wholesale lets its anchors collapse onto whatever comes
/// next. When that is another diagram, the tab must not start writing to it.
#[gpui::test]
async fn a_deleted_fence_does_not_hand_the_tab_to_the_next_one(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(
        indoc! {"
            ```mermaid
            flowchart LRˇ
                A --> B
            ```
            ```mermaid
            flowchart LR
                X --> Y
            ```
        "},
        cx,
    )
    .await;
    set_first_node_label("開始", &modal, &mut cx);
    rewrite_in_buffer("```mermaid\nflowchart LR\n    A --> B\n```\n", "", &mut cx);
    cx.run_until_parked();

    let save = cx.update(|window, cx| modal.update(cx, |modal, cx| modal.apply(true, window, cx)));
    assert!(save.is_err(), "the diagram that is left is not this tab's");
    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        "```mermaid\nflowchart LR\n    X --> Y\n```\n"
    );
}

/// "Revert" and the pane's "Discard Edits" read the fence again.
#[gpui::test]
async fn reverting_takes_the_document_version(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(
        indoc! {"
            ```mermaid
            flowchart LRˇ
                A --> B
            ```
        "},
        cx,
    )
    .await;
    set_first_node_label("開始", &modal, &mut cx);
    rewrite_in_buffer("A --> B", "A --> C", &mut cx);
    cx.run_until_parked();

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            modal
                .revert(window, cx)
                .expect("the fence is still there to read");
        })
    });
    cx.update(|_window, cx| {
        let modal = modal.read(cx);
        assert_eq!(modal.node_ids(cx), vec!["A".to_string(), "C".to_string()]);
        assert!(!modal.is_dirty(cx));
        assert!(!modal.has_conflict(cx));
    });
}

/// Taking the fence lines away, or turning the block into another language,
/// leaves the body text under the tab's anchors exactly as it was. The tab
/// must still see that its diagram is gone rather than write Mermaid lines
/// into paragraphs or into somebody's code sample.
#[gpui::test]
async fn removing_the_fence_lines_leaves_nothing_to_write_to(cx: &mut TestAppContext) {
    assert_edited_tab_no_longer_writes(&[("```mermaid\n", ""), ("```\n", "")], cx).await;
}

#[gpui::test]
async fn a_fence_turned_into_another_language_is_not_written_to(cx: &mut TestAppContext) {
    assert_edited_tab_no_longer_writes(&[("```mermaid\n", "```text\n")], cx).await;
}

async fn assert_edited_tab_no_longer_writes(edits: &[(&str, &str)], cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(FENCE_BEFORE_A_PARAGRAPH, cx).await;
    set_first_node_label("開始", &modal, &mut cx);
    for (old, new) in edits {
        rewrite_in_buffer(old, new, &mut cx);
    }
    cx.run_until_parked();
    let before = cx.update_editor(|editor, _window, cx| editor.text(cx));

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            assert_eq!(modal.source_state, SourceState::Gone);
            assert!(
                modal.apply(true, window, cx).is_err(),
                "saving must not write into what is left"
            );
        })
    });
    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        before
    );
}

/// The same for a tab with no edits: it closes rather than staying on text
/// that is no longer a diagram.
#[gpui::test]
async fn removing_the_fence_lines_closes_an_unedited_tab(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(FENCE_BEFORE_A_PARAGRAPH, cx).await;
    let events = record_events(&modal, &mut cx);
    rewrite_in_buffer("```mermaid\n", "", &mut cx);
    rewrite_in_buffer("```\n", "", &mut cx);
    cx.run_until_parked();
    assert_eq!(events.borrow().last().copied(), Some("close"));
}

/// An empty fence has nothing under its anchors, so deleting it whole leaves
/// "nothing" there just the same. The diagram picked in the tab afterwards
/// must not be written where the fence used to be.
#[gpui::test]
async fn a_deleted_empty_fence_is_not_written_to(cx: &mut TestAppContext) {
    let _app_state = init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
    cx.set_state("前\n\n```mermaid\nˇ```\n\n後\n");
    let editor = cx.editor.clone();
    let target = cx
        .update_editor(|editor, _window, cx| editor.mermaid_fence_target(cx))
        .expect("the cursor is inside a mermaid fence");
    assert!(!target.inserts_fence());
    let modal = cx.update(|window, cx| {
        cx.new(|cx| MermaidEditor::new(editor.downgrade(), target, None, window, cx))
    });
    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            let document = parse_document("flowchart LR\n    A --> B")
                .unwrap_or_else(|error| panic!("the diagram should parse: {error}"));
            modal.set_document(document, window, cx);
        })
    });
    cx.run_until_parked();
    rewrite_in_buffer("```mermaid\n```\n", "", &mut cx);
    cx.run_until_parked();

    let result =
        cx.update(|window, cx| modal.update(cx, |modal, cx| modal.apply(true, window, cx)));
    assert!(result.is_err(), "the fence is gone");
    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        "前\n\n\n後\n"
    );
}

#[test]
fn fit_to_pane_enlarges_a_small_drawing_and_shrinks_a_large_one() {
    let frame = size(px(1000.), px(800.));
    let small = fit_scale(size(px(120.), px(60.)), frame);
    assert_eq!(
        small, PREVIEW_MAX_FIT_SCALE,
        "a tiny diagram is enlarged up to the cap"
    );
    let medium = fit_scale(size(px(400.), px(100.)), frame);
    assert!(
        (medium - (1000. - 24.) / 400.).abs() < 0.001,
        "the width limits a wide diagram: {medium}"
    );
    let large = fit_scale(size(px(3000.), px(400.)), frame);
    assert!(large < 1., "a large diagram is shrunk: {large}");
}

#[test]
fn the_raster_follows_the_zoom_in_steps_and_within_its_memory() {
    let natural = size(px(200.), px(100.));
    // At 100% on a 1x display the 2x smoothing raster is already sharp.
    assert_eq!(raster_scale_for(1., 1., natural), 1.);
    // Shown at 3x on a 1.5x display: 4.5 device pixels per natural pixel,
    // which the raster scale steps up to cover.
    assert_eq!(raster_scale_for(3., 1.5, natural), 3.);
    assert_eq!(
        raster_scale_for(2.9, 1.5, natural),
        3.,
        "steps, not every value"
    );
    // A large drawing stops where a raster would take too much memory.
    let large = size(px(2000.), px(1500.));
    assert!(raster_scale_for(4., 2., large) * 2. * 2000. <= MAX_RASTER_SIDE);
    assert!((2000. * 1500.) * (raster_scale_for(4., 2., large) * 2.).powi(2) <= MAX_RASTER_PIXELS);
}

/// Zooming starts from what is on screen, so the first "+" after "fit to
/// pane" enlarges the fitted drawing rather than jumping back to 100%.
#[gpui::test]
async fn zooming_steps_from_the_fitted_scale(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(FENCE_BEFORE_A_PARAGRAPH, cx).await;
    cx.update(|_window, cx| {
        modal.update(cx, |modal, cx| {
            modal.preview.natural_size = Some(size(px(100.), px(50.)));
            modal.preview_frame.set(Some(size(px(424.), px(424.))));
            assert_eq!(modal.preview_display_scale(), Some(4.));
            modal.zoom_preview_by(-1., cx);
            assert_eq!(
                modal.preview_zoom,
                PreviewZoom::Scale(4. / PREVIEW_ZOOM_STEP)
            );
            modal.zoom_preview_by(100., cx);
            assert_eq!(modal.preview_zoom, PreviewZoom::Scale(PREVIEW_MAX_SCALE));
        })
    });
}

fn set_first_node_label(
    text: &'static str,
    modal: &Entity<MermaidEditor>,
    cx: &mut EditorTestContext,
) {
    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            let Some(row) = flowchart(modal).nodes.first() else {
                panic!("the diagram should have a node");
            };
            let label = row.label.clone();
            label.update(cx, |label, cx| label.set_text(text, window, cx));
        })
    });
    cx.run_until_parked();
}

/// Rewrites the first `old` in the buffer, the way another pane would.
fn rewrite_in_buffer(old: &str, new: &str, cx: &mut EditorTestContext) {
    cx.update_buffer(|buffer, cx| {
        let text = buffer.text();
        let offset = text
            .find(old)
            .unwrap_or_else(|| panic!("the buffer should hold {old:?}"));
        buffer.edit([(offset..offset + old.len(), new)], None, cx);
    });
}

fn record_events(
    modal: &Entity<MermaidEditor>,
    cx: &mut EditorTestContext,
) -> Rc<std::cell::RefCell<Vec<&'static str>>> {
    let events = Rc::new(std::cell::RefCell::new(Vec::new()));
    cx.update(|_window, cx| {
        cx.subscribe(modal, {
            let events = events.clone();
            move |_, event: &MermaidEditorEvent, _| {
                events.borrow_mut().push(match event {
                    MermaidEditorEvent::TabChanged => "tab changed",
                    MermaidEditorEvent::Close => "close",
                });
            }
        })
        .detach();
    });
    events
}

/// The way in from the workspace: one tab per diagram, and closing a tab with
/// edits asks first.
mod in_the_workspace {
    use super::*;
    use fs::FakeFs;
    use gpui::VisualTestContext;
    use project::Project;
    use serde_json::json;
    use util::path;
    use workspace::MultiWorkspace;

    const DOCUMENT: &str = "# 図\n\n```mermaid\nflowchart LR\n    A --> B\n```\n\n```mermaid\nflowchart LR\n    X --> Y\n```\n";

    async fn workspace_with_document(
        cx: &mut TestAppContext,
    ) -> (Entity<Workspace>, Entity<Editor>, &mut VisualTestContext) {
        cx.update(|cx| cx.set_global(db::AppDatabase::test_new()));
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), json!({ "doc.md": DOCUMENT }))
            .await;
        let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace =
            multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
        let worktree_id = project.read_with(cx, |project, cx| {
            project
                .worktrees(cx)
                .next()
                .map(|worktree| worktree.read(cx).id())
                .expect("the project has a worktree")
        });
        let open = workspace.update_in(cx, |workspace, window, cx| {
            workspace.open_path(
                project::ProjectPath {
                    worktree_id,
                    path: util::rel_path::rel_path("doc.md").into(),
                },
                None,
                true,
                window,
                cx,
            )
        });
        let item = open.await.expect("the document opens");
        let editor = item
            .downcast::<Editor>()
            .expect("the document opens in an editor");
        editor.update(cx, |editor, cx| {
            if let Some(buffer) = editor.buffer().read(cx).as_singleton() {
                buffer.update(cx, |buffer, cx| {
                    buffer.set_language(Some(markdown_language()), cx)
                });
            }
        });
        cx.run_until_parked();
        (workspace, editor, cx)
    }

    /// Opens the tab on the fence holding `needle`, the way the preview's edit
    /// button does.
    fn open_on_fence(
        needle: &str,
        workspace: &Entity<Workspace>,
        editor: &Entity<Editor>,
        cx: &mut VisualTestContext,
    ) {
        let offset = DOCUMENT
            .find(needle)
            .unwrap_or_else(|| panic!("the document holds {needle:?}"));
        let target = editor.update(cx, |editor, cx| editor.mermaid_fence_target_at(offset, cx));
        workspace.update_in(cx, |workspace, window, cx| {
            open(workspace, editor.clone(), target, window, cx);
        });
        cx.run_until_parked();
    }

    fn tabs(
        workspace: &Entity<Workspace>,
        cx: &mut VisualTestContext,
    ) -> Vec<Entity<MermaidEditor>> {
        workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_pane()
                .read(cx)
                .items_of_type::<MermaidEditor>()
                .collect()
        })
    }

    fn active_tab(
        workspace: &Entity<Workspace>,
        cx: &mut VisualTestContext,
    ) -> Option<Entity<MermaidEditor>> {
        workspace.read_with(cx, |workspace, cx| {
            workspace.active_item_as::<MermaidEditor>(cx)
        })
    }

    #[gpui::test]
    async fn the_same_diagram_brings_its_tab_forward(cx: &mut TestAppContext) {
        let (workspace, editor, cx) = workspace_with_document(cx).await;

        open_on_fence("A --> B", &workspace, &editor, cx);
        let Some(first) = active_tab(&workspace, cx) else {
            panic!("the tab should be the active item");
        };
        let title = first.read_with(cx, |tab, cx| tab.tab_content_text(0, cx));
        assert_eq!(title.as_ref(), "Flowchart (doc.md)");

        // Back to the document, then the same diagram again.
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.activate_item(&editor, true, true, window, cx);
        });
        open_on_fence("A --> B", &workspace, &editor, cx);
        assert_eq!(
            tabs(&workspace, cx).len(),
            1,
            "no second tab on one diagram"
        );
        assert_eq!(active_tab(&workspace, cx), Some(first.clone()));

        open_on_fence("X --> Y", &workspace, &editor, cx);
        assert_eq!(
            tabs(&workspace, cx).len(),
            2,
            "another diagram gets its own tab"
        );
        assert_ne!(active_tab(&workspace, cx), Some(first));
    }

    #[gpui::test]
    async fn closing_an_edited_tab_asks_first(cx: &mut TestAppContext) {
        let (workspace, editor, cx) = workspace_with_document(cx).await;
        open_on_fence("A --> B", &workspace, &editor, cx);
        let Some(tab) = active_tab(&workspace, cx) else {
            panic!("the tab should be the active item");
        };
        tab.update_in(cx, |tab, window, cx| {
            let Some(row) = flowchart(tab).nodes.first() else {
                panic!("the diagram should have a node");
            };
            let label = row.label.clone();
            label.update(cx, |label, cx| label.set_text("開始", window, cx));
        });
        cx.run_until_parked();
        assert!(tab.read_with(cx, |tab, cx| tab.is_dirty(cx)));

        let close = |cx: &mut VisualTestContext| {
            let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
            pane.update_in(cx, |pane, window, cx| {
                pane.close_active_item(
                    &workspace::CloseActiveItem {
                        save_intent: None,
                        close_pinned: false,
                    },
                    window,
                    cx,
                )
            })
        };

        let closing = close(cx);
        cx.run_until_parked();
        let Some((message, _)) = cx.pending_prompt() else {
            panic!("closing should ask what to do");
        };
        assert_eq!(
            message, "The Flowchart in \"doc.md\" has unsaved edits. Do you want to save them?",
            "the question should name the diagram and its document"
        );
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
        closing.await.log_err();
        assert_eq!(tabs(&workspace, cx).len(), 1, "Cancel keeps the tab");

        let closing = close(cx);
        cx.run_until_parked();
        assert!(cx.has_pending_prompt());
        cx.simulate_prompt_answer("Save");
        cx.run_until_parked();
        closing.await.expect("saving and closing should succeed");
        assert!(tabs(&workspace, cx).is_empty(), "Save closes the tab");
        let text = editor.read_with(cx, |editor, cx| editor.text(cx));
        assert!(
            text.contains("A[開始]"),
            "Save applies the edit to the document:\n{text}"
        );
    }

    #[gpui::test]
    async fn closing_without_saving_leaves_the_document_alone(cx: &mut TestAppContext) {
        let (workspace, editor, cx) = workspace_with_document(cx).await;
        open_on_fence("A --> B", &workspace, &editor, cx);
        let Some(tab) = active_tab(&workspace, cx) else {
            panic!("the tab should be the active item");
        };
        tab.update_in(cx, |tab, window, cx| {
            let Some(row) = flowchart(tab).nodes.first() else {
                panic!("the diagram should have a node");
            };
            let label = row.label.clone();
            label.update(cx, |label, cx| label.set_text("開始", window, cx));
        });
        cx.run_until_parked();

        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
        let closing = pane.update_in(cx, |pane, window, cx| {
            pane.close_active_item(
                &workspace::CloseActiveItem {
                    save_intent: None,
                    close_pinned: false,
                },
                window,
                cx,
            )
        });
        cx.run_until_parked();
        assert!(cx.has_pending_prompt());
        cx.simulate_prompt_answer("Don't Save");
        cx.run_until_parked();
        closing.await.expect("closing should succeed");
        assert!(tabs(&workspace, cx).is_empty());
        assert_eq!(editor.read_with(cx, |editor, cx| editor.text(cx)), DOCUMENT);
    }
}

/// A fence holding a diagram no model can express is refused before the modal
/// is built, which is the branch `open` takes.
#[gpui::test]
async fn a_diagram_without_a_model_does_not_open(cx: &mut TestAppContext) {
    let _state = init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
    cx.set_state(indoc! {"
        ```mermaid
        zenuml
          A->B: 優先度ˇ
        ```
    "});

    let target = cx
        .update_editor(|editor, _window, cx| editor.mermaid_fence_target(cx))
        .expect("the cursor is inside a mermaid fence");
    let error = parse_document(&target.source)
        .err()
        .expect("a ZenUML diagram has no editing model");
    assert!(
        format!("{error}").contains("structured editor"),
        "unexpected message: {error}"
    );
}

fn flowchart(modal: &MermaidEditor) -> &FlowchartForm {
    modal
        .flowchart()
        .expect("the modal should hold a flowchart form")
}

/// The body of the one fence in the buffer, as the model would read it.
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

fn init_test(cx: &mut TestAppContext) -> Arc<workspace::AppState> {
    cx.update(|cx| {
        let state = workspace::AppState::test(cx);
        editor::init(cx);
        crate::init(cx);
        state
    })
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
) -> (
    EditorTestContext,
    Entity<MermaidEditor>,
    Arc<workspace::AppState>,
) {
    let app_state = init_test(cx);
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
    (cx, modal, app_state)
}

/// The flowchart work of session 7a: assigning classes, keeping `callback`
/// statements pointed at their node, renumbering `linkStyle` and carrying a
/// user's own edge id through the form.
mod flowchart_7a {
    use super::*;
    use mermaid_render::flowchart_model::{PreservedLine, PreservedPosition};

    #[gpui::test]
    async fn the_class_menu_offers_the_class_def_names_and_what_is_typed(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                flowchart TD
                  Aˇ --> B
                  classDef warn fill:#f96
                ```
            "},
            cx,
        )
        .await;

        let offered = cx.update(|_window, cx| modal.read(cx).class_candidates(cx));
        assert_eq!(offered, vec!["warn".to_string()]);

        // There is no UI for writing a `classDef` line, so a name that has none
        // yet is typed into the field beside the node list.
        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                let field = flowchart(modal).class_input.clone();
                field.update(cx, |field, cx| field.set_text("強調", window, cx));
            })
        });
        let offered = cx.update(|_window, cx| modal.read(cx).class_candidates(cx));
        assert_eq!(offered, vec!["warn".to_string(), "強調".to_string()]);

        // Mermaid reads a class name up to the first separator, so a name with
        // one in it would not say what it looks like and is never offered.
        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                let field = flowchart(modal).class_input.clone();
                field.update(cx, |field, cx| field.set_text("a,b", window, cx));
            })
        });
        let offered = cx.update(|_window, cx| modal.read(cx).class_candidates(cx));
        assert_eq!(offered, vec!["warn".to_string()]);
    }

    #[gpui::test]
    async fn ticking_a_class_writes_one_class_statement(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                flowchart TD
                  Aˇ --> B
                  subgraph g [まとまり]
                    A
                  end
                  classDef warn fill:#f96
                ```
            "},
            cx,
        )
        .await;

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                modal.toggle_class(ClassTarget::Node, 0, "warn", cx);
                // A class name with no `classDef` of its own is typed by hand,
                // and Mermaid never quotes one, so a Japanese name is as good
                // as an ASCII one.
                modal.toggle_class(ClassTarget::Node, 0, "強調", cx);
                modal.toggle_class(ClassTarget::Subgraph, 0, "warn", cx);
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        let body = fence_body(&mut cx);
        assert_eq!(
            body.matches("class A warn,強調").count(),
            1,
            "the node's classes should be written once:\n{body}"
        );
        assert_eq!(
            body.matches("class g warn").count(),
            1,
            "the group's class should be written once:\n{body}"
        );
        assert!(
            matches!(parse_document(&body), Ok(MermaidDocument::Flowchart(_))),
            "the fence should reopen:\n{body}"
        );
    }

    /// `merman` marks the target of a `click` statement with a synthetic
    /// `clickable` class that the statement itself puts back on every parse.
    /// Offering it, or writing it out as a `class` statement, would add one more
    /// on every visit to the editor.
    #[gpui::test]
    async fn a_clicked_node_does_not_collect_the_synthetic_class(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {r#"
                ```mermaid
                flowchart TD
                  Aˇ --> B
                  click A "https://example.com" _blank
                  classDef warn fill:#f96
                ```
            "#},
            cx,
        )
        .await;

        let offered = cx.update(|_window, cx| modal.read(cx).class_candidates(cx));
        assert_eq!(
            offered,
            vec!["warn".to_string()],
            "the synthetic class must not be on offer"
        );

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                modal.toggle_class(ClassTarget::Node, 0, "warn", cx);
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        let body = fence_body(&mut cx);
        assert_eq!(
            body.matches("class A warn").count(),
            1,
            "the class the user ticked should be written once:\n{body}"
        );
        assert!(
            !body.contains("clickable"),
            "the synthetic class must not be written back:\n{body}"
        );
    }

    /// `callback` names the function in `click A callback myFn`, which is the
    /// only place Mermaid spells the word: the `click` is what addresses the
    /// node, so the statement has to follow that node rather than the function.
    #[test]
    fn a_click_callback_statement_follows_a_rename_and_goes_with_a_deletion() {
        let renamed = retargeted(
            "click A callback myFn",
            &["Z"],
            &[("A".to_string(), "Z".to_string())],
        );
        assert_eq!(
            renamed,
            vec!["click Z callback myFn".to_string()],
            "the statement should follow the node it names"
        );

        let deleted = retargeted("click A callback myFn", &["B"], &[]);
        assert!(
            deleted.is_empty(),
            "the statement should go with the node it named: {deleted:?}"
        );
    }

    /// `retarget_preserved_lines` over one kept statement, with `live` as the
    /// ids the diagram still has and `A` as one it started with.
    fn retargeted(line: &str, live: &[&str], renames: &[(String, String)]) -> Vec<String> {
        let mut document = FlowchartDocument::empty();
        for id in live {
            document.nodes.push(FlowchartNode {
                id: (*id).to_string(),
                label: (*id).to_string(),
                markdown_label: false,
                shape: NodeShape::Rectangle,
                classes: Vec::new(),
            });
        }
        document.preserved_lines.push(PreservedLine {
            position: PreservedPosition::Bottom,
            text: line.to_string(),
        });
        let original = OriginalTargets {
            ids: vec!["A".to_string()],
            edge_ids: Vec::new(),
        };
        retarget_preserved_lines(&mut document, renames, &original);
        document
            .preserved_lines
            .iter()
            .map(|line| line.text.clone())
            .collect()
    }

    /// A `style` line for an id this diagram never had — one the user wrote
    /// for a node they have not added yet — is none of the form's business.
    /// Reading its second word as a node that has gone would delete a line the
    /// form does not understand, which is the one loss the user cannot see
    /// coming.
    #[test]
    fn a_kept_statement_that_never_named_an_element_is_left_alone() {
        let mut document = FlowchartDocument::empty();
        document.nodes.push(FlowchartNode {
            id: "B".into(),
            label: "B".into(),
            markdown_label: false,
            shape: NodeShape::Rectangle,
            classes: Vec::new(),
        });
        for text in [
            "style Q fill:#f96",
            "style A fill:#f96",
            "click B call go()",
        ] {
            document.preserved_lines.push(PreservedLine {
                position: PreservedPosition::Bottom,
                text: text.to_string(),
            });
        }
        let original = OriginalTargets {
            ids: vec!["A".to_string(), "B".to_string()],
            edge_ids: Vec::new(),
        };

        retarget_preserved_lines(&mut document, &[], &original);

        let kept = document
            .preserved_lines
            .iter()
            .map(|line| line.text.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            kept,
            vec![
                // `Q` was never an element of this diagram, so the statement is
                // none of the form's business.
                "style Q fill:#f96".to_string(),
                // `B` is still here.
                "click B call go()".to_string(),
            ],
            "`style A …` names a node that has gone and would declare it again"
        );
    }

    /// Mermaid separates words with any whitespace, a full width space
    /// included, so the span a rename is written into is counted in bytes off
    /// the line rather than assumed to be one byte per space.
    #[test]
    fn a_full_width_space_does_not_move_the_span_a_rename_is_written_into() {
        let line = "  style\u{3000}A fill:#f96";
        let span = targeted_span(line, &["style"]).expect("the statement addresses `A`");
        assert_eq!(line.get(span.clone()), Some("A"));
        assert_eq!(span, 10..11);

        let mut document = FlowchartDocument::empty();
        document.nodes.push(FlowchartNode {
            id: "Z".into(),
            label: "Z".into(),
            markdown_label: false,
            shape: NodeShape::Rectangle,
            classes: Vec::new(),
        });
        document.preserved_lines.push(PreservedLine {
            position: PreservedPosition::Bottom,
            text: line.to_string(),
        });
        let original = OriginalTargets {
            ids: vec!["A".to_string()],
            edge_ids: Vec::new(),
        };

        retarget_preserved_lines(
            &mut document,
            &[("A".to_string(), "Z".to_string())],
            &original,
        );

        assert_eq!(
            document
                .preserved_lines
                .first()
                .map(|line| line.text.as_str()),
            Some("  style\u{3000}Z fill:#f96"),
            "the full width space should be where it was"
        );
    }

    /// `linkStyle` addresses a position rather than an id, and it is renumbered
    /// before the kept statements are retargeted. Reading it as a `style`
    /// statement afterwards would rewrite its number as though it were an id, or
    /// drop the line once nothing was called `0`.
    #[test]
    fn a_link_style_is_not_read_as_a_style_statement() {
        assert!(
            targeted_span("  linkStyle 0 stroke:red", &["style", "click"]).is_none(),
            "`linkStyle` is its own keyword"
        );

        let mut document = FlowchartDocument::empty();
        document.nodes.push(FlowchartNode {
            id: "Z".into(),
            label: "Z".into(),
            markdown_label: false,
            shape: NodeShape::Rectangle,
            classes: Vec::new(),
        });
        document.preserved_lines.push(PreservedLine {
            position: PreservedPosition::Bottom,
            text: "linkStyle 0 stroke:red".to_string(),
        });
        let original = OriginalTargets {
            ids: vec!["A".to_string()],
            edge_ids: Vec::new(),
        };

        retarget_preserved_lines(
            &mut document,
            &[("A".to_string(), "Z".to_string())],
            &original,
        );

        assert_eq!(
            document
                .preserved_lines
                .first()
                .map(|line| line.text.as_str()),
            Some("linkStyle 0 stroke:red"),
            "the retargeting has no business with a position"
        );
    }

    /// A `linkStyle` separates its words with whatever whitespace the user
    /// typed, a full width space included, so the span the numbers are written
    /// back into is counted in bytes off the line rather than assumed to be one
    /// byte per space.
    #[test]
    fn a_full_width_space_does_not_move_the_numbers_a_link_style_addresses() {
        let line = "  linkStyle\u{3000}0,1 stroke:red";
        let span = number_list_span(line).expect("the statement addresses two links");
        assert_eq!(line.get(span), Some("0,1"));

        let mut document = FlowchartDocument::empty();
        document.preserved_lines.push(PreservedLine {
            position: PreservedPosition::Bottom,
            text: line.to_string(),
        });

        // The two edges swapped places.
        renumber_link_styles(&mut document, &[Some(1), Some(0)], 2);

        assert_eq!(
            document
                .preserved_lines
                .first()
                .map(|line| line.text.as_str()),
            Some("  linkStyle\u{3000}1,0 stroke:red"),
            "the full width space should be where it was"
        );
    }

    #[gpui::test]
    async fn removing_an_edge_renumbers_the_link_styles(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                flowchart TD
                  Aˇ --> B
                  B --> C
                  C --> D
                  linkStyle 1 stroke:red
                ```
            "},
            cx,
        )
        .await;

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                modal.remove_edge(0, cx);
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        let body = fence_body(&mut cx);
        assert!(
            body.contains("linkStyle 0 stroke:red"),
            "the statement should follow the edge it styled:\n{body}"
        );
    }

    #[gpui::test]
    async fn a_link_style_list_loses_only_the_edge_that_went(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                flowchart TD
                  Aˇ --> B
                  B --> C
                  C --> D
                  linkStyle 0,2 stroke:red
                ```
            "},
            cx,
        )
        .await;

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                modal.remove_edge(2, cx);
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        let body = fence_body(&mut cx);
        assert!(
            body.contains("linkStyle 0 stroke:red"),
            "only the deleted edge should leave the list:\n{body}"
        );
    }

    #[gpui::test]
    async fn a_link_style_whose_every_edge_went_is_dropped(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                flowchart TD
                  Aˇ --> B
                  B --> C
                  C --> D
                  linkStyle 1 stroke:red
                ```
            "},
            cx,
        )
        .await;

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                modal.remove_edge(1, cx);
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        let body = fence_body(&mut cx);
        assert!(
            !body.contains("linkStyle"),
            "the statement has nothing left to be about:\n{body}"
        );
    }

    /// `linkStyle default` is not a position, so nothing about it moves.
    #[gpui::test]
    async fn a_link_style_default_is_left_alone(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                flowchart TD
                  Aˇ --> B
                  B --> C
                  C --> D
                  linkStyle default stroke:red
                ```
            "},
            cx,
        )
        .await;

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                modal.remove_edge(0, cx);
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        let body = fence_body(&mut cx);
        assert!(
            body.contains("linkStyle default stroke:red"),
            "the default styling addresses no position:\n{body}"
        );
    }

    #[test]
    fn a_link_style_number_beyond_the_diagram_leaves_its_statement_untouched() {
        let mut document = FlowchartDocument::empty();
        for text in ["linkStyle 5 stroke:red", "linkStyle 0,1 stroke:blue"] {
            document.preserved_lines.push(PreservedLine {
                position: PreservedPosition::Bottom,
                text: text.to_string(),
            });
        }

        // The first two edges swapped places and a third was added since.
        renumber_link_styles(&mut document, &[Some(1), Some(0), None], 2);

        let kept = document
            .preserved_lines
            .iter()
            .map(|line| line.text.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            kept,
            vec![
                "linkStyle 5 stroke:red".to_string(),
                "linkStyle 1,0 stroke:blue".to_string(),
            ]
        );
    }

    #[gpui::test]
    async fn moving_an_edge_carries_its_link_style(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                flowchart TD
                  Aˇ --> B
                  B --> C
                  linkStyle 0 stroke:red
                ```
            "},
            cx,
        )
        .await;

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                modal.move_edge(0, true, cx);
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        let body = fence_body(&mut cx);
        assert!(
            body.contains("linkStyle 1 stroke:red"),
            "the styling should stay on the edge it was written for:\n{body}"
        );
    }

    /// Deleting a node takes every edge that touched it, and the positions the
    /// surviving edges are left with are the ones `linkStyle` has to be read
    /// against.
    #[gpui::test]
    async fn removing_a_node_renumbers_the_link_styles_of_the_edges_that_stay(
        cx: &mut TestAppContext,
    ) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                flowchart TD
                  Aˇ --> B
                  B --> C
                  C --> D
                  linkStyle 2 stroke:red
                ```
            "},
            cx,
        )
        .await;

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                // `B` is the second node, and both of the first two edges touch it.
                modal.remove_node(1, cx);
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        let body = fence_body(&mut cx);
        assert!(
            body.contains("linkStyle 0 stroke:red"),
            "the only edge left was the third one:\n{body}"
        );
    }

    /// The numbers after a deleted edge all move up by one, so a list has to
    /// close the gap rather than only drop the entry.
    #[gpui::test]
    async fn a_link_style_list_closes_the_gap_a_deleted_edge_leaves(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                flowchart TD
                  Aˇ --> B
                  B --> C
                  C --> D
                  linkStyle 0,1,2 stroke:red
                ```
            "},
            cx,
        )
        .await;

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                modal.remove_edge(1, cx);
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        let body = fence_body(&mut cx);
        assert!(
            body.contains("linkStyle 0,1 stroke:red"),
            "the third edge is the second one now:\n{body}"
        );
    }

    /// The mapping has to survive a run of edits rather than one: a deletion
    /// followed by an addition, and two moves in a row, both leave the edge a
    /// statement was written for somewhere the first edit did not put it.
    #[gpui::test]
    async fn the_link_style_mapping_survives_a_run_of_edits(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                flowchart TD
                  Aˇ --> B
                  B --> C
                  C --> D
                  linkStyle 0 stroke:red
                ```
            "},
            cx,
        )
        .await;

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                modal.move_edge(0, true, cx);
                modal.move_edge(1, true, cx);
                modal.remove_edge(0, cx);
                modal.add_edge(window, cx);
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        let body = fence_body(&mut cx);
        // The first edge went to the back, the one that took its place was
        // deleted, and a new one was added after it.
        assert!(
            body.contains("linkStyle 1 stroke:red"),
            "the styling should still be on `A --> B`:\n{body}"
        );
        assert!(
            matches!(parse_document(&body), Ok(MermaidDocument::Flowchart(_))),
            "the fence should reopen:\n{body}"
        );
    }

    #[gpui::test]
    async fn an_edge_id_and_its_metadata_come_back_out(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                flowchart TD
                  Aˇ e1@--> B
                  B --> C
                  e1@{ animate: true }
                ```
            "},
            cx,
        )
        .await;

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                let Some(field) = flowchart(modal).edge_labels.first().cloned() else {
                    panic!("the diagram should have an edge");
                };
                field.update(cx, |field, cx| field.set_text("経路", window, cx));
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        let body = fence_body(&mut cx);
        assert!(
            body.contains("A e1@-->|経路| B"),
            "the edge should keep the id the source gave it:\n{body}"
        );
        assert!(
            body.contains("e1@{ animate: true }"),
            "the edge's metadata should be kept verbatim:\n{body}"
        );
    }

    #[gpui::test]
    async fn deleting_an_edge_takes_its_metadata_statement_with_it(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                flowchart TD
                  Aˇ e1@--> B
                  B --> C
                  e1@{ animate: true }
                ```
            "},
            cx,
        )
        .await;

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                modal.remove_edge(0, cx);
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        let body = fence_body(&mut cx);
        assert!(
            !body.contains("e1"),
            "a statement left behind would declare `e1` as a node:\n{body}"
        );
        assert!(
            matches!(parse_document(&body), Ok(MermaidDocument::Flowchart(_))),
            "the fence should reopen:\n{body}"
        );
    }
}

/// The `box` grouping, `create` and `destroy` (7a).
mod sequence_7a {
    use super::*;

    fn form(modal: &MermaidEditor) -> &sequence::SequenceForm {
        modal
            .sequence()
            .expect("the modal should hold a sequence form")
    }

    fn column(modal: &MermaidEditor, id: &str) -> usize {
        form(modal)
            .document
            .participants
            .iter()
            .position(|participant| participant.id == id)
            .unwrap_or_else(|| panic!("`{id}` should be a column"))
    }

    /// A `box` is written as a run of declarations, so a column that joins one
    /// moves next to the members it now shares the group with — and the columns
    /// it passes keep their order among themselves.
    #[gpui::test]
    async fn joining_a_box_moves_the_column_next_to_it(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                sequenceDiagramˇ
                    box 業務
                        participant App
                        participant Db
                    end
                    participant User
                    participant Log
                    User->>App: 要求
                    App->>Db: 問い合わせ
                    App->>Log: 記録
                ```
            "},
            cx,
        )
        .await;

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                let index = column(modal, "Log");
                modal.set_participant_box(index, Some(0), cx);
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        assert_eq!(
            fence_body(&mut cx),
            indoc! {"
                sequenceDiagram
                    box 業務
                        participant App
                        participant Db
                        participant Log
                    end
                    participant User
                    User->>App: 要求
                    App->>Db: 問い合わせ
                    App->>Log: 記録"},
        );
    }

    /// Leaving a box means getting out from between its members, which is the
    /// same move in the other direction.
    #[gpui::test]
    async fn leaving_a_box_moves_the_column_past_the_run(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                sequenceDiagramˇ
                    box 業務
                        participant App
                        participant Db
                    end
                    participant User
                    App->>Db: 問い合わせ
                    User->>App: 要求
                ```
            "},
            cx,
        )
        .await;

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                let index = column(modal, "App");
                modal.set_participant_box(index, None, cx);
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        assert_eq!(
            fence_body(&mut cx),
            indoc! {"
                sequenceDiagram
                    box 業務
                        participant Db
                    end
                    participant App
                    participant User
                    App->>Db: 問い合わせ
                    User->>App: 要求"},
        );
    }

    /// A box added from the form carries a name and a fill of its own, and the
    /// fill is one of the colors Mermaid reads as a color.
    #[gpui::test]
    async fn a_new_box_takes_a_name_and_a_fill(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                sequenceDiagramˇ
                    participant App
                    participant Db
                    participant User
                    App->>Db: 問い合わせ
                ```
            "},
            cx,
        )
        .await;

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                modal.add_sequence_box(window, cx);
                for id in ["App", "Db"] {
                    let index = column(modal, id);
                    modal.set_participant_box(index, Some(0), cx);
                }
                modal.set_sequence_box_fill(0, Some("lightblue".to_string()), cx);
                let name = form(modal)
                    .boxes
                    .first()
                    .expect("the new box should have a row")
                    .name
                    .clone();
                // The padding is what a name typed into the field looks like
                // before it is written back, and the fence gets the name.
                name.update(cx, |name, cx| name.set_text("  業務  ", window, cx));
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        assert_eq!(
            fence_body(&mut cx),
            indoc! {"
                sequenceDiagram
                    box lightblue 業務
                        participant App
                        participant Db
                    end
                    participant User
                    App->>Db: 問い合わせ"},
        );
    }

    /// A column taken out of the middle of a box and put into an empty one has
    /// no member to move next to, so it has to move out of the run it is
    /// leaving instead — staying put would cut that box in two.
    #[gpui::test]
    async fn joining_an_empty_box_gets_the_column_out_of_the_old_run(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                sequenceDiagramˇ
                    box 業務
                        participant App
                        participant Db
                        participant Log
                    end
                    App->>Db: 問い合わせ
                    App->>Log: 記録
                ```
            "},
            cx,
        )
        .await;

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                modal.add_sequence_box(window, cx);
                let index = column(modal, "Db");
                modal.set_participant_box(index, Some(1), cx);
                assert_eq!(
                    form(modal).document.structure_error(),
                    None,
                    "the box the column left has to stay one run"
                );
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        assert_eq!(
            fence_body(&mut cx),
            indoc! {"
                sequenceDiagram
                    box 業務
                        participant App
                        participant Log
                    end
                    box Group
                        participant Db
                    end
                    App->>Db: 問い合わせ
                    App->>Log: 記録"},
        );
    }

    /// Deleting a box keeps its columns and leaves the boxes after it pointing
    /// at themselves rather than at their old neighbour.
    #[gpui::test]
    async fn removing_a_box_keeps_its_columns_and_renumbers_the_rest(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                sequenceDiagramˇ
                    box 一
                        participant A
                    end
                    box 二
                        participant B
                    end
                    A->>B: x
                ```
            "},
            cx,
        )
        .await;

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                modal.remove_sequence_box(0, cx);
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        assert_eq!(
            fence_body(&mut cx),
            indoc! {"
                sequenceDiagram
                    participant A
                    box 二
                        participant B
                    end
                    A->>B: x"},
        );
    }

    /// `create` declares a column of its own, so adding one adds the column and
    /// the statement together — and renaming the column follows into it.
    #[gpui::test]
    async fn the_create_button_adds_a_column_and_its_statement(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                sequenceDiagramˇ
                    participant アリス
                    アリス->>アリス: 準備
                ```
            "},
            cx,
        )
        .await;

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                modal.add_sequence_item(sequence::AddKind::Create, window, cx);
                let id = form(modal)
                    .participants
                    .last()
                    .expect("the created column should have a row")
                    .id
                    .clone();
                id.update(cx, |id, cx| id.set_text("カール", window, cx));
                modal.add_sequence_item(sequence::AddKind::Message, window, cx);
                let sequence::ItemRow::Text(text) = form(modal)
                    .items
                    .last()
                    .expect("the message should have a row")
                else {
                    panic!("a message row holds one text field");
                };
                let text = text.clone();
                text.update(cx, |text, cx| text.set_text("挨拶", window, cx));
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        assert_eq!(
            fence_body(&mut cx),
            indoc! {"
                sequenceDiagram
                    participant アリス
                    アリス->>アリス: 準備
                    create participant カール
                    アリス->>カール: 挨拶"},
        );
    }

    /// A created column's description field is drawn on its `create` row, so
    /// Tab reaches it there, between the messages around that row, and not
    /// among the participant fields above.
    #[gpui::test]
    async fn the_description_of_a_created_column_is_reached_on_its_create_row(
        cx: &mut TestAppContext,
    ) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                sequenceDiagramˇ
                    participant アリス
                    アリス->>アリス: 準備
                    create participant カール as 客
                    アリス->>カール: 挨拶
                ```
            "},
            cx,
        )
        .await;

        cx.update(|_window, cx| {
            let modal = modal.read(cx);
            let texts = form(modal)
                .fields()
                .iter()
                .map(|field| field.read(cx).text(cx))
                .collect::<Vec<_>>();
            assert_eq!(
                texts,
                vec!["", "アリス", "", "カール", "準備", "客", "挨拶"]
            );
        });
    }

    /// The `create` row declares the column, so deleting the column from the
    /// participant list has to take the statement with it.
    #[gpui::test]
    async fn deleting_a_created_column_removes_its_create_row(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                sequenceDiagramˇ
                    participant アリス
                    アリス->>アリス: 準備
                    create participant カール
                    アリス->>カール: 挨拶
                ```
            "},
            cx,
        )
        .await;

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                let index = column(modal, "カール");
                modal.remove_participant(index, cx);
                assert!(
                    !form(modal)
                        .document
                        .items
                        .iter()
                        .any(|item| matches!(item, SequenceItem::Create(_))),
                    "the `create` row should have gone with the column"
                );
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        assert_eq!(
            fence_body(&mut cx),
            indoc! {"
                sequenceDiagram
                    participant アリス
                    アリス->>アリス: 準備"},
        );
    }

    /// Mermaid refuses a `create` whose next message is addressed elsewhere, so
    /// the modal has to say so instead of writing a fence that cannot be drawn.
    #[gpui::test]
    async fn a_create_whose_message_points_elsewhere_is_refused(cx: &mut TestAppContext) {
        let source = indoc! {"
            ```mermaid
            sequenceDiagramˇ
                participant アリス
                アリス->>アリス: 準備
            ```
        "};
        let (mut cx, modal, _app_state) = open_on(source, cx).await;
        let before = cx.update_editor(|editor, _window, cx| editor.text(cx));

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                modal.add_sequence_item(sequence::AddKind::Create, window, cx);
                // The created column needs the message right after it; this one
                // goes back to アリス instead.
                modal.add_sequence_item(sequence::AddKind::Message, window, cx);
                let last = form(modal).document.items.len() - 1;
                modal.update_sequence_item(last, cx, |item| {
                    if let SequenceItem::Message(message) = item {
                        message.to = "アリス".to_string();
                    }
                });
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        assert_eq!(
            cx.update_editor(|editor, _window, cx| editor.text(cx)),
            before,
            "a diagram Mermaid would refuse must not be written back"
        );
        let message = cx.update(|_window, cx| modal.read(cx).message.clone());
        assert_eq!(
            message.as_deref(),
            Some("The message after `create NewParticipant` must be sent to `NewParticipant`."),
        );
    }

    /// A column declared at the top of the diagram may not come after a
    /// `create`d one, so a new participant goes in front of the created rows
    /// rather than at the end of the list.
    #[gpui::test]
    async fn a_participant_added_beside_a_create_lands_in_front_of_it(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                sequenceDiagramˇ
                    participant アリス
                    アリス->>アリス: 準備
                    create participant カール
                    アリス->>カール: 挨拶
                ```
            "},
            cx,
        )
        .await;

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                modal.add_participant(window, cx);
                let index = column(modal, "NewParticipant");
                let id = form(modal)
                    .participants
                    .get(index)
                    .expect("the new column should have a row")
                    .id
                    .clone();
                id.update(cx, |id, cx| id.set_text("ボブ", window, cx));
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        assert_eq!(
            fence_body(&mut cx),
            indoc! {"
                sequenceDiagram
                    participant アリス
                    participant ボブ
                    アリス->>アリス: 準備
                    create participant カール
                    アリス->>カール: 挨拶"},
        );
    }
}

/// The sequence leftovers of session mm: moving boxes, their wrap setting,
/// the `create` and `destroy` targets, moving a column without splitting a
/// box, config objects and ids spelled with `#…;`.
mod sequence_seq2 {
    use super::*;

    fn form(modal: &MermaidEditor) -> &sequence::SequenceForm {
        modal
            .sequence()
            .expect("the modal should hold a sequence form")
    }

    fn column(modal: &MermaidEditor, id: &str) -> usize {
        form(modal)
            .document
            .participants
            .iter()
            .position(|participant| participant.id == id)
            .unwrap_or_else(|| panic!("`{id}` should be a column"))
    }

    fn box_names(modal: &MermaidEditor, cx: &App) -> Vec<String> {
        form(modal)
            .boxes
            .iter()
            .map(|row| row.name.read(cx).text(cx))
            .collect()
    }

    /// Moving a box takes its whole run of columns along, and the name typed
    /// into its row moves with it.
    #[gpui::test]
    async fn a_box_moves_with_its_columns(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                sequenceDiagramˇ
                    box 一
                        participant A
                        participant B
                    end
                    participant C
                    box 二
                        participant D
                    end
                    A->>D: x
                ```
            "},
            cx,
        )
        .await;

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                let name = form(modal)
                    .boxes
                    .get(1)
                    .expect("the second box should have a row")
                    .name
                    .clone();
                name.update(cx, |name, cx| name.set_text("二番", window, cx));
                modal.move_sequence_box(1, false, cx);
                assert_eq!(box_names(modal, cx), vec!["二番", "一"]);
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        assert_eq!(
            fence_body(&mut cx),
            indoc! {"
                sequenceDiagram
                    box 二番
                        participant D
                    end
                    participant C
                    box 一
                        participant A
                        participant B
                    end
                    A->>D: x"},
        );
    }

    /// An empty box between two others is written after them, so the list
    /// shows it there from the start rather than moving it on the first edit.
    #[gpui::test]
    async fn an_empty_box_is_listed_where_it_will_be_written(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                sequenceDiagramˇ
                    box 一
                        participant A
                    end
                    box 空
                    end
                    box 二
                        participant B
                    end
                    A->>B: x
                ```
            "},
            cx,
        )
        .await;

        cx.update(|_window, cx| {
            let modal = modal.read(cx);
            assert_eq!(box_names(modal, cx), vec!["一", "二", "空"]);
            let document_names = form(modal)
                .document
                .boxes
                .iter()
                .map(|group| group.name.clone().unwrap_or_default())
                .collect::<Vec<_>>();
            assert_eq!(document_names, vec!["一", "二", "空"]);
        });
    }

    #[gpui::test]
    async fn the_wrap_box_writes_the_prefix(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                sequenceDiagramˇ
                    box 業務
                        participant A
                    end
                    box lightblue
                        participant B
                    end
                    A->>B: x
                ```
            "},
            cx,
        )
        .await;

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                modal.set_sequence_box_wrap(0, true, cx);
                modal.set_sequence_box_wrap(1, true, cx);
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        assert_eq!(
            fence_body(&mut cx),
            indoc! {"
                sequenceDiagram
                    box wrap:業務
                        participant A
                    end
                    box lightblue wrap:
                        participant B
                    end
                    A->>B: x"},
        );
    }

    /// A column in no box steps over a whole box instead of into it, and a
    /// member at the edge of its box stays where it is.
    #[gpui::test]
    async fn a_column_never_lands_inside_a_box(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                sequenceDiagramˇ
                    participant C
                    box 業務
                        participant A
                        participant B
                    end
                    A->>B: x
                    C->>A: y
                ```
            "},
            cx,
        )
        .await;

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                let edge = column(modal, "B");
                modal.move_participant(edge, true, cx);
                assert_eq!(column(modal, "B"), edge, "B is the last column already");
                let first = column(modal, "A");
                modal.move_participant(first, false, cx);
                assert_eq!(
                    column(modal, "A"),
                    first,
                    "leaving the box is the group menu's job"
                );

                let index = column(modal, "C");
                modal.move_participant(index, true, cx);
                assert_eq!(form(modal).document.structure_error(), None);
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        assert_eq!(
            fence_body(&mut cx),
            indoc! {"
                sequenceDiagram
                    box 業務
                        participant A
                        participant B
                    end
                    participant C
                    A->>B: x
                    C->>A: y"},
        );
    }

    /// Mermaid requires the message after a `destroy` to involve the column
    /// it ends, so a new `destroy` names the last message's receiver rather
    /// than the first column.
    #[gpui::test]
    async fn a_new_destroy_names_the_last_receiver(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                sequenceDiagramˇ
                    participant A
                    participant B
                    participant C
                    A->>C: 依頼
                ```
            "},
            cx,
        )
        .await;

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                modal.add_sequence_item(sequence::AddKind::Destroy, window, cx);
                assert!(
                    matches!(
                        form(modal).document.items.last(),
                        Some(SequenceItem::Destroy(id)) if id == "C"
                    ),
                    "{:?}",
                    form(modal).document.items
                );
                modal.add_sequence_item(sequence::AddKind::Message, window, cx);
                let last = form(modal).document.items.len() - 1;
                modal.update_sequence_item(last, cx, |item| {
                    if let SequenceItem::Message(message) = item {
                        message.to = "C".to_string();
                    }
                });
                // A second `destroy` goes to somebody not yet destroyed.
                modal.add_sequence_item(sequence::AddKind::Destroy, window, cx);
                assert!(
                    matches!(
                        form(modal).document.items.last(),
                        Some(SequenceItem::Destroy(id)) if id == "A"
                    ),
                    "{:?}",
                    form(modal).document.items
                );
                let last = form(modal).document.items.len() - 1;
                modal.remove_sequence_item(last, cx);
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        assert_eq!(
            fence_body(&mut cx),
            indoc! {"
                sequenceDiagram
                    participant A
                    participant B
                    participant C
                    A->>C: 依頼
                    destroy C
                    A->>C:"},
        );
    }

    /// The `create` row offers the columns it can declare, and choosing one
    /// moves the columns so that the created ones stay last. Its description
    /// is edited on the same row.
    #[gpui::test]
    async fn a_create_row_can_be_pointed_at_another_column(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                sequenceDiagramˇ
                    box 業務
                        participant A
                    end
                    participant B
                    participant D
                    A->>B: x
                    create participant C
                    A->>C: y
                ```
            "},
            cx,
        )
        .await;

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                let document = form(modal).current_document(cx);
                assert_eq!(
                    sequence::create_target_choices(&document, 1),
                    vec!["B".to_string(), "D".to_string(), "C".to_string()],
                    "a column in a box cannot be created"
                );
                modal.set_create_target(1, "D".to_string(), cx);
                modal.update_sequence_item(2, cx, |item| {
                    if let SequenceItem::Message(message) = item {
                        message.to = "D".to_string();
                    }
                });
                let alias = form(modal)
                    .participants
                    .get(column(modal, "D"))
                    .expect("the created column should have a row")
                    .alias
                    .clone();
                alias.update(cx, |alias, cx| alias.set_text("新規", window, cx));
                assert_eq!(form(modal).current_document(cx).structure_error(), None);
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        assert_eq!(
            fence_body(&mut cx),
            indoc! {"
                sequenceDiagram
                    box 業務
                        participant A
                    end
                    participant B
                    participant C
                    A->>B: x
                    create participant D as 新規
                    A->>D: y"},
        );
    }

    /// A config object stays on its column when the column is renamed.
    #[gpui::test]
    async fn a_config_object_follows_a_rename(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                sequenceDiagramˇ
                    participant A@{ \"type\": \"boundary\" }
                    participant B
                    A->>B: x
                ```
            "},
            cx,
        )
        .await;

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                let id = form(modal)
                    .participants
                    .first()
                    .expect("A should have a row")
                    .id
                    .clone();
                id.update(cx, |id, cx| id.set_text("境界", window, cx));
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        assert_eq!(
            fence_body(&mut cx),
            indoc! {"
                sequenceDiagram
                    participant 境界@{ \"type\": \"boundary\" }
                    participant B
                    境界->>B: x"},
        );
    }

    /// An id spelled with `#…;` opens, is shown as spelled, and an unedited
    /// diagram is written back byte for byte.
    #[gpui::test]
    async fn an_id_with_an_entity_opens_and_is_left_alone(cx: &mut TestAppContext) {
        let source = indoc! {"
            ```mermaid
            sequenceDiagramˇ
                participant A#59;B
                participant C
                A#59;B->>C: x
            ```
        "};
        let (mut cx, modal, _app_state) = open_on(source, cx).await;
        let before = cx.update_editor(|editor, _window, cx| editor.text(cx));

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                let shown = form(modal)
                    .participants
                    .first()
                    .expect("the column should have a row")
                    .id
                    .read(cx)
                    .text(cx);
                assert_eq!(shown, "A#59;B");
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        assert_eq!(
            cx.update_editor(|editor, _window, cx| editor.text(cx)),
            before,
            "an unedited diagram should be written back unchanged"
        );
        let message = cx.update(|_window, cx| modal.read(cx).message.clone());
        assert_eq!(message, None, "confirming should not have been refused");
    }
}

/// The flowchart leftovers of session mm: the edge id and `linkStyle` fields of
/// an edge row, the `classDef` rows, and edges kept in their subgraph block.
mod flowchart_flow2 {
    use super::*;

    fn set_text(field: &Entity<Editor>, text: &str, window: &mut Window, cx: &mut App) {
        field.update(cx, |field, cx| field.set_text(text, window, cx));
    }

    fn edge_fields(modal: &MermaidEditor, index: usize) -> (Entity<Editor>, Entity<Editor>) {
        let fields = flowchart(modal)
            .edge_fields
            .get(index)
            .unwrap_or_else(|| panic!("edge {index} should have its fields"));
        (fields.id.clone(), fields.style.clone())
    }

    fn class_def_fields(modal: &MermaidEditor, index: usize) -> (Entity<Editor>, Entity<Editor>) {
        let row = flowchart(modal)
            .class_defs
            .get(index)
            .unwrap_or_else(|| panic!("class definition {index} should have a row"));
        (row.name.clone(), row.style.clone())
    }

    fn message(cx: &mut EditorTestContext, modal: &Entity<MermaidEditor>) -> Option<String> {
        cx.update(|_window, cx| {
            modal
                .read(cx)
                .message
                .as_ref()
                .map(|message| message.to_string())
        })
    }

    #[gpui::test]
    async fn renaming_an_edge_id_follows_into_its_metadata(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                flowchart TD
                  Aˇ e1@--> B
                  B --> C
                  e1@{ animate: true }
                ```
            "},
            cx,
        )
        .await;

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                let (first, _) = edge_fields(modal, 0);
                let (second, _) = edge_fields(modal, 1);
                set_text(&first, "e9", window, cx);
                set_text(&second, "e2", window, cx);
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        assert_eq!(
            fence_body(&mut cx),
            indoc! {"
                flowchart TB
                    A
                    B
                    C
                    A e9@--> B
                    B e2@--> C
                  e9@{ animate: true }"},
        );
    }

    /// Clearing the id and deleting the edge both leave the metadata statement
    /// without its edge, and a statement left behind would declare a node of
    /// that name. Renaming another edge onto the freed id must not pick it up
    /// either.
    #[gpui::test]
    async fn metadata_goes_with_the_id_it_was_written_for(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                flowchart TD
                  Aˇ e1@--> B
                  B e2@--> C
                  C e3@--> D
                  e1@{ animate: true }
                  e2@{ animate: false }
                  e3@{ curve: basis }
                ```
            "},
            cx,
        )
        .await;

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                // `e2` goes, `e1` takes its name, and `e3` loses its id.
                modal.remove_edge(1, cx);
                let (first, _) = edge_fields(modal, 0);
                let (last, _) = edge_fields(modal, 1);
                set_text(&first, "e2", window, cx);
                set_text(&last, "", window, cx);
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        assert_eq!(
            fence_body(&mut cx),
            indoc! {"
                flowchart TB
                    A
                    B
                    C
                    D
                    A e2@--> B
                    C --> D
                  e2@{ animate: true }"},
        );
    }

    #[gpui::test]
    async fn an_edge_id_that_is_taken_is_refused(cx: &mut TestAppContext) {
        let source = indoc! {"
            ```mermaid
            flowchart TD
              Aˇ e1@--> B
              B --> C
            ```
        "};
        for (id, expected) in [
            (
                "B",
                "The link ID \"B\" is the same as the ID of a node or group.",
            ),
            ("e1", "The link ID \"e1\" is used more than once."),
            (
                "e-2",
                "The ID \"e-2\" of link 2 is not valid (ASCII letters, digits and _ only; no Mermaid keywords).",
            ),
        ] {
            let (mut cx, modal, _app_state) = open_on(source, cx).await;
            let before = cx.update_editor(|editor, _window, cx| editor.text(cx));
            cx.update(|window, cx| {
                modal.update(cx, |modal, cx| {
                    let (second, _) = edge_fields(modal, 1);
                    set_text(&second, id, window, cx);
                    modal.confirm(&menu::SecondaryConfirm, window, cx);
                })
            });
            assert_eq!(
                cx.update_editor(|editor, _window, cx| editor.text(cx)),
                before,
                "`{id}` must not be written back"
            );
            assert_eq!(message(&mut cx, &modal).as_deref(), Some(expected));
        }
    }

    #[gpui::test]
    async fn a_link_style_is_edited_added_and_removed_from_its_row(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                flowchart TD
                  Aˇ --> B
                  B --> C
                  C --> D
                  linkStyle 1 stroke:#f00;
                  linkStyle 2 stroke:#0f0
                ```
            "},
            cx,
        )
        .await;

        let shown = cx.update(|_window, cx| {
            modal.update(cx, |modal, cx| {
                let (_, style) = edge_fields(modal, 1);
                style.read(cx).text(cx)
            })
        });
        assert_eq!(
            shown, "stroke:#f00;",
            "the field shows the statement as written"
        );

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                let (_, first) = edge_fields(modal, 0);
                let (_, second) = edge_fields(modal, 1);
                let (_, third) = edge_fields(modal, 2);
                set_text(&first, "stroke:#00f,stroke-width:3px", window, cx);
                set_text(&second, "", window, cx);
                set_text(&third, "stroke:#0f0,stroke-dasharray:4", window, cx);
                // The styling travels with its edge.
                modal.move_edge(2, false, cx);
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        assert_eq!(
            fence_body(&mut cx),
            indoc! {"
                flowchart TB
                    A
                    B
                    C
                    D
                    A --> B
                    C --> D
                    B --> C
                    linkStyle 0 stroke:#00f,stroke-width:3px
                    linkStyle 1 stroke:#0f0,stroke-dasharray:4"},
        );
    }

    /// A statement naming several edges stays as it was written, and the edges
    /// it names offer no field of their own: a second statement would apply
    /// beside it.
    #[gpui::test]
    async fn an_edge_styled_by_a_shared_statement_has_no_style_field(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                flowchart TD
                  Aˇ --> B
                  B --> C
                  C --> D
                  linkStyle 0,2 stroke:#f00
                ```
            "},
            cx,
        )
        .await;

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                let form = flowchart(modal);
                assert!(form.link_style_is_fixed(0));
                assert!(!form.link_style_is_fixed(1));
                assert!(form.link_style_is_fixed(2));
                let fields = modal.fields();
                let (_, fixed) = edge_fields(modal, 0);
                let (_, free) = edge_fields(modal, 1);
                assert!(
                    !fields.contains(&fixed),
                    "Tab must not reach a field that is not drawn"
                );
                assert!(fields.contains(&free));

                // Typing into it anyway changes nothing.
                set_text(&fixed, "stroke:#00f", window, cx);
                set_text(&free, "stroke:#0f0", window, cx);
                modal.move_edge(0, true, cx);
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        let body = fence_body(&mut cx);
        assert!(
            body.ends_with("    linkStyle 0 stroke:#0f0\n  linkStyle 1,2 stroke:#f00"),
            "{body}"
        );
        assert!(!body.contains("#00f"), "{body}");
    }

    #[gpui::test]
    async fn a_link_style_that_would_start_another_statement_is_refused(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                flowchart TD
                  Aˇ --> B
                ```
            "},
            cx,
        )
        .await;
        let before = cx.update_editor(|editor, _window, cx| editor.text(cx));

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                let (_, style) = edge_fields(modal, 0);
                set_text(&style, "stroke:#f00; A --> Z", window, cx);
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        assert_eq!(
            cx.update_editor(|editor, _window, cx| editor.text(cx)),
            before
        );
        assert_eq!(
            message(&mut cx, &modal).as_deref(),
            Some(
                "The line style of link 1 has characters that cannot be used (; or %%, an unclosed quote, a trailing {, or a leading comma)."
            )
        );
    }

    #[gpui::test]
    async fn renaming_a_class_definition_renames_the_class(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                flowchart TD
                  Aˇ:::warn --> B
                  C
                  class B warn
                  classDef warn fill:#f96
                  classDef ok stroke:#3f3
                ```
            "},
            cx,
        )
        .await;

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                let (name, style) = class_def_fields(modal, 0);
                set_text(&name, "alert", window, cx);
                set_text(&style, "fill:#f66", window, cx);
                // The menus offer the new name, and ticking it on a node that
                // did not have the class assigns the definition being renamed.
                assert!(modal.class_candidates(cx).contains(&"alert".to_string()));
                assert!(!modal.class_candidates(cx).contains(&"warn".to_string()));
                modal.toggle_class(ClassTarget::Node, 2, "alert", cx);
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        assert_eq!(
            fence_body(&mut cx),
            indoc! {"
                flowchart TB
                    A
                    B
                    C
                    A --> B
                    class A alert
                    class B alert
                    class C alert
                    classDef alert fill:#f66
                    classDef ok stroke:#3f3"},
        );
    }

    /// Deleting a definition leaves the class on the nodes that carry it: a
    /// class without a definition is still valid, and a `themeCSS` directive
    /// may be what styles it.
    #[gpui::test]
    async fn class_definitions_are_added_moved_and_removed(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                flowchart TD
                  Aˇ --> B
                  class A warn
                  classDef warn fill:#f96
                  classDef ok stroke:#3f3
                ```
            "},
            cx,
        )
        .await;

        let removed = cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                let (name, style) = class_def_fields(modal, 0);
                modal.remove_class_def(0, cx);
                modal.add_class_def(window, cx);
                let (added_name, added_style) = class_def_fields(modal, 1);
                set_text(&added_name, "強調", window, cx);
                set_text(&added_style, "stroke-width:4px", window, cx);
                modal.move_class_def(1, false, cx);
                modal.confirm(&menu::SecondaryConfirm, window, cx);
                vec![name.downgrade(), style.downgrade()]
            })
        });

        assert_eq!(
            fence_body(&mut cx),
            indoc! {"
                flowchart TB
                    A
                    B
                    A --> B
                    class A warn
                    classDef 強調 stroke-width:4px
                    classDef ok stroke:#3f3"},
        );
        drop(modal);
        cx.run_until_parked();
        for field in removed {
            field.assert_released();
        }
    }

    /// A definition added in the form and assigned from the menu takes its
    /// assignments along when it is renamed afterwards. Its first name skips
    /// one a node already carries, which the rename must leave alone.
    #[gpui::test]
    async fn renaming_an_added_class_definition_renames_its_assignments(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                flowchart TD
                  Aˇ --> B
                  class B class
                ```
            "},
            cx,
        )
        .await;

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                modal.add_class_def(window, cx);
                let (name, style) = class_def_fields(modal, 0);
                assert_eq!(name.read(cx).text(cx), "class1");
                set_text(&style, "fill:#f66", window, cx);
                modal.toggle_class(ClassTarget::Node, 0, "class1", cx);
                set_text(&name, "hot", window, cx);
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        assert_eq!(
            fence_body(&mut cx),
            indoc! {"
                flowchart TB
                    A
                    B
                    A --> B
                    class A hot
                    class B class
                    classDef hot fill:#f66"},
        );
    }

    /// A second added definition does not start with the name the first was
    /// added under and renamed from: assignments made from it would then be
    /// renamed along with the first one's.
    #[gpui::test]
    async fn an_added_definition_does_not_reuse_the_first_name_of_a_renamed_one(
        cx: &mut TestAppContext,
    ) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                flowchart TD
                  Aˇ --> B
                ```
            "},
            cx,
        )
        .await;

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                modal.add_class_def(window, cx);
                let (name, style) = class_def_fields(modal, 0);
                set_text(&style, "fill:#f66", window, cx);
                set_text(&name, "x", window, cx);
                modal.add_class_def(window, cx);
                let (name, style) = class_def_fields(modal, 1);
                let second = name.read(cx).text(cx);
                assert_eq!(second, "class1");
                set_text(&style, "fill:#0f0", window, cx);
                modal.toggle_class(ClassTarget::Node, 0, &second, cx);
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        assert_eq!(
            fence_body(&mut cx),
            indoc! {"
                flowchart TB
                    A
                    B
                    A --> B
                    class A class1
                    classDef x fill:#f66
                    classDef class1 fill:#0f0"},
        );
    }

    /// A name a definition was opened under stands for that definition even
    /// after it is renamed, so choosing another row now called that does not
    /// take the renamed one off.
    #[gpui::test]
    async fn choosing_a_name_another_definition_was_renamed_from_keeps_that_one(
        cx: &mut TestAppContext,
    ) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                flowchart TD
                  Aˇ --> B
                  class A hot
                  classDef hot fill:#f00
                ```
            "},
            cx,
        )
        .await;

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                let (name, _style) = class_def_fields(modal, 0);
                set_text(&name, "warm", window, cx);
                modal.add_class_def(window, cx);
                let (name, style) = class_def_fields(modal, 1);
                set_text(&style, "fill:#0f0", window, cx);
                set_text(&name, "hot", window, cx);
                modal.toggle_class(ClassTarget::Node, 0, "hot", cx);
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        assert_eq!(
            fence_body(&mut cx),
            indoc! {"
                flowchart TB
                    A
                    B
                    A --> B
                    class A warm,hot
                    classDef warm fill:#f00
                    classDef hot fill:#0f0"},
        );
    }

    /// A node can carry, without any definition, the name an added definition
    /// is then renamed to. The menu shows it ticked under that name, and
    /// choosing it has to take it off rather than add the added row's own.
    #[gpui::test]
    async fn a_class_a_node_carries_under_a_renamed_definition_name_can_be_taken_off(
        cx: &mut TestAppContext,
    ) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                flowchart TD
                  Aˇ --> B
                  class A,B hot
                ```
            "},
            cx,
        )
        .await;

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                modal.add_class_def(window, cx);
                let (name, style) = class_def_fields(modal, 0);
                set_text(&style, "fill:#f66", window, cx);
                set_text(&name, "hot", window, cx);
                modal.toggle_class(ClassTarget::Node, 0, "hot", cx);
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        assert_eq!(
            fence_body(&mut cx),
            indoc! {"
                flowchart TB
                    A
                    B
                    A --> B
                    class B hot
                    classDef hot fill:#f66"},
        );
    }

    #[gpui::test]
    async fn a_class_definition_name_that_is_taken_or_unreadable_is_refused(
        cx: &mut TestAppContext,
    ) {
        let source = indoc! {"
            ```mermaid
            flowchart TD
              Aˇ --> B
              classDef warn fill:#f96
              classDef ok stroke:#3f3
            ```
        "};
        for (name, expected) in [
            (
                "warn",
                "The class definition \"warn\" is defined more than once.",
            ),
            ("", "A class definition has an empty name."),
            (
                "a,b",
                "The class definition name \"a,b\" is not valid (no whitespace or symbols such as , ; : # %).",
            ),
        ] {
            let (mut cx, modal, _app_state) = open_on(source, cx).await;
            let before = cx.update_editor(|editor, _window, cx| editor.text(cx));
            cx.update(|window, cx| {
                modal.update(cx, |modal, cx| {
                    let (field, _) = class_def_fields(modal, 1);
                    set_text(&field, name, window, cx);
                    modal.confirm(&menu::SecondaryConfirm, window, cx);
                })
            });
            assert_eq!(
                cx.update_editor(|editor, _window, cx| editor.text(cx)),
                before,
                "`{name}` must not be written back"
            );
            assert_eq!(message(&mut cx, &modal).as_deref(), Some(expected));
        }
    }

    /// `classDef default` styles every node rather than naming a class, so its
    /// name is not a field at all.
    #[gpui::test]
    async fn the_default_class_definition_keeps_its_name(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                flowchart TD
                  Aˇ --> B
                  classDef default fill:#eee
                ```
            "},
            cx,
        )
        .await;

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                let (name, style) = class_def_fields(modal, 0);
                let fields = modal.fields();
                assert!(!fields.contains(&name));
                assert!(fields.contains(&style));
                set_text(&name, "other", window, cx);
                set_text(&style, "fill:#ddd", window, cx);
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        assert!(
            fence_body(&mut cx).ends_with("    classDef default fill:#ddd"),
            "{}",
            fence_body(&mut cx)
        );
    }

    /// A `classDef` naming two classes cannot be a row, and moving only the
    /// others would reorder the styles, so none of them is a row and all stay
    /// as written.
    #[gpui::test]
    async fn class_definitions_the_form_cannot_hold_stay_as_written(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                flowchart TD
                  Aˇ --> B
                  classDef warn fill:#f96
                  classDef ok,done stroke:#3f3
                ```
            "},
            cx,
        )
        .await;

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                assert!(flowchart(modal).class_defs.is_empty());
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        assert!(
            fence_body(&mut cx)
                .ends_with("  classDef warn fill:#f96\n  classDef ok,done stroke:#3f3"),
            "{}",
            fence_body(&mut cx)
        );
    }

    #[gpui::test]
    async fn an_edge_stays_in_its_block_through_the_form(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                flowchart TD
                  subgraph g [まとまり]
                    Aˇ --> B
                  end
                  B --> C
                ```
            "},
            cx,
        )
        .await;

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                let Some(label) = flowchart(modal).edge_labels.first().cloned() else {
                    panic!("the diagram should have an edge");
                };
                set_text(&label, "中", window, cx);
                let Some(row) = flowchart(modal).subgraphs.first() else {
                    panic!("the diagram should have a group");
                };
                let id = row.id.clone();
                set_text(&id, "group", window, cx);
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        assert_eq!(
            fence_body(&mut cx),
            indoc! {"
                flowchart TB
                    A
                    B
                    C
                    subgraph group [まとまり]
                        B
                        A
                        A -->|中| B
                    end
                    B --> C"},
        );
    }

    /// A nested group is a member of its parent under its own id, so deleting
    /// the group has to take that membership too; left behind, the id came
    /// back as a node.
    #[gpui::test]
    async fn removing_a_nested_group_leaves_no_node_behind(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                flowchart TD
                  subgraph outer [外]
                    subgraph inner [内]
                      Aˇ --> B
                    end
                    C
                  end
                ```
            "},
            cx,
        )
        .await;

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                let inner = flowchart(modal)
                    .document
                    .subgraphs
                    .iter()
                    .position(|group| group.id == "inner")
                    .expect("the inner group should have a row");
                modal.remove_subgraph(inner, cx);
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        let body = fence_body(&mut cx);
        let Ok(MermaidDocument::Flowchart(document)) = parse_document(&body) else {
            panic!("the fence should reopen:\n{body}");
        };
        assert_eq!(
            document
                .nodes
                .iter()
                .map(|node| node.id.as_str())
                .collect::<Vec<_>>(),
            vec!["A", "B", "C"],
            "{body}"
        );
    }

    #[gpui::test]
    async fn a_deleted_edge_releases_its_id_and_style_fields(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                flowchart TD
                  Aˇ --> B
                  B --> C
                ```
            "},
            cx,
        )
        .await;

        let removed = cx.update(|_window, cx| {
            modal.update(cx, |modal, cx| {
                let (id, style) = edge_fields(modal, 0);
                modal.remove_edge(0, cx);
                assert_eq!(flowchart(modal).edge_fields.len(), 1);
                vec![id.downgrade(), style.downgrade()]
            })
        });
        cx.run_until_parked();
        for field in removed {
            field.assert_released();
        }
    }

    /// The menu shows a renamed definition under its new name while the node
    /// still holds the old one, so unticking the new name has to take the old
    /// one off.
    #[gpui::test]
    async fn unticking_a_renamed_class_removes_it(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                flowchart TD
                  Aˇ --> B
                  class A warn
                  classDef warn fill:#f96
                ```
            "},
            cx,
        )
        .await;

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                let (name, _) = class_def_fields(modal, 0);
                set_text(&name, "alert", window, cx);
                modal.toggle_class(ClassTarget::Node, 0, "alert", cx);
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        assert_eq!(
            fence_body(&mut cx),
            indoc! {"
                flowchart TB
                    A
                    B
                    A --> B
                    classDef alert fill:#f96"},
        );
    }

    /// Deleting one edge and moving another in the same sitting: a carried
    /// statement goes with its edge and disappears with it, and a statement kept
    /// verbatim is renumbered from where each edge came from.
    #[gpui::test]
    async fn link_styles_follow_a_deletion_and_a_move_together(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                flowchart TD
                  Aˇ --> B
                  B --> C
                  C --> D
                  D --> E
                  linkStyle 0,3 stroke:#f00
                  linkStyle 1 stroke:#00f
                  linkStyle 2 stroke:#0f0
                ```
            "},
            cx,
        )
        .await;

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                modal.remove_edge(1, cx);
                modal.move_edge(2, false, cx);
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        assert_eq!(
            fence_body(&mut cx),
            indoc! {"
                flowchart TB
                    A
                    B
                    C
                    D
                    E
                    A --> B
                    D --> E
                    C --> D
                    linkStyle 2 stroke:#0f0
                  linkStyle 0,1 stroke:#f00"},
        );
    }
}

/// The names a new row is given come from the catalog, so with the Japanese
/// catalog the editor inserts the same Japanese it did before the English
/// source text, and the fence it writes still reads as Mermaid.
mod japanese_defaults {
    use super::*;

    fn japanese() -> i18n::test_support::LocaleOverride {
        i18n::test_support::LocaleOverride::new(Some(i18n_catalog::test_support::bundled_catalog(
            "ja",
        )))
    }

    fn assert_opens(body: &str) {
        if let Err(error) = parse_document(body) {
            panic!("the written fence should open again: {error}\n{body}");
        }
    }

    #[gpui::test]
    async fn a_new_state_is_named_in_japanese(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                stateDiagram-v2ˇ
                    [*] --> 待機
                ```
            "},
            cx,
        )
        .await;
        let _japanese = japanese();
        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                modal.add_state(Vec::new(), window, cx);
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });
        let body = fence_body(&mut cx);
        assert_eq!(body, "stateDiagram-v2\n    [*] --> 待機\n    状態");
        assert_opens(&body);
    }

    #[gpui::test]
    async fn a_new_participant_is_named_in_japanese(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                sequenceDiagramˇ
                    participant アリス
                    アリス->>アリス: 準備
                ```
            "},
            cx,
        )
        .await;
        let _japanese = japanese();
        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                modal.add_participant(window, cx);
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });
        let body = fence_body(&mut cx);
        assert_eq!(
            body,
            "sequenceDiagram\n    participant アリス\n    participant 参加者\n    アリス->>アリス: 準備"
        );
        assert_opens(&body);
    }

    #[gpui::test]
    async fn a_new_requirement_and_element_are_named_in_japanese(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                requirementDiagramˇ
                    requirement login {
                    }
                ```
            "},
            cx,
        )
        .await;
        let _japanese = japanese();
        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                modal.add_requirement(window, cx);
                modal.add_requirement_element(window, cx);
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });
        let body = fence_body(&mut cx);
        assert_eq!(
            body,
            "requirementDiagram\n    requirement login {\n    }\n    requirement \"要求\" {\n    }\n    element \"要素\" {\n    }"
        );
        assert_opens(&body);
    }

    #[gpui::test]
    async fn a_new_entity_is_named_in_japanese(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                erDiagramˇ
                    顧客 {
                        string 名前
                    }
                ```
            "},
            cx,
        )
        .await;
        let _japanese = japanese();
        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                modal.add_entity(window, cx);
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });
        let body = fence_body(&mut cx);
        assert!(
            body.lines().any(|line| line.trim() == "エンティティ"),
            "the new entity should be named エンティティ:\n{body}"
        );
        assert_opens(&body);
    }
}
