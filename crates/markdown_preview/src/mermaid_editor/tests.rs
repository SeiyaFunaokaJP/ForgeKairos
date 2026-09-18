use super::*;
use editor::test::editor_test_context::EditorTestContext;
use gpui::TestAppContext;
use indoc::indoc;
use mermaid_render::flowchart_model::parse_flowchart;
use mermaid_render::pie_model::PieDocument;
use mermaid_render::sequence_model::SequenceItem;
use mermaid_render::state_model::{StateDocument, StateItem};
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
        Some("ID「A」が重複しています。"),
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
    assert_eq!(message.as_deref(), Some("ID が空の要素があります。"));
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
            modal.cancel(&menu::Cancel, window, cx);
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
    assert!(
        message.is_some_and(|message| message.contains("使えません")),
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
        Some("`end` が対応するブロックより多くあります。"),
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
    assert_eq!(message.as_deref(), Some("図の種類を選んでください。"));

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
            "参加者を 1 つ以上追加してください。",
        ),
        (
            MermaidDiagramKind::Er,
            "エンティティを 1 つ以上追加してください。",
        ),
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

/// The modal stays open for as long as the user needs, and the fence it read
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

    // A surgical edit rather than a fresh buffer, so that the fence is still
    // the one the modal was opened on and only its body has moved on.
    cx.update_buffer(|buffer, cx| {
        let text = buffer.text();
        let offset = text
            .find("A --> B")
            .expect("the fence holds the edge the test rewrites");
        buffer.edit([(offset..offset + "A --> B".len(), "A --> C")], None, cx);
    });
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
    let message = cx.update(|_window, cx| modal.read(cx).message.clone());
    assert_eq!(
        message.as_deref(),
        Some("図のテキストが編集中に変更されました。もう一度開き直してください。")
    );
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
        mindmap
          rootˇ
        ```
    "});

    let target = cx
        .update_editor(|editor, _window, cx| editor.mermaid_fence_target(cx))
        .expect("the cursor is inside a mermaid fence");
    let error = parse_document(&target.source)
        .err()
        .expect("a mind map has no editing model");
    assert!(
        format!("{error}").contains("構造化エディター"),
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
                    box グループ
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
            Some("`create 参加者` の次のメッセージは `参加者` 宛てでなければなりません。"),
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
                let index = column(modal, "参加者");
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
