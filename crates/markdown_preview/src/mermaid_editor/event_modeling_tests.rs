use super::*;
use editor::test::editor_test_context::EditorTestContext;
use gpui::TestAppContext;
use indoc::indoc;
use mermaid_render::event_modeling_model::{emit_event_modeling, parse_event_modeling};
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

fn frame_row(modal: &MermaidEditor, index: usize) -> &FrameRow {
    modal
        .event_modeling()
        .and_then(|form| form.frames.get(index))
        .unwrap_or_else(|| panic!("frame {index} should have a row"))
}

const TEMPLATE_FENCE: &str = indoc! {"
    ```mermaid
    eventmodelingˇ
        %% tf 番号 種類 名前。ui は画面、cmd は操作、evt は出来事、rmo は読み取りモデル。名前は半角英数字で書く (Mermaid は日本語の名前を読めない)
        tf 01 ui CartScreen
        tf 02 cmd AddItem
        tf 03 evt ItemAdded
        tf 04 rmo CartItems ->> 03
    ```
"};

/// The template `markdown::InsertMermaidDiagram` writes is read from the action
/// rather than copied here, so that a template which drifts away from what the
/// model emits turns this red.
#[test]
fn the_insert_template_is_the_source_the_model_writes_back() {
    let template = editor::actions::MermaidDiagramKind::EventModeling.template();
    let document = parse_event_modeling(template)
        .unwrap_or_else(|error| panic!("the insert template should open: {error}"));
    assert_eq!(emit_event_modeling(&document), template);
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
        "an unedited event model must be written back exactly as it was"
    );
}

/// Renaming a frame and a data block carries into the arrows and references
/// that name them, and the other fields, menus and rows land where they were
/// set.
#[gpui::test]
async fn renames_carry_into_arrows_and_references(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            let name = frame_row(modal, 2).name.clone();
            name.update(cx, |name, cx| name.set_text("30", window, cx));
            let namespace = frame_row(modal, 2).namespace.clone();
            namespace.update(cx, |namespace, cx| namespace.set_text("Cart", window, cx));
            let inline = frame_row(modal, 1).inline_data.clone();
            inline.update(cx, |inline, cx| inline.set_text("{ 商品: 1 }", window, cx));

            modal.add_event_modeling_data(window, cx);
            modal.update_event_modeling_frame(3, cx, |frame| {
                frame.data_reference = Some("Data".to_string());
            });
            let data_name = modal
                .event_modeling()
                .and_then(|form| form.data_blocks.first())
                .map(|row| row.name.clone());
            if let Some(data_name) = data_name {
                data_name.update(cx, |data_name, cx| data_name.set_text("Items", window, cx));
            }
            modal.update_event_modeling_frame(0, cx, |frame| frame.reset = true);
            modal.update_event_modeling_frame(3, cx, |frame| {
                frame.entity_type =
                    mermaid_render::event_modeling_model::EventModelingEntityType::Processor;
            });
            modal.add_event_modeling_relation(cx);
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        indoc! {"
            ```mermaid
            eventmodeling
                %% tf 番号 種類 名前。ui は画面、cmd は操作、evt は出来事、rmo は読み取りモデル。名前は半角英数字で書く (Mermaid は日本語の名前を読めない)
                rf 01 ui CartScreen
                tf 02 cmd AddItem { 商品: 1 }
                tf 30 evt Cart.ItemAdded
                tf 04 pcr CartItems ->> 30 ->> 02 [[Items]]
                data Items {
              item: value
            }
            ```
        "},
    );
}

/// Deleting a frame takes the arrows drawn from it and hands its comment on;
/// adding one numbers it after the largest number.
#[gpui::test]
async fn removing_a_frame_takes_its_arrows_with_it(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            modal.remove_event_modeling_frame(2, cx);
            modal.remove_event_modeling_frame(0, cx);
            modal.add_event_modeling_frame(window, cx);
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        indoc! {"
            ```mermaid
            eventmodeling
                %% tf 番号 種類 名前。ui は画面、cmd は操作、evt は出来事、rmo は読み取りモデル。名前は半角英数字で書く (Mermaid は日本語の名前を読めない)
                tf 02 cmd AddItem
                tf 04 rmo CartItems
                tf 05 evt NewElement
            ```
        "},
    );
}

/// An arrow that `merman` would drop — one from a frame drawn later — and a
/// name with a space in it are reported and nothing is written.
#[gpui::test]
async fn what_would_be_drawn_differently_is_refused(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;
    let before = cx.update_editor(|editor, _window, cx| editor.text(cx));

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            // `04` draws its arrow from `03`; moved in front of it, the arrow
            // would come from a frame not drawn yet.
            modal.move_event_modeling_frame(3, false, cx);
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
            .is_some_and(|message| message.contains("前のフレーム")),
        "the modal should say the arrow's source comes later, got {message:?}"
    );

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            modal.move_event_modeling_frame(2, true, cx);
            let label = frame_row(modal, 0).label.clone();
            label.update(cx, |label, cx| label.set_text("Cart Screen", window, cx));
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
            .is_some_and(|message| message.contains("Cart Screen")),
        "the modal should name the label it refused, got {message:?}"
    );
}

/// Deleting a data block takes the references to it along, so the frames that
/// showed it are written without a reference to a block that is gone.
#[gpui::test]
async fn removing_a_data_block_clears_the_references_to_it(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(
        indoc! {"
            ```mermaid
            eventmodelingˇ
                tf 01 cmd Add [[Detail]]
                tf 02 evt Added [[Detail]]
                tf 03 rmo List [[Keep]]
                %% Detail の前
                data Detail {
              商品: 1
            }
                data Keep {
              数: 2
            }
            ```
        "},
        cx,
    )
    .await;

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            modal.remove_event_modeling_data(0, cx);
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        indoc! {"
            ```mermaid
            eventmodeling
                tf 01 cmd Add
                tf 02 evt Added
                tf 03 rmo List [[Keep]]
                %% Detail の前
                data Keep {
              数: 2
            }
            ```
        "},
    );
}

/// A data block written over several lines has no field, and is written back
/// unchanged around the edits made elsewhere.
#[gpui::test]
async fn a_multi_line_data_block_is_kept_as_written(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(
        indoc! {"
            ```mermaid
            eventmodelingˇ
                tf 01 cmd Add
                tf 02 evt Added [[Detail]]
                data Detail {
              商品: 1,
              数: 2
            }
            ```
        "},
        cx,
    )
    .await;

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            let form = modal.event_modeling().expect("an event model form");
            assert!(
                form.data_blocks
                    .first()
                    .is_some_and(|row| row.value.is_none())
            );
            let name = form.data_blocks.first().map(|row| row.name.clone());
            if let Some(name) = name {
                name.update(cx, |name, cx| name.set_text("Items", window, cx));
            }
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        indoc! {"
            ```mermaid
            eventmodeling
                tf 01 cmd Add
                tf 02 evt Added [[Items]]
                data Items {
              商品: 1,
              数: 2
            }
            ```
        "},
    );
}

/// A frame number typed over another frame's on the way to a new one must
/// not hand that frame's arrows over when a button is pressed in between: the
/// arrows stay keyed on the numbers the rows started with.
#[gpui::test]
async fn a_passing_duplicate_name_moves_no_arrow(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;
    let before = cx.update_editor(|editor, _window, cx| editor.text(cx));

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            let name = frame_row(modal, 2).name.clone();
            name.update(cx, |name, cx| name.set_text("04", window, cx));
            modal.add_event_modeling_frame(window, cx);
            modal.remove_event_modeling_frame(4, cx);
            modal.move_event_modeling_frame(0, true, cx);
            modal.move_event_modeling_frame(1, false, cx);
            name.update(cx, |name, cx| name.set_text("05", window, cx));
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        before
            .replace("tf 03 evt", "tf 05 evt")
            .replace("->> 03", "->> 05")
    );
}
