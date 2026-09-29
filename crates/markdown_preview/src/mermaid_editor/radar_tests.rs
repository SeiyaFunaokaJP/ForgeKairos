use super::*;
use editor::test::editor_test_context::EditorTestContext;
use gpui::TestAppContext;
use mermaid_render::mermaid_model::parse_document;
use mermaid_render::radar_model::{emit_radar, parse_radar};
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

const TEMPLATE_FENCE: &str = "```mermaid\nradar-betaˇ\n    %% axis で軸、curve で系列。値は軸の順に並べる\n    title スキルの比較\n    axis design[\"設計\"], build[\"実装\"], test[\"テスト\"], ops[\"運用\"]\n    curve a[\"Aさん\"]{4, 3, 5, 2}\n    curve b[\"Bさん\"]{2, 5, 3, 4}\n    max 5\n```\n";

fn axis_id(modal: &MermaidEditor, row: usize) -> Entity<Editor> {
    modal
        .radar()
        .and_then(|form| form.rows.get(row))
        .and_then(|row| row.axis.as_ref())
        .map(|axis| axis.id.clone())
        .unwrap_or_else(|| panic!("row {row} should have an axis"))
}

fn fence_text(cx: &mut EditorTestContext) -> String {
    cx.update_editor(|editor, _window, cx| editor.text(cx))
}

/// The template `markdown::InsertMermaidDiagram` writes is read from the action
/// rather than copied here, so that a template which drifts away from what the
/// model emits turns this red.
#[test]
fn the_insert_template_is_the_source_the_model_writes_back() {
    let check = |template: &str| {
        let chart = parse_radar(template)
            .unwrap_or_else(|error| panic!("the insert template should open: {error}"));
        assert_eq!(emit_radar(&chart), template);
    };
    check(editor::actions::MermaidDiagramKind::Radar.template());

    // The fences these tests open are the Japanese template, which is what a
    // user with the Japanese catalog inserts.
    let _japanese = i18n::test_support::LocaleOverride::new(Some(
        i18n_catalog::test_support::bundled_catalog("ja"),
    ));
    let template = editor::actions::MermaidDiagramKind::Radar.template();
    check(template);
    assert!(
        TEMPLATE_FENCE.replace('\u{2c7}', "").contains(template),
        "the fence these tests open should hold the insert template"
    );
}

#[gpui::test]
async fn confirming_the_template_unchanged_leaves_the_fence_alone(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;
    let before = fence_text(&mut cx);
    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            modal.confirm(&menu::SecondaryConfirm, window, cx)
        })
    });
    assert_eq!(
        fence_text(&mut cx),
        before,
        "an unedited chart must be written back exactly as it was"
    );
}

/// Adding and removing an axis carries every curve's value for it, and a new
/// curve gets a value for every axis.
#[gpui::test]
async fn axes_and_curves_move_their_values_together(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            axis_id(modal, 1).update(cx, |field, cx| field.set_text("impl", window, cx));
            modal.add_radar_axis(window, cx);
            modal.remove_radar_axis(0, cx);
            modal.move_radar_curve(1, false, cx);
            modal.add_radar_curve(window, cx);
            modal.set_radar_graticule(Some(RadarGraticule::Polygon), cx);
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    assert_eq!(
        fence_text(&mut cx),
        "```mermaid\nradar-beta\n    %% axis で軸、curve で系列。値は軸の順に並べる\n    title スキルの比較\n    axis impl[\"実装\"], test[\"テスト\"], ops[\"運用\"], axis1[\"Axis 5\"]\n    curve b[\"Bさん\"]{5, 3, 4, 0}\n    curve a[\"Aさん\"]{3, 5, 2, 0}\n    curve curve1{0, 0, 0, 0}\n    graticule polygon\n    max 5\n```\n"
    );
}

/// A curve written with axis names follows an axis that is renamed.
#[gpui::test]
async fn a_renamed_axis_renames_its_key(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(
        "```mermaid\nradar-betaˇ\n    axis A, B\n    curve c{B: 2, A: 1}\n```\n",
        cx,
    )
    .await;

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            axis_id(modal, 0).update(cx, |field, cx| field.set_text("speed", window, cx));
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    assert_eq!(
        fence_text(&mut cx),
        "```mermaid\nradar-beta\n    axis speed, B\n    curve c{speed: 1, B: 2}\n```\n"
    );
}

/// Two axes with one id, and a keyed curve missing a value, cannot be
/// written; nothing is written and the reason is shown.
#[gpui::test]
async fn what_mermaid_cannot_read_is_refused(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;
    let before = fence_text(&mut cx);

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            axis_id(modal, 1).update(cx, |field, cx| field.set_text("design", window, cx));
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });
    assert_eq!(fence_text(&mut cx), before);
    let message = cx.update(|_window, cx| modal.read(cx).message.clone());
    assert!(
        message
            .as_deref()
            .is_some_and(|message| message.contains("Two axes have the ID `design`")),
        "the modal should name the duplicate axis, got {message:?}"
    );

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            axis_id(modal, 1).update(cx, |field, cx| field.set_text("build", window, cx));
            modal.set_radar_curve_keyed(0, true, cx);
            let last = modal
                .radar()
                .and_then(|form| form.rows.get(3))
                .and_then(|row| row.cells.first().cloned());
            if let Some(last) = last {
                last.update(cx, |field, cx| field.set_text("", window, cx));
            }
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });
    assert_eq!(fence_text(&mut cx), before);
    let message = cx.update(|_window, cx| modal.read(cx).message.clone());
    assert!(
        message
            .as_deref()
            .is_some_and(|message| message.contains("every axis needs a value")),
        "the modal should say the keyed curve needs every value, got {message:?}"
    );
}

/// The curves hold their values by row rather than by axis id, so an id typed
/// over another axis's on the way to a new one moves no value when a button is
/// pressed in between.
#[gpui::test]
async fn a_passing_duplicate_axis_id_moves_no_value(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;
    let before = fence_text(&mut cx);

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            let id = axis_id(modal, 0);
            id.update(cx, |id, cx| id.set_text("build", window, cx));
            modal.move_radar_axis(0, true, cx);
            modal.move_radar_axis(1, false, cx);
            id.update(cx, |id, cx| id.set_text("plan", window, cx));
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    assert_eq!(fence_text(&mut cx), before.replace("design[", "plan["));
}
