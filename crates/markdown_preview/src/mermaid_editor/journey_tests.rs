use super::*;
use editor::test::editor_test_context::EditorTestContext;
use gpui::TestAppContext;
use indoc::indoc;
use mermaid_render::journey_model::{emit_journey, parse_journey};
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

fn task_row(modal: &MermaidEditor, section: usize, index: usize) -> &TaskRow {
    modal
        .journey()
        .and_then(|form| form.sections.get(section))
        .and_then(|section| section.tasks.get(index))
        .unwrap_or_else(|| panic!("task {section}/{index} should have a row"))
}

const TEMPLATE_FENCE: &str = indoc! {"
    ```mermaid
    journeyˇ
        %% 作業 : 満足度 (1〜5) : 関わる人
        title 購入までの流れ
        section 探す
          商品を検索する: 4: 利用者
          比較する: 3: 利用者
        section 買う
          カートに入れる: 5: 利用者
          支払う: 2: 利用者, 店舗
    ```
"};

/// The template `markdown::InsertMermaidDiagram` writes is read from the action
/// rather than copied here, so that a template which drifts away from what the
/// model emits turns this red.
#[test]
fn the_insert_template_is_the_source_the_model_writes_back() {
    let check = |template: &str| {
        let journey = parse_journey(template)
            .unwrap_or_else(|error| panic!("the insert template should open: {error}"));
        assert_eq!(emit_journey(&journey), template);
    };
    check(editor::actions::MermaidDiagramKind::Journey.template());

    // The fences these tests open are the Japanese template, which is what a
    // user with the Japanese catalog inserts.
    let _japanese = i18n::test_support::LocaleOverride::new(Some(
        i18n_catalog::test_support::bundled_catalog("ja"),
    ));
    let template = editor::actions::MermaidDiagramKind::Journey.template();
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
        "an unedited journey must be written back exactly as it was"
    );
}

/// A task's name, score and people are written back, a task moved past the end
/// of its section lands at the top of the next one, a new section and task get
/// their default text, and renaming a person reaches every task and the fields
/// that show them.
#[gpui::test]
async fn tasks_scores_people_and_renames_are_written_back(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            let name = task_row(modal, 0, 0).name.clone();
            name.update(cx, |name, cx| name.set_text("キーワードで探す", window, cx));
            modal.set_journey_score(0, 0, 5, cx);
            let people = task_row(modal, 0, 1).people.clone();
            people.update(cx, |people, cx| people.set_text("利用者, 家族", window, cx));
            modal.move_journey_task(0, 1, true, cx);
            modal.add_journey_section(window, cx);
            modal.add_journey_task(2, window, cx);

            modal.set_journey_rename_from("利用者".to_string(), cx);
            let rename_to = modal.journey().map(|form| form.rename_to.clone());
            if let Some(rename_to) = rename_to {
                rename_to.update(cx, |field, cx| field.set_text("顧客", window, cx));
            }
            modal.apply_journey_rename(window, cx);
        })
    });

    let people_field = cx.update(|_window, cx| {
        let modal = modal.read(cx);
        task_row(modal, 1, 0).people.read(cx).text(cx)
    });
    assert_eq!(
        people_field, "顧客, 家族",
        "the rename should rewrite the fields that list the person"
    );

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            modal.confirm(&menu::SecondaryConfirm, window, cx)
        })
    });

    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        indoc! {"
            ```mermaid
            journey
                %% 作業 : 満足度 (1〜5) : 関わる人
                title 購入までの流れ
                section 探す
                  キーワードで探す: 5: 顧客
                section 買う
                  比較する: 3: 顧客, 家族
                  カートに入れる: 5: 顧客
                  支払う: 2: 顧客, 店舗
                section New section
                  New task: 3
            ```
        "},
    );
}

/// A task name Mermaid would cut at its `:` and a person list with an empty
/// name are reported and nothing is written.
#[gpui::test]
async fn what_would_be_drawn_differently_is_refused(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;
    let before = cx.update_editor(|editor, _window, cx| editor.text(cx));

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            let name = task_row(modal, 0, 0).name.clone();
            name.update(cx, |name, cx| name.set_text("検索: 商品", window, cx));
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
            .is_some_and(|message| message.contains("検索: 商品")),
        "the modal should name the task it refused, got {message:?}"
    );

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            let name = task_row(modal, 0, 0).name.clone();
            name.update(cx, |name, cx| name.set_text("検索", window, cx));
            let people = task_row(modal, 0, 0).people.clone();
            people.update(cx, |people, cx| people.set_text("利用者, ", window, cx));
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
            .is_some_and(|message| message.contains("cannot list the people")),
        "the modal should say the people cannot be written, got {message:?}"
    );
}

/// A task written with a trailing `:` is drawn with a nameless person; opening
/// and confirming it, with its empty people field untouched, keeps the `:`.
#[gpui::test]
async fn a_trailing_colon_survives_an_untouched_empty_field(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(
        indoc! {"
            ```mermaid
            journeyˇ
                section 登録
                  申し込む: 5:
                  確認する: 4
            ```
        "},
        cx,
    )
    .await;

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            modal.set_journey_score(0, 1, 2, cx);
            modal.confirm(&menu::SecondaryConfirm, window, cx)
        })
    });

    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        indoc! {"
            ```mermaid
            journey
                section 登録
                  申し込む: 5:
                  確認する: 2
            ```
        "},
    );
}

/// The band before the first `section` line can only be drawn first, so the
/// buttons that would move it, or move a section in front of it, are off.
#[test]
fn the_nameless_band_stays_in_front() {
    let section = |name: Option<&str>| JourneySection {
        name: name.map(str::to_string),
        tasks: Vec::new(),
    };
    let sections = [section(None), section(Some("A")), section(Some("B"))];
    assert!(!section_may_move(&sections, 0, true));
    assert!(!section_may_move(&sections, 1, false));
    assert!(section_may_move(&sections, 1, true));
    assert!(section_may_move(&sections, 2, false));

    let named = [section(Some("A")), section(Some("B"))];
    assert!(section_may_move(&named, 0, true));
    assert!(section_may_move(&named, 1, false));
    assert!(!section_may_move(&named, 1, true));
}
