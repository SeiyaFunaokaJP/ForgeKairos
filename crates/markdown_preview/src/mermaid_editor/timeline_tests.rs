use super::*;
use editor::test::editor_test_context::EditorTestContext;
use gpui::TestAppContext;
use indoc::indoc;
use mermaid_render::mermaid_model::parse_document;
use mermaid_render::timeline_model::{emit_timeline, parse_timeline};
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

fn period_row(modal: &MermaidEditor, section: usize, index: usize) -> &PeriodRow {
    modal
        .timeline()
        .and_then(|form| form.sections.get(section))
        .and_then(|section| section.periods.get(index))
        .unwrap_or_else(|| panic!("period {section}/{index} should have a row"))
}

const TEMPLATE_FENCE: &str = indoc! {"
    ```mermaid
    timelineˇ
        title 沿革
        2024 : 企画
        2025 : 開発 : 公開
        2026 : 運用
    ```
"};

/// The template `markdown::InsertMermaidDiagram` writes is read from the action
/// rather than copied here, so that a template which drifts away from what the
/// model emits turns this red.
#[test]
fn the_insert_template_is_the_source_the_model_writes_back() {
    let template = editor::actions::MermaidDiagramKind::Timeline.template();
    let timeline = parse_timeline(template)
        .unwrap_or_else(|error| panic!("the insert template should open: {error}"));
    assert_eq!(emit_timeline(&timeline), template);
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
        "an unedited timeline must be written back exactly as it was"
    );
}

/// Naming the band the template's periods sit in turns it into a section, a
/// new section gets a name of its own, and a period moved past the end of its
/// section lands at the top of the next one with its events.
#[gpui::test]
async fn sections_periods_and_events_are_written_back(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            let Some(name) = modal
                .timeline()
                .and_then(|form| form.sections.first())
                .map(|section| section.name.clone())
            else {
                panic!("the template's periods should sit in a band");
            };
            name.update(cx, |name, cx| name.set_text("前半", window, cx));
            modal.add_timeline_section(window, cx);
            modal.move_timeline_period(0, 2, true, cx);
            modal.add_timeline_event(1, 0, window, cx);
            let event = period_row(modal, 1, 0).events.last().cloned();
            if let Some(event) = event {
                event.update(cx, |event, cx| event.set_text("10:30 に公開", window, cx));
            }
            modal.remove_timeline_event(0, 1, 0, cx);
            modal.add_timeline_period(1, window, cx);
            let label = period_row(modal, 1, 1).label.clone();
            label.update(cx, |label, cx| label.set_text("2027", window, cx));
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        indoc! {"
            ```mermaid
            timeline
                title 沿革
                section 前半
                2024 : 企画
                2025 : 公開
                section 新しいセクション
                2026 : 運用 : 10:30 に公開
                2027 : 出来事
            ```
        "},
    );
}

/// An event that Mermaid would split in two is reported and nothing is
/// written, and so is a period the drawing would leave out.
#[gpui::test]
async fn what_would_be_drawn_differently_is_refused(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;
    let before = cx.update_editor(|editor, _window, cx| editor.text(cx));

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            let event = period_row(modal, 0, 0).events.first().cloned();
            if let Some(event) = event {
                event.update(cx, |event, cx| event.set_text("企画: 前半", window, cx));
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
            .is_some_and(|message| message.contains("企画: 前半")),
        "the modal should name the event it refused, got {message:?}"
    );

    // With the event fixed, a named section after the nameless band would
    // leave the band's periods undrawn.
    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            let event = period_row(modal, 0, 0).events.first().cloned();
            if let Some(event) = event {
                event.update(cx, |event, cx| event.set_text("企画", window, cx));
            }
            modal.add_timeline_section(window, cx);
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
            .is_some_and(|message| message.contains("描かれません")),
        "the modal should say the first band would vanish, got {message:?}"
    );
}
