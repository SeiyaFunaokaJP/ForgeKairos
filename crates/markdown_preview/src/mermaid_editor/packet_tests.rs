use super::*;
use editor::test::editor_test_context::EditorTestContext;
use gpui::TestAppContext;
use indoc::indoc;
use mermaid_render::mermaid_model::parse_document;
use mermaid_render::packet_model::{emit_packet, parse_packet};
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

fn field_row(modal: &MermaidEditor, index: usize) -> (Entity<Editor>, Entity<Editor>) {
    modal
        .packet()
        .and_then(|form| form.fields.get(index))
        .map(|row| (row.bits.clone(), row.label.clone()))
        .unwrap_or_else(|| panic!("field {index} should have a row"))
}

const TEMPLATE_FENCE: &str = indoc! {r#"
    ```mermaid
    packet-betaˇ
        %% 開始ビット-終了ビット: "項目名"
        title UDP ヘッダー
        0-15: "送信元ポート"
        16-31: "宛先ポート"
        32-47: "長さ"
        48-63: "チェックサム"
        64-95: "データ (可変長)"
    ```
"#};

/// The template `markdown::InsertMermaidDiagram` writes is read from the action
/// rather than copied here, so that a template which drifts away from what the
/// model emits turns this red.
#[test]
fn the_insert_template_is_the_source_the_model_writes_back() {
    let template = editor::actions::MermaidDiagramKind::Packet.template();
    let packet = parse_packet(template)
        .unwrap_or_else(|error| panic!("the insert template should open: {error}"));
    assert_eq!(emit_packet(&packet), template);
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
        "an unedited packet diagram must be written back exactly as it was"
    );
}

/// Removing, resizing, moving and adding fields renumber every field after
/// them, so the ranges written back stay contiguous.
#[gpui::test]
async fn the_ranges_follow_the_lengths(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            modal.remove_packet_field(0, cx);
            let (bits, label) = field_row(modal, 0);
            bits.update(cx, |field, cx| field.set_text("8", window, cx));
            label.update(cx, |field, cx| {
                field.set_text("宛先 \"ポート\"", window, cx)
            });
            modal.move_packet_field(3, false, cx);
            modal.add_packet_field(window, cx);
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        indoc! {r#"
            ```mermaid
            packet-beta
                %% 開始ビット-終了ビット: "項目名"
                title UDP ヘッダー
                0-7: "宛先 \"ポート\""
                8-23: "長さ"
                24-55: "データ (可変長)"
                56-71: "チェックサム"
                72-79: "新しい項目"
            ```
        "#},
    );
}

/// A length that is not a whole number of bits and a label Mermaid would cut
/// at `%%` are reported, and nothing is written.
#[gpui::test]
async fn what_mermaid_cannot_draw_is_refused(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;
    let before = cx.update_editor(|editor, _window, cx| editor.text(cx));

    for (bits_text, label_text, expected) in [
        ("0", "送信元ポート", "ビット数"),
        ("1.5", "送信元ポート", "ビット数"),
        ("16", "50%% 完了", "50%% 完了"),
    ] {
        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                let (bits, label) = field_row(modal, 0);
                bits.update(cx, |field, cx| field.set_text(bits_text, window, cx));
                label.update(cx, |field, cx| field.set_text(label_text, window, cx));
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });
        let message = cx.update(|_window, cx| modal.read(cx).message.clone());
        assert!(
            message
                .as_deref()
                .is_some_and(|message| message.contains(expected)),
            "{bits_text} / {label_text} should be refused naming {expected}, got {message:?}"
        );
        assert_eq!(
            cx.update_editor(|editor, _window, cx| editor.text(cx)),
            before
        );
    }
}
