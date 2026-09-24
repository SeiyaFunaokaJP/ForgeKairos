use super::*;

use editor::ToPoint as _;
use gpui::{
    DismissEvent, EventEmitter, FocusHandle, Focusable, KeyBinding, Keymap, Keystroke, Modifiers,
    TestAppContext, VisualTestContext, point, px,
};
use ide_query::TourStep;
use project::{FakeFs, WorktreeId};
use serde_json::json;
use settings::{KeymapFile, KeymapFileLoadResult, SettingsStore};
use util::path;
use util::rel_path::rel_path;
use workspace::{AppState, ModalView, SaveIntent, SplitDirection};

struct TestModal(FocusHandle);

impl TestModal {
    fn new(_: &mut Window, cx: &mut Context<Self>) -> Self {
        Self(cx.focus_handle())
    }
}

impl EventEmitter<DismissEvent> for TestModal {}

impl Focusable for TestModal {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.0.clone()
    }
}

impl ModalView for TestModal {}

impl Render for TestModal {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().track_focus(&self.0)
    }
}

fn init_test(cx: &mut TestAppContext) {
    cx.update(|cx| {
        AppState::test(cx);
        editor::init(cx);
        crate::init(cx);
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |settings| {
                settings.terminal.get_or_insert_default().starts_open = Some(false);
            });
        });
    });
}

fn numbered_lines(count: usize) -> String {
    (1..=count)
        .map(|number| format!("line {number}"))
        .collect::<Vec<_>>()
        .join("\n")
}

async fn setup(
    cx: &mut TestAppContext,
) -> (
    Entity<Project>,
    Entity<Workspace>,
    WorktreeId,
    &mut VisualTestContext,
) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/root"),
        json!({
            "a.rs": numbered_lines(20),
            "b.rs": numbered_lines(10),
            "long.rs": numbered_lines(300),
            "dir": { "c.rs": "fn c() {}\n" },
        }),
    )
    .await;
    fs.insert_file(
        path!("/root/big.txt"),
        vec![b'x'; MAX_DISPLAY_FILE_BYTES as usize + 1],
    )
    .await;
    let project = Project::test(fs, [path!("/root").as_ref()], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace =
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
    let worktree_id = project.read_with(cx, |project, cx| {
        project
            .worktrees(cx)
            .next()
            .expect("the test project has a worktree")
            .read(cx)
            .id()
    });
    cx.run_until_parked();
    (project, workspace, worktree_id, cx)
}

fn project_path(worktree_id: WorktreeId, path: &str) -> ProjectPath {
    ProjectPath {
        worktree_id,
        path: rel_path(path).into(),
    }
}

fn source(id: &str) -> AssistSource {
    AssistSource {
        id: id.to_string().into(),
        label: format!("Agent {id}").into(),
    }
}

fn lines(start_line: u32, end_line: u32) -> LineRange {
    LineRange {
        start_line,
        end_line,
    }
}

fn highlight(worktree_id: WorktreeId, path: &str, start: u32, end: u32) -> CodeHighlight {
    CodeHighlight {
        path: project_path(worktree_id, path),
        lines: lines(start, end),
        note: None,
    }
}

fn annotation(
    worktree_id: WorktreeId,
    path: &str,
    start: u32,
    end: u32,
    markdown: &str,
) -> CodeAnnotation {
    CodeAnnotation {
        path: project_path(worktree_id, path),
        lines: lines(start, end),
        markdown: markdown.to_string(),
    }
}

fn step(worktree_id: WorktreeId, path: &str, start: u32, end: u32, title: &str) -> TourStep {
    TourStep {
        path: project_path(worktree_id, path),
        lines: lines(start, end),
        title: title.to_string(),
        markdown: format!("{title} の説明"),
    }
}

fn code_display(cx: &mut VisualTestContext) -> Arc<dyn CodeDisplay> {
    cx.update(|_, cx| ide_query::code_display(cx).expect("ide_assist::init registers it"))
}

async fn show_highlights(
    project: &Entity<Project>,
    source: AssistSource,
    highlights: Vec<CodeHighlight>,
    cx: &mut VisualTestContext,
) -> Result<DisplayOutcome> {
    let display = code_display(cx);
    let task = cx.update(|_, cx| display.show_highlights(project, source, highlights, cx));
    let outcome = task.await;
    cx.run_until_parked();
    outcome
}

async fn show_annotations(
    project: &Entity<Project>,
    source: AssistSource,
    annotations: Vec<CodeAnnotation>,
    cx: &mut VisualTestContext,
) -> Result<DisplayOutcome> {
    let display = code_display(cx);
    let task = cx.update(|_, cx| display.show_annotations(project, source, annotations, cx));
    let outcome = task.await;
    cx.run_until_parked();
    outcome
}

async fn start_tour(
    project: &Entity<Project>,
    source: AssistSource,
    title: &str,
    steps: Vec<TourStep>,
    cx: &mut VisualTestContext,
) -> Result<DisplayOutcome> {
    let display = code_display(cx);
    let tour = CodeTour {
        title: title.to_string(),
        steps,
    };
    let task = cx.update(|_, cx| display.start_tour(project, source, tour, cx));
    let outcome = task.await;
    cx.run_until_parked();
    outcome
}

fn clear(project: &Entity<Project>, source: &AssistSource, cx: &mut VisualTestContext) {
    let display = code_display(cx);
    cx.update(|_, cx| display.clear(project, source, cx));
    cx.run_until_parked();
}

fn assist(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Entity<WorkspaceAssist> {
    workspace.read_with(cx, |workspace, cx| {
        workspace_assist(workspace, cx).expect("every workspace gets an assist")
    })
}

fn editors_for(
    workspace: &Entity<Workspace>,
    path: &str,
    cx: &mut VisualTestContext,
) -> Vec<Entity<Editor>> {
    workspace.read_with(cx, |workspace, cx| {
        workspace
            .items_of_type::<Editor>(cx)
            .filter(|editor| {
                editor
                    .read(cx)
                    .buffer()
                    .read(cx)
                    .as_singleton()
                    .and_then(|buffer| {
                        buffer
                            .read(cx)
                            .file()
                            .map(|file| file.path().as_unix_str() == path)
                    })
                    .unwrap_or(false)
            })
            .collect()
    })
}

fn editor_for(
    workspace: &Entity<Workspace>,
    path: &str,
    cx: &mut VisualTestContext,
) -> Option<Entity<Editor>> {
    editors_for(workspace, path, cx).into_iter().next()
}

/// 0-based rows, as the editor paints them.
fn rows<T: 'static>(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> Vec<(u32, u32)> {
    editor.update(cx, |editor, cx| {
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        editor
            .highlighted_rows::<T>(cx)
            .map(|(range, _)| {
                (
                    range.start.to_point(&snapshot).row,
                    range.end.to_point(&snapshot).row,
                )
            })
            .collect()
    })
}

fn applied_blocks(
    assist: &Entity<WorkspaceAssist>,
    editor: &Entity<Editor>,
    cx: &mut VisualTestContext,
) -> HashSet<CustomBlockId> {
    assist.read_with(cx, |assist, _| {
        assist
            .applied
            .get(&editor.entity_id())
            .map(|applied| applied.block_ids.clone())
            .unwrap_or_default()
    })
}

fn live_blocks(
    editor: &Entity<Editor>,
    block_ids: &HashSet<CustomBlockId>,
    cx: &mut VisualTestContext,
) -> usize {
    editor.update(cx, |editor, cx| {
        block_ids
            .iter()
            .filter(|block_id| editor.row_for_block(**block_id, cx).is_some())
            .count()
    })
}

fn tour_position(assist: &Entity<WorkspaceAssist>, cx: &mut VisualTestContext) -> Option<String> {
    assist.read_with(cx, |assist, _| {
        assist
            .tour
            .as_ref()
            .map(|tour| tour_status_label(tour.current, tour.steps.len()))
    })
}

fn card_in(
    assist: &Entity<WorkspaceAssist>,
    editor: &Entity<Editor>,
    cx: &mut VisualTestContext,
) -> Option<(usize, usize, Option<SharedString>)> {
    assist.update(cx, |assist, cx| {
        assist.tour_card_for(editor.entity_id(), cx).map(|card| {
            let unavailable = match card.body {
                TourCardBody::Unavailable(reason) => Some(reason),
                TourCardBody::Markdown { .. } => None,
            };
            (card.step, card.total, unavailable)
        })
    })
}

fn focus(editor: &Entity<Editor>, cx: &mut VisualTestContext) {
    editor.update_in(cx, |editor, window, cx| {
        window.focus(&editor.focus_handle(cx), cx);
    });
    cx.run_until_parked();
}

fn draw(cx: &mut VisualTestContext) {
    cx.update(|window, cx| {
        window.refresh();
        window.draw(cx).clear(cx);
    });
}

#[gpui::test]
async fn test_init_registers_the_code_display(cx: &mut TestAppContext) {
    init_test(cx);
    cx.update(|cx| assert!(ide_query::code_display(cx).is_some()));
}

#[gpui::test]
async fn test_highlights_replace_their_source_and_keep_other_sources(cx: &mut TestAppContext) {
    let (project, workspace, worktree_id, cx) = setup(cx).await;

    let outcome = show_highlights(
        &project,
        source("a"),
        vec![highlight(worktree_id, "a.rs", 2, 3)],
        cx,
    )
    .await
    .unwrap();
    assert_eq!(
        outcome,
        DisplayOutcome {
            shown: 1,
            skipped: Vec::new()
        }
    );
    let editor = editor_for(&workspace, "a.rs", cx).expect("the first highlight's file opens");
    assert_eq!(rows::<AssistHighlightRows>(&editor, cx), vec![(1, 2)]);

    show_highlights(
        &project,
        source("a"),
        vec![highlight(worktree_id, "a.rs", 5, 6)],
        cx,
    )
    .await
    .unwrap();
    assert_eq!(rows::<AssistHighlightRows>(&editor, cx), vec![(4, 5)]);

    show_highlights(
        &project,
        source("b"),
        vec![highlight(worktree_id, "a.rs", 9, 9)],
        cx,
    )
    .await
    .unwrap();
    assert_eq!(
        rows::<AssistHighlightRows>(&editor, cx),
        vec![(4, 5), (8, 8)]
    );
}

#[gpui::test]
async fn test_highlight_notes_are_labelled_blocks(cx: &mut TestAppContext) {
    let (project, workspace, worktree_id, cx) = setup(cx).await;
    let mut noted = highlight(worktree_id, "a.rs", 2, 4);
    noted.note = Some("ここが\n入口".to_string());
    show_highlights(
        &project,
        source("a"),
        vec![noted, highlight(worktree_id, "a.rs", 8, 8)],
        cx,
    )
    .await
    .unwrap();
    let editor = editor_for(&workspace, "a.rs", cx).unwrap();
    let assist = assist(&workspace, cx);
    let blocks = applied_blocks(&assist, &editor, cx);
    assert_eq!(
        blocks.len(),
        1,
        "only the highlight with a note gets a block"
    );
    assert_eq!(live_blocks(&editor, &blocks, cx), 1);
    let note = assist.read_with(cx, |assist, _| {
        assist
            .displays
            .iter()
            .find_map(|display| match &display.content {
                DisplayContent::Highlight { note } => note.clone(),
                DisplayContent::Annotation { .. } => None,
            })
    });
    assert_eq!(note.as_deref(), Some("ここが 入口"));
}

#[gpui::test]
async fn test_annotations_and_clear_by_source(cx: &mut TestAppContext) {
    let (project, workspace, worktree_id, cx) = setup(cx).await;
    show_annotations(
        &project,
        source("a"),
        vec![annotation(worktree_id, "a.rs", 3, 4, "**説明**")],
        cx,
    )
    .await
    .unwrap();
    show_highlights(
        &project,
        source("b"),
        vec![highlight(worktree_id, "a.rs", 8, 8)],
        cx,
    )
    .await
    .unwrap();
    let editor = editor_for(&workspace, "a.rs", cx).unwrap();
    let assist = assist(&workspace, cx);
    assert_eq!(rows::<AssistAnnotationRows>(&editor, cx), vec![(2, 3)]);
    assert_eq!(rows::<AssistHighlightRows>(&editor, cx), vec![(7, 7)]);
    let annotation_blocks = applied_blocks(&assist, &editor, cx);
    assert_eq!(live_blocks(&editor, &annotation_blocks, cx), 1);

    // Replacing the annotations of the same source swaps the block.
    show_annotations(
        &project,
        source("a"),
        vec![annotation(worktree_id, "a.rs", 10, 10, "次")],
        cx,
    )
    .await
    .unwrap();
    assert_eq!(rows::<AssistAnnotationRows>(&editor, cx), vec![(9, 9)]);
    assert_eq!(live_blocks(&editor, &annotation_blocks, cx), 0);
    let annotation_blocks = applied_blocks(&assist, &editor, cx);
    assert_eq!(live_blocks(&editor, &annotation_blocks, cx), 1);

    clear(&project, &source("a"), cx);
    assert!(rows::<AssistAnnotationRows>(&editor, cx).is_empty());
    assert_eq!(live_blocks(&editor, &annotation_blocks, cx), 0);
    assert_eq!(rows::<AssistHighlightRows>(&editor, cx), vec![(7, 7)]);
    assert!(editor.read_with(cx, |editor, _| editor.addon::<AssistAddon>().is_some()));

    clear(&project, &source("b"), cx);
    assert!(rows::<AssistHighlightRows>(&editor, cx).is_empty());
    assert!(editor.read_with(cx, |editor, _| editor.addon::<AssistAddon>().is_none()));
    assert!(assist.read_with(cx, |assist, _| assist.applied.is_empty()));
}

#[gpui::test]
async fn test_clear_wins_over_an_earlier_pending_request(cx: &mut TestAppContext) {
    let (project, workspace, worktree_id, cx) = setup(cx).await;
    let display = code_display(cx);
    let pending = cx.update(|_, cx| {
        let pending = display.show_highlights(
            &project,
            source("a"),
            vec![highlight(worktree_id, "a.rs", 2, 3)],
            cx,
        );
        display.clear(&project, &source("a"), cx);
        pending
    });
    let outcome = pending.await.unwrap();
    cx.run_until_parked();
    assert_eq!(outcome.shown, 1);
    let assist = assist(&workspace, cx);
    assert!(
        assist.read_with(cx, |assist, _| assist.displays.is_empty()),
        "a request that started before `clear` must not bring its display back"
    );
}

#[gpui::test]
async fn test_requests_over_the_limits_are_refused(cx: &mut TestAppContext) {
    let (project, workspace, worktree_id, cx) = setup(cx).await;
    show_highlights(
        &project,
        source("a"),
        vec![highlight(worktree_id, "a.rs", 1, 1)],
        cx,
    )
    .await
    .unwrap();

    let too_many = (0..=MAX_HIGHLIGHTS)
        .map(|_| highlight(worktree_id, "a.rs", 2, 2))
        .collect();
    let error = show_highlights(&project, source("a"), too_many, cx)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("50 件まで"), "{error}");
    let assist = assist(&workspace, cx);
    assert_eq!(
        assist.read_with(cx, |assist, _| assist.displays.len()),
        1,
        "a refused request leaves the previous display alone"
    );
    let exactly_the_limit = (0..MAX_HIGHLIGHTS)
        .map(|_| highlight(worktree_id, "a.rs", 2, 2))
        .collect();
    assert!(
        show_highlights(&project, source("a"), exactly_the_limit, cx)
            .await
            .is_ok()
    );

    let too_many = (0..=MAX_ANNOTATIONS)
        .map(|_| annotation(worktree_id, "a.rs", 2, 2, "x"))
        .collect();
    let error = show_annotations(&project, source("a"), too_many, cx)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("20 件まで"), "{error}");

    let too_many = (0..=MAX_TOUR_STEPS)
        .map(|_| step(worktree_id, "a.rs", 2, 2, "x"))
        .collect();
    let error = start_tour(&project, source("a"), "多すぎる", too_many, cx)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("30 個まで"), "{error}");

    let error = start_tour(&project, source("a"), "空", Vec::new(), cx)
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("ステップがありません"),
        "{error}"
    );
    assert!(assist.read_with(cx, |assist, _| assist.tour.is_none()));
}

#[gpui::test]
async fn test_unshowable_targets_are_skipped_with_reasons(cx: &mut TestAppContext) {
    let (project, workspace, worktree_id, cx) = setup(cx).await;
    let outcome = show_highlights(
        &project,
        source("a"),
        vec![
            highlight(worktree_id, "missing.rs", 1, 1),
            highlight(worktree_id, "dir", 1, 1),
            highlight(worktree_id, "big.txt", 1, 1),
            highlight(worktree_id, "a.rs", 30, 31),
            highlight(worktree_id, "a.rs", 0, 1),
            highlight(worktree_id, "a.rs", 3, 2),
            highlight(worktree_id, "b.rs", 1, 2),
            highlight(worktree_id, "a.rs", 19, 25),
        ],
        cx,
    )
    .await
    .unwrap();
    assert_eq!(outcome.shown, 2);
    let reasons = outcome
        .skipped
        .iter()
        .map(|skipped| {
            (
                skipped.path.path.as_unix_str().to_string(),
                skipped.reason.clone(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        reasons,
        vec![
            (
                "missing.rs".to_string(),
                "ファイルが見つかりません".to_string()
            ),
            ("dir".to_string(), "フォルダーは表示できません".to_string()),
            (
                "big.txt".to_string(),
                "ファイルが大きすぎます (4 MiB 超)".to_string()
            ),
            (
                "a.rs".to_string(),
                "30 行目はファイルの外です (このファイルは 20 行)".to_string()
            ),
            (
                "a.rs".to_string(),
                "行の範囲が正しくありません (0-1 行目)".to_string()
            ),
            (
                "a.rs".to_string(),
                "行の範囲が正しくありません (3-2 行目)".to_string()
            ),
        ]
    );

    // Only the first shown item's file is opened; the rest shows up when the user opens it.
    let editor_b = editor_for(&workspace, "b.rs", cx).expect("b.rs is the first shown item");
    assert_eq!(rows::<AssistHighlightRows>(&editor_b, cx), vec![(0, 1)]);
    assert!(editor_for(&workspace, "a.rs", cx).is_none());
    workspace
        .update_in(cx, |workspace, window, cx| {
            workspace.open_path(project_path(worktree_id, "a.rs"), None, true, window, cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();
    let editor_a = editor_for(&workspace, "a.rs", cx).unwrap();
    assert_eq!(
        rows::<AssistHighlightRows>(&editor_a, cx),
        vec![(18, 19)],
        "the range past the end is clamped to the last line"
    );
}

#[gpui::test]
async fn test_opening_does_not_take_focus(cx: &mut TestAppContext) {
    let (project, workspace, worktree_id, cx) = setup(cx).await;
    workspace
        .update_in(cx, |workspace, window, cx| {
            workspace.open_path(project_path(worktree_id, "b.rs"), None, true, window, cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();
    let editor_b = editor_for(&workspace, "b.rs", cx).unwrap();
    draw(cx);
    assert!(cx.update(|window, cx| editor_b.focus_handle(cx).is_focused(window)));
    let active_item = |cx: &mut VisualTestContext| {
        workspace.read_with(cx, |workspace, cx| {
            workspace.active_item(cx).map(|item| item.item_id())
        })
    };

    // The user is typing in the pane: the file only gets a tab behind the one they work in.
    show_highlights(
        &project,
        source("a"),
        vec![highlight(worktree_id, "a.rs", 2, 3)],
        cx,
    )
    .await
    .unwrap();
    draw(cx);
    let editor_a = editor_for(&workspace, "a.rs", cx).expect("a.rs gets a tab");
    assert_eq!(active_item(cx), Some(editor_b.entity_id()));
    assert!(cx.update(|window, cx| editor_b.focus_handle(cx).is_focused(window)));
    assert_eq!(rows::<AssistHighlightRows>(&editor_a, cx), vec![(1, 2)]);

    // The user is elsewhere (e.g. the agent's terminal): the file comes to the front, still
    // without focus.
    workspace.update_in(cx, |workspace, window, cx| {
        let status_bar = workspace.status_bar().read(cx).focus_handle(cx);
        window.focus(&status_bar, cx);
    });
    draw(cx);
    assert!(!cx.update(|window, cx| editor_b.focus_handle(cx).is_focused(window)));
    show_highlights(
        &project,
        source("a"),
        vec![highlight(worktree_id, "a.rs", 5, 5)],
        cx,
    )
    .await
    .unwrap();
    draw(cx);
    assert_eq!(active_item(cx), Some(editor_a.entity_id()));
    assert!(!cx.update(|window, cx| editor_a.focus_handle(cx).is_focused(window)));
}

#[gpui::test]
async fn test_agent_does_not_scroll_the_editor_the_user_types_in(cx: &mut TestAppContext) {
    let (project, workspace, worktree_id, cx) = setup(cx).await;
    workspace
        .update_in(cx, |workspace, window, cx| {
            workspace.open_path(project_path(worktree_id, "long.rs"), None, true, window, cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();
    let editor = editor_for(&workspace, "long.rs", cx).unwrap();
    draw(cx);
    let scroll_top =
        |cx: &mut VisualTestContext| editor.update(cx, |editor, cx| editor.scroll_position(cx).y);
    assert_eq!(scroll_top(cx), 0.);

    show_highlights(
        &project,
        source("a"),
        vec![highlight(worktree_id, "long.rs", 250, 250)],
        cx,
    )
    .await
    .unwrap();
    draw(cx);
    draw(cx);
    assert_eq!(rows::<AssistHighlightRows>(&editor, cx), vec![(249, 249)]);
    assert_eq!(
        scroll_top(cx),
        0.,
        "the view under the user's cursor stays put"
    );

    workspace.update_in(cx, |workspace, window, cx| {
        let status_bar = workspace.status_bar().read(cx).focus_handle(cx);
        window.focus(&status_bar, cx);
    });
    draw(cx);
    show_highlights(
        &project,
        source("a"),
        vec![highlight(worktree_id, "long.rs", 250, 250)],
        cx,
    )
    .await
    .unwrap();
    draw(cx);
    draw(cx);
    assert!(
        scroll_top(cx) > 150.,
        "otherwise the highlight is brought into view: {}",
        scroll_top(cx)
    );
}

#[gpui::test]
async fn test_nothing_opens_while_a_modal_is_open(cx: &mut TestAppContext) {
    let (project, workspace, worktree_id, cx) = setup(cx).await;
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.toggle_modal(window, cx, TestModal::new);
    });
    let outcome = show_highlights(
        &project,
        source("a"),
        vec![highlight(worktree_id, "a.rs", 2, 3)],
        cx,
    )
    .await
    .unwrap();
    assert_eq!(outcome.shown, 1);
    assert!(editor_for(&workspace, "a.rs", cx).is_none());
    assert!(workspace.update_in(cx, |workspace, window, cx| {
        workspace.has_active_modal(window, cx)
    }));

    let tour = start_tour(
        &project,
        source("a"),
        "モーダル中",
        vec![step(worktree_id, "b.rs", 1, 1, "一歩目")],
        cx,
    )
    .await
    .unwrap();
    assert_eq!(tour.shown, 1);
    assert!(editor_for(&workspace, "b.rs", cx).is_none());
    let assist = assist(&workspace, cx);
    assert_eq!(tour_position(&assist, cx).as_deref(), Some("ツアー 1/1"));

    workspace.update_in(cx, |workspace, window, cx| {
        workspace.toggle_modal(window, cx, TestModal::new);
    });
    cx.run_until_parked();
    // The status bar item is how the user gets the step on screen afterwards.
    assist.update(cx, |assist, cx| assist.return_to_current_step(cx));
    cx.run_until_parked();
    let editor_b = editor_for(&workspace, "b.rs", cx).unwrap();
    assert_eq!(card_in(&assist, &editor_b, cx), Some((0, 1, None)));
}

#[test]
fn test_sanitize_markdown_leaves_no_images_hidden_links_or_html() {
    let source = "see [the docs](https://evil.example/steal) and ![logo](data:image/png;base64,AAAA)\n\
                  ![](https://tracker.example/pixel.png) <b>bold</b> <img src=x onerror=alert(1)>\n\n\
                  <div onclick=\"x\">block</div>\n\n\
                  [ref link][r]\n\n[r]: https://evil.example/ref\n\n\
                  `code` and **strong**\n\n```rust\nfn main() {}\n```\n";
    let sanitized = sanitize_markdown(source);
    let events = parse_markdown_events(&sanitized);
    for (range, event) in &events {
        match event {
            MarkdownEvent::Start(MarkdownTag::Image { .. })
            | MarkdownEvent::Html
            | MarkdownEvent::InlineHtml
            | MarkdownEvent::InlineHtmlStyleStart(_) => {
                panic!("{event:?} survived in {sanitized:?}")
            }
            MarkdownEvent::Start(MarkdownTag::Link { dest_url, .. }) => {
                panic!(
                    "link to {dest_url} ({:?}) survived",
                    sanitized.get(range.clone())
                )
            }
            _ => {}
        }
    }
    assert!(!sanitized.contains("data:image"), "{sanitized}");
    assert!(!sanitized.contains("tracker.example"), "{sanitized}");
    assert!(!sanitized.contains("steal"), "{sanitized}");
    let rendered_text = events
        .iter()
        .filter_map(|(range, event)| match event {
            MarkdownEvent::Text => sanitized.get(range.clone()).map(str::to_string),
            MarkdownEvent::SubstitutedText(text) => Some(text.clone()),
            _ => None,
        })
        .collect::<String>();
    for expected in [
        "the docs",
        "(画像: logo)",
        "(画像)",
        "<b>bold</b>",
        "<img src=x onerror=alert(1)>",
        "ref link",
        "strong",
    ] {
        assert!(
            rendered_text.contains(expected),
            "{expected:?} missing from {rendered_text:?}"
        );
    }
    assert!(sanitized.contains("```rust\nfn main() {}\n```"));
}

#[test]
fn test_display_markdown_is_cut_at_the_character_limit() {
    let long = "あ".repeat(MAX_DISPLAY_MARKDOWN_CHARS + 100);
    let markdown = DisplayMarkdown::new(&long);
    assert!(markdown.truncated);
    assert_eq!(markdown.source.chars().count(), MAX_DISPLAY_MARKDOWN_CHARS);

    let short = "あ".repeat(MAX_DISPLAY_MARKDOWN_CHARS);
    let markdown = DisplayMarkdown::new(&short);
    assert!(!markdown.truncated);
    assert_eq!(markdown.source, short);
}

#[test]
fn test_single_line_collapses_and_cuts() {
    assert_eq!(
        single_line("  一行目\n\t二行目  ", 100).as_ref(),
        "一行目 二行目"
    );
    assert_eq!(single_line("あいうえお", 3).as_ref(), "あいう…");
    assert_eq!(single_line("あいう", 3).as_ref(), "あいう");
    assert_eq!(tour_status_label(1, 10), "ツアー 2/10");
    assert_eq!(lines_label(lines(3, 3)), "3 行目");
    assert_eq!(lines_label(lines(3, 5)), "3-5 行目");
}

#[gpui::test]
async fn test_annotation_links_do_not_open(cx: &mut TestAppContext) {
    let (project, workspace, worktree_id, cx) = setup(cx).await;
    let url = "https://example.com/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    show_annotations(
        &project,
        source("a"),
        vec![annotation(worktree_id, "a.rs", 2, 2, url)],
        cx,
    )
    .await
    .unwrap();
    let editor = editor_for(&workspace, "a.rs", cx).unwrap();
    focus(&editor, cx);
    draw(cx);
    let bounds = cx
        .debug_bounds("ide-assist-annotation-markdown")
        .expect("the annotation is on screen");
    let position = bounds.origin + point(px(4.), bounds.size.height / 2.);
    cx.simulate_click(position, Modifiers::none());
    cx.run_until_parked();
    assert_eq!(cx.opened_url(), None);
}

#[gpui::test]
async fn test_tour_steps_forward_and_back(cx: &mut TestAppContext) {
    let (project, workspace, worktree_id, cx) = setup(cx).await;
    let outcome = start_tour(
        &project,
        source("a"),
        "流れ",
        vec![
            step(worktree_id, "a.rs", 2, 3, "入口"),
            step(worktree_id, "b.rs", 1, 2, "設定"),
            step(worktree_id, "a.rs", 8, 9, "出口"),
        ],
        cx,
    )
    .await
    .unwrap();
    assert_eq!(
        outcome,
        DisplayOutcome {
            shown: 3,
            skipped: Vec::new()
        }
    );
    let assist = assist(&workspace, cx);
    assert_eq!(tour_position(&assist, cx).as_deref(), Some("ツアー 1/3"));
    let editor_a = editor_for(&workspace, "a.rs", cx).unwrap();
    assert_eq!(card_in(&assist, &editor_a, cx), Some((0, 3, None)));
    assert_eq!(rows::<TourStepRows>(&editor_a, cx), vec![(1, 2)]);
    assert!(
        !cx.update(|window, cx| editor_a.focus_handle(cx).is_focused(window)),
        "starting a tour does not take focus"
    );

    assist.update(cx, |assist, cx| assist.step_by(1, cx));
    cx.run_until_parked();
    assert_eq!(tour_position(&assist, cx).as_deref(), Some("ツアー 2/3"));
    let editor_b = editor_for(&workspace, "b.rs", cx).unwrap();
    assert_eq!(card_in(&assist, &editor_b, cx), Some((1, 3, None)));
    assert_eq!(rows::<TourStepRows>(&editor_b, cx), vec![(0, 1)]);
    assert!(
        cx.update(|window, cx| editor_b.focus_handle(cx).is_focused(window)),
        "the user's own navigation focuses the step"
    );
    for editor in editors_for(&workspace, "a.rs", cx) {
        assert_eq!(card_in(&assist, &editor, cx), None);
        assert!(rows::<TourStepRows>(&editor, cx).is_empty());
    }

    assist.update(cx, |assist, cx| assist.step_by(1, cx));
    cx.run_until_parked();
    let editor_a = editor_for(&workspace, "a.rs", cx).unwrap();
    assert_eq!(card_in(&assist, &editor_a, cx), Some((2, 3, None)));
    assert_eq!(rows::<TourStepRows>(&editor_a, cx), vec![(7, 8)]);

    assist.update(cx, |assist, cx| assist.step_by(1, cx));
    cx.run_until_parked();
    assert_eq!(
        tour_position(&assist, cx).as_deref(),
        Some("ツアー 3/3"),
        "next on the last step stays there"
    );

    for _ in 0..3 {
        assist.update(cx, |assist, cx| assist.step_by(-1, cx));
        cx.run_until_parked();
    }
    assert_eq!(
        tour_position(&assist, cx).as_deref(),
        Some("ツアー 1/3"),
        "previous on the first step stays there"
    );
    let editor_a = editor_for(&workspace, "a.rs", cx).unwrap();
    assert_eq!(card_in(&assist, &editor_a, cx), Some((0, 3, None)));

    focus(&editor_a, cx);
    cx.dispatch_action(EndTour);
    cx.run_until_parked();
    assert_eq!(tour_position(&assist, cx), None);
    assert_eq!(card_in(&assist, &editor_a, cx), None);
    assert!(rows::<TourStepRows>(&editor_a, cx).is_empty());
    assert!(applied_blocks(&assist, &editor_a, cx).is_empty());
}

#[gpui::test]
async fn test_tour_keys_work_in_the_step_editor(cx: &mut TestAppContext) {
    let (project, workspace, worktree_id, cx) = setup(cx).await;
    cx.update(|_, cx| {
        cx.bind_keys([
            KeyBinding::new(
                "ctrl-alt-]",
                NextTourStep,
                Some("Editor && ide_assist_tour"),
            ),
            KeyBinding::new(
                "ctrl-alt-[",
                PreviousTourStep,
                Some("Editor && ide_assist_tour"),
            ),
        ])
    });
    start_tour(
        &project,
        source("a"),
        "キー",
        vec![
            step(worktree_id, "a.rs", 2, 3, "一"),
            step(worktree_id, "b.rs", 1, 2, "二"),
        ],
        cx,
    )
    .await
    .unwrap();
    let assist = assist(&workspace, cx);
    let editor_a = editor_for(&workspace, "a.rs", cx).unwrap();
    assert!(editor_a.update_in(cx, |editor, window, cx| {
        editor.key_context(window, cx).contains(TOUR_KEY_CONTEXT)
    }));
    focus(&editor_a, cx);
    cx.simulate_keystrokes("ctrl-alt-]");
    cx.run_until_parked();
    assert_eq!(tour_position(&assist, cx).as_deref(), Some("ツアー 2/2"));
    cx.simulate_keystrokes("ctrl-alt-[");
    cx.run_until_parked();
    assert_eq!(tour_position(&assist, cx).as_deref(), Some("ツアー 1/2"));

    assist.update(cx, |assist, cx| assist.end_tour(cx));
    cx.run_until_parked();
    let editor_a = editor_for(&workspace, "a.rs", cx).unwrap();
    assert!(!editor_a.update_in(cx, |editor, window, cx| {
        editor.key_context(window, cx).contains(TOUR_KEY_CONTEXT)
    }));
}

#[gpui::test]
fn test_shipped_keymaps_bind_tour_keys_only_during_a_tour(cx: &mut TestAppContext) {
    cx.update(|cx| {
        for (name, keymap) in [
            (
                "default-windows",
                include_str!("../../../assets/keymaps/default-windows.json"),
            ),
            (
                "default-linux",
                include_str!("../../../assets/keymaps/default-linux.json"),
            ),
            (
                "default-macos",
                include_str!("../../../assets/keymaps/default-macos.json"),
            ),
        ] {
            let bindings = match KeymapFile::load(keymap, cx) {
                KeymapFileLoadResult::Success { key_bindings }
                | KeymapFileLoadResult::SomeFailedToLoad { key_bindings, .. } => key_bindings,
                KeymapFileLoadResult::JsonParseFailure { error } => {
                    panic!("{name}: {error}")
                }
            };
            let keymap = Keymap::new(bindings);
            for (keystroke, action) in [
                ("ctrl-alt-]", "ide_assist::NextTourStep"),
                ("ctrl-alt-[", "ide_assist::PreviousTourStep"),
            ] {
                let keystroke = Keystroke::parse(keystroke).unwrap();
                let during_tour = [
                    KeyContext::parse("Workspace").unwrap(),
                    KeyContext::parse("Pane").unwrap(),
                    KeyContext::parse("Editor mode=full edit_prediction ide_assist_tour").unwrap(),
                ];
                let (bindings, _) =
                    keymap.bindings_for_input(std::slice::from_ref(&keystroke), &during_tour);
                assert_eq!(
                    bindings.first().map(|binding| binding.action().name()),
                    Some(action),
                    "{name}: {keystroke} during a tour"
                );
                let without_tour = [
                    KeyContext::parse("Workspace").unwrap(),
                    KeyContext::parse("Pane").unwrap(),
                    KeyContext::parse("Editor mode=full edit_prediction").unwrap(),
                ];
                let (bindings, _) =
                    keymap.bindings_for_input(std::slice::from_ref(&keystroke), &without_tour);
                assert!(
                    bindings
                        .iter()
                        .all(|binding| !binding.action().name().starts_with("ide_assist::")),
                    "{name}: {keystroke} outside a tour"
                );
            }
        }
    });
}

#[gpui::test]
async fn test_tour_step_that_cannot_open(cx: &mut TestAppContext) {
    let (project, workspace, worktree_id, cx) = setup(cx).await;
    let outcome = start_tour(
        &project,
        source("a"),
        "穴あき",
        vec![
            step(worktree_id, "missing.rs", 1, 1, "無い"),
            step(worktree_id, "a.rs", 2, 3, "一"),
            step(worktree_id, "gone.rs", 1, 1, "消えた"),
            step(worktree_id, "b.rs", 1, 1, "二"),
        ],
        cx,
    )
    .await
    .unwrap();
    assert_eq!(outcome.shown, 2);
    assert_eq!(outcome.skipped.len(), 2);
    let assist = assist(&workspace, cx);
    assert_eq!(
        tour_position(&assist, cx).as_deref(),
        Some("ツアー 2/4"),
        "the tour starts at the first step that opens"
    );
    let editor_a = editor_for(&workspace, "a.rs", cx).unwrap();
    assert_eq!(card_in(&assist, &editor_a, cx), Some((1, 4, None)));

    assist.update(cx, |assist, cx| assist.step_by(1, cx));
    cx.run_until_parked();
    assert_eq!(tour_position(&assist, cx).as_deref(), Some("ツアー 3/4"));
    assert_eq!(
        card_in(&assist, &editor_a, cx),
        Some((2, 4, Some("ファイルが見つかりません".into()))),
        "the card stays put and says why"
    );
    assert!(rows::<TourStepRows>(&editor_a, cx).is_empty());

    assist.update(cx, |assist, cx| assist.step_by(1, cx));
    cx.run_until_parked();
    let editor_b = editor_for(&workspace, "b.rs", cx).unwrap();
    assert_eq!(card_in(&assist, &editor_b, cx), Some((3, 4, None)));

    let outcome = start_tour(
        &project,
        source("b"),
        "全部だめ",
        vec![step(worktree_id, "missing.rs", 1, 1, "無い")],
        cx,
    )
    .await
    .unwrap();
    assert_eq!(outcome.shown, 0);
    assert_eq!(
        tour_position(&assist, cx).as_deref(),
        Some("ツアー 4/4"),
        "a tour with nothing to show does not replace the running one"
    );
}

#[gpui::test]
async fn test_a_new_tour_replaces_the_running_one(cx: &mut TestAppContext) {
    let (project, workspace, worktree_id, cx) = setup(cx).await;
    start_tour(
        &project,
        source("a"),
        "一つ目",
        vec![step(worktree_id, "a.rs", 2, 3, "一")],
        cx,
    )
    .await
    .unwrap();
    start_tour(
        &project,
        source("b"),
        "二つ目",
        vec![
            step(worktree_id, "b.rs", 1, 1, "一"),
            step(worktree_id, "b.rs", 3, 3, "二"),
        ],
        cx,
    )
    .await
    .unwrap();
    let assist = assist(&workspace, cx);
    assert_eq!(
        assist.read_with(cx, |assist, _| assist
            .tour
            .as_ref()
            .map(|tour| (tour.source.id.clone(), tour.title.clone()))),
        Some(("b".into(), "二つ目".into()))
    );
    for editor in editors_for(&workspace, "a.rs", cx) {
        assert_eq!(card_in(&assist, &editor, cx), None);
    }

    clear(&project, &source("a"), cx);
    assert!(
        assist.read_with(cx, |assist, _| assist.tour.is_some()),
        "clearing another source keeps the tour"
    );
    clear(&project, &source("b"), cx);
    assert!(assist.read_with(cx, |assist, _| assist.tour.is_none()));
}

#[gpui::test]
async fn test_displays_follow_edits(cx: &mut TestAppContext) {
    let (project, workspace, worktree_id, cx) = setup(cx).await;
    show_highlights(
        &project,
        source("a"),
        vec![highlight(worktree_id, "a.rs", 5, 6)],
        cx,
    )
    .await
    .unwrap();
    show_annotations(
        &project,
        source("a"),
        vec![annotation(worktree_id, "a.rs", 10, 10, "注")],
        cx,
    )
    .await
    .unwrap();
    start_tour(
        &project,
        source("a"),
        "追随",
        vec![
            step(worktree_id, "a.rs", 12, 13, "一"),
            step(worktree_id, "b.rs", 1, 1, "二"),
        ],
        cx,
    )
    .await
    .unwrap();
    let editor = editor_for(&workspace, "a.rs", cx).unwrap();
    editor.update(cx, |editor, cx| {
        editor.edit([(Point::new(0, 0)..Point::new(0, 0), "new 1\nnew 2\n")], cx);
        editor.edit([(Point::new(20, 0)..Point::new(20, 0), "tail\n")], cx);
    });
    cx.run_until_parked();
    assert_eq!(rows::<AssistHighlightRows>(&editor, cx), vec![(6, 7)]);
    assert_eq!(rows::<AssistAnnotationRows>(&editor, cx), vec![(11, 11)]);
    assert_eq!(rows::<TourStepRows>(&editor, cx), vec![(13, 14)]);

    // Everything is laid out again from the stored anchors, not from the editor's own copy.
    let assist = assist(&workspace, cx);
    assist.update(cx, |assist, cx| assist.step_by(1, cx));
    cx.run_until_parked();
    assist.update(cx, |assist, cx| assist.step_by(-1, cx));
    cx.run_until_parked();
    let editor = editor_for(&workspace, "a.rs", cx).unwrap();
    assert_eq!(rows::<AssistHighlightRows>(&editor, cx), vec![(6, 7)]);
    assert_eq!(rows::<AssistAnnotationRows>(&editor, cx), vec![(11, 11)]);
    assert_eq!(rows::<TourStepRows>(&editor, cx), vec![(13, 14)]);
}

#[gpui::test]
async fn test_escape_clears_what_the_editor_shows(cx: &mut TestAppContext) {
    let (project, workspace, worktree_id, cx) = setup(cx).await;
    show_highlights(
        &project,
        source("a"),
        vec![
            highlight(worktree_id, "a.rs", 2, 2),
            highlight(worktree_id, "b.rs", 1, 1),
        ],
        cx,
    )
    .await
    .unwrap();
    show_annotations(
        &project,
        source("b"),
        vec![annotation(worktree_id, "a.rs", 4, 4, "注")],
        cx,
    )
    .await
    .unwrap();
    start_tour(
        &project,
        source("a"),
        "Esc",
        vec![
            step(worktree_id, "a.rs", 6, 6, "一"),
            step(worktree_id, "b.rs", 2, 2, "二"),
        ],
        cx,
    )
    .await
    .unwrap();
    let editor = editor_for(&workspace, "a.rs", cx).unwrap();
    let assist = assist(&workspace, cx);
    let blocks = applied_blocks(&assist, &editor, cx);
    assert_eq!(live_blocks(&editor, &blocks, cx), 2);

    // Vim's insert-mode Escape dismisses menus the same way but must leave these alone.
    let dismissed = editor.update_in(cx, |editor, window, cx| {
        editor.dismiss_menus_and_popups(false, window, cx)
    });
    cx.run_until_parked();
    assert!(!dismissed);
    assert_eq!(live_blocks(&editor, &blocks, cx), 2);
    assert!(assist.read_with(cx, |assist, _| assist.tour.is_some()));

    focus(&editor, cx);
    cx.dispatch_action(editor::actions::Cancel);
    cx.run_until_parked();
    assert!(rows::<AssistHighlightRows>(&editor, cx).is_empty());
    assert!(rows::<AssistAnnotationRows>(&editor, cx).is_empty());
    assert!(rows::<TourStepRows>(&editor, cx).is_empty());
    assert_eq!(live_blocks(&editor, &blocks, cx), 0);
    assert!(assist.read_with(cx, |assist, _| assist.tour.is_none()));
    assert!(editor.read_with(cx, |editor, _| editor.addon::<AssistAddon>().is_none()));
    assert_eq!(
        assist.read_with(cx, |assist, _| assist.displays.len()),
        1,
        "the other file's highlight stays"
    );
}

#[gpui::test]
async fn test_missing_workspace_is_reported_in_japanese(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/root"), json!({ "a.rs": "fn a() {}\n" }))
        .await;
    let project = Project::test(fs, [path!("/root").as_ref()], cx).await;
    let worktree_id = project.read_with(cx, |project, cx| {
        project.worktrees(cx).next().unwrap().read(cx).id()
    });
    let task = cx.update(|cx| {
        ide_query::code_display(cx).unwrap().show_highlights(
            &project,
            source("a"),
            vec![highlight(worktree_id, "a.rs", 1, 1)],
            cx,
        )
    });
    let error = task.await.unwrap_err();
    assert_eq!(
        error.to_string(),
        "このプロジェクトを開いている Zed のウィンドウが見つかりません"
    );
}

#[gpui::test]
async fn test_closed_editor_is_released(cx: &mut TestAppContext) {
    let (project, workspace, worktree_id, cx) = setup(cx).await;
    show_annotations(
        &project,
        source("a"),
        vec![annotation(worktree_id, "a.rs", 2, 3, "**閉じる**")],
        cx,
    )
    .await
    .unwrap();
    start_tour(
        &project,
        source("a"),
        "閉じる",
        vec![
            step(worktree_id, "a.rs", 5, 5, "一"),
            step(worktree_id, "b.rs", 1, 1, "二"),
        ],
        cx,
    )
    .await
    .unwrap();
    draw(cx);
    let editor = editor_for(&workspace, "a.rs", cx).unwrap();
    let editor_id = editor.entity_id();
    let released_editor = editor.downgrade();
    let assist = assist(&workspace, cx);
    let released_markdown = assist.read_with(cx, |assist, _| {
        assist
            .displays
            .iter()
            .find_map(|display| match &display.content {
                DisplayContent::Annotation { markdown, .. } => Some(markdown.downgrade()),
                DisplayContent::Highlight { .. } => None,
            })
            .expect("the annotation has its markdown")
    });
    drop(editor);
    let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
    pane.update_in(cx, |pane, window, cx| {
        pane.close_item_by_id(editor_id, SaveIntent::Skip, window, cx)
    })
    .await
    .unwrap();
    cx.run_until_parked();
    draw(cx);
    cx.run_until_parked();
    released_editor.assert_released();

    // The next sync forgets the closed editor.
    show_highlights(
        &project,
        source("b"),
        vec![highlight(worktree_id, "b.rs", 1, 1)],
        cx,
    )
    .await
    .unwrap();
    assert!(assist.read_with(cx, |assist, _| !assist.applied.contains_key(&editor_id)));

    clear(&project, &source("a"), cx);
    draw(cx);
    cx.run_until_parked();
    released_markdown.assert_released();
}

#[gpui::test]
async fn test_tour_survives_its_tab_moving_to_another_pane(cx: &mut TestAppContext) {
    let (project, workspace, worktree_id, cx) = setup(cx).await;
    cx.update(|_, cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |settings| {
                settings.preview_tabs.get_or_insert_default().enabled = Some(false);
            });
        });
    });
    workspace
        .update_in(cx, |workspace, window, cx| {
            workspace.open_path(
                project_path(worktree_id, "dir/c.rs"),
                None,
                false,
                window,
                cx,
            )
        })
        .await
        .unwrap();
    cx.run_until_parked();
    start_tour(
        &project,
        source("a"),
        "移動",
        vec![
            step(worktree_id, "a.rs", 2, 2, "一"),
            step(worktree_id, "b.rs", 1, 1, "二"),
        ],
        cx,
    )
    .await
    .unwrap();
    let assist = assist(&workspace, cx);
    let editor_a = editor_for(&workspace, "a.rs", cx).unwrap();
    let editor_c = editor_for(&workspace, "dir/c.rs", cx).unwrap();
    let (source_pane, destination) = workspace.update_in(cx, |workspace, window, cx| {
        let source_pane = workspace.active_pane().clone();
        let destination =
            workspace.split_pane(source_pane.clone(), SplitDirection::Right, window, cx);
        workspace::move_item(
            &source_pane,
            &destination,
            editor_a.entity_id(),
            0,
            true,
            window,
            cx,
        );
        (source_pane, destination)
    });
    cx.run_until_parked();
    assert_eq!(card_in(&assist, &editor_a, cx), Some((0, 2, None)));
    // The user goes back to the other pane, so new steps open there.
    focus(&editor_c, cx);
    assert_eq!(
        workspace.read_with(cx, |workspace, _| workspace.active_pane().clone()),
        source_pane
    );

    assist.update(cx, |assist, cx| assist.step_by(1, cx));
    cx.run_until_parked();
    let editor_b = editor_for(&workspace, "b.rs", cx).unwrap();
    assert_eq!(card_in(&assist, &editor_b, cx), Some((1, 2, None)));
    assert_eq!(card_in(&assist, &editor_a, cx), None);

    assist.update(cx, |assist, cx| assist.step_by(-1, cx));
    cx.run_until_parked();
    assert_eq!(
        editors_for(&workspace, "a.rs", cx),
        vec![editor_a.clone()],
        "going back reuses the moved tab instead of opening another"
    );
    assert!(
        destination
            .read_with(cx, |pane, _| pane.index_for_item(&editor_a))
            .is_some()
    );
    assert_eq!(card_in(&assist, &editor_a, cx), Some((0, 2, None)));
}

#[gpui::test]
async fn test_tour_card_buttons(cx: &mut TestAppContext) {
    let (project, workspace, worktree_id, cx) = setup(cx).await;
    start_tour(
        &project,
        source("a"),
        "ボタン",
        vec![
            step(worktree_id, "a.rs", 2, 2, "一"),
            step(worktree_id, "a.rs", 6, 6, "二"),
        ],
        cx,
    )
    .await
    .unwrap();
    let assist = assist(&workspace, cx);
    let editor = editor_for(&workspace, "a.rs", cx).unwrap();
    focus(&editor, cx);
    draw(cx);
    let next = cx
        .debug_bounds("ide-assist-tour-next")
        .expect("the tour card is on screen");
    cx.simulate_click(next.center(), Modifiers::none());
    cx.run_until_parked();
    assert_eq!(tour_position(&assist, cx).as_deref(), Some("ツアー 2/2"));
    assert_eq!(card_in(&assist, &editor, cx), Some((1, 2, None)));
}

/// What the renderer itself parsed, which is what decides what gets drawn, loaded or linked.
fn assert_renders_inert(markdown: &Entity<Markdown>, cx: &mut VisualTestContext) {
    markdown.read_with(cx, |markdown, _| {
        let parsed = markdown.parsed_markdown();
        let source = parsed.source();
        let events = &parsed.events;
        let mut depth = 0usize;
        for (index, (range, event)) in events.iter().enumerate() {
            match event {
                MarkdownEvent::Start(MarkdownTag::Image { .. })
                | MarkdownEvent::Html
                | MarkdownEvent::InlineHtml
                | MarkdownEvent::InlineHtmlStyleStart(_) => {
                    panic!("{event:?} reaches the renderer in {source:?}")
                }
                MarkdownEvent::Start(MarkdownTag::Link { dest_url, .. }) => {
                    let text = text_within(&events[index + 1..], range, source);
                    assert_eq!(
                        text.as_str(),
                        dest_url.as_ref(),
                        "a link hides its destination in {source:?}"
                    );
                }
                MarkdownEvent::Start(_) => {}
                _ => {}
            }
            match event {
                MarkdownEvent::Start(_) => depth += 1,
                MarkdownEvent::End(_) => depth -= 1,
                _ => {}
            }
            assert!(depth <= MAX_MARKDOWN_NESTING, "nested {depth} deep");
        }
    });
}

const HOSTILE_MARKDOWN: &[&str] = &[
    // Front matter: `parse_markdown_events` reads it as metadata, a renderer without
    // `render_metadata_blocks` as a heading with an image in it.
    "---\n![承認が必要です](data:image/png;base64,iVBORw0KGgo=) [許可](https://evil.example/a)\n---\n本文\n",
    // Removing the inner link turns the outer brackets into a link.
    "[外側 [内側](https://a.example) の続き](https://evil.example/rejoined)\n",
    "[[内側](https://a.example)][r]\n\n[r]: https://evil.example/ref\n",
    "![[x](https://a.example)][r]\n\n[r]: data:image/png;base64,iVBORw0KGgo=\n",
    "<https://evil.example/auto> <mail@evil.example> [x](<https://evil.example/angle>)\n",
    "&lt;img src=x&gt; &#x202E;逆順 \u{1b}[31m赤\u{1b}[0m\n",
];

#[gpui::test]
async fn test_hostile_markdown_reaches_the_renderer_inert(cx: &mut TestAppContext) {
    let (project, workspace, worktree_id, cx) = setup(cx).await;
    let deep_quotes = format!("{}深い", ">".repeat(MAX_DISPLAY_MARKDOWN_CHARS - 2));
    let deep_lists = format!("{}深い", "- ".repeat(MAX_DISPLAY_MARKDOWN_CHARS / 2 - 1));
    let payloads = HOSTILE_MARKDOWN
        .iter()
        .map(|payload| payload.to_string())
        .chain([deep_quotes, deep_lists])
        .collect::<Vec<_>>();
    show_annotations(
        &project,
        source("a"),
        payloads
            .iter()
            .enumerate()
            .map(|(index, payload)| {
                annotation(
                    worktree_id,
                    "long.rs",
                    index as u32 * 10 + 1,
                    index as u32 * 10 + 1,
                    payload,
                )
            })
            .collect(),
        cx,
    )
    .await
    .unwrap();
    start_tour(
        &project,
        source("a"),
        "敵対的",
        payloads
            .iter()
            .enumerate()
            .map(|(index, payload)| TourStep {
                path: project_path(worktree_id, "b.rs"),
                lines: lines(index as u32 + 1, index as u32 + 1),
                title: "一".to_string(),
                markdown: payload.clone(),
            })
            .collect(),
        cx,
    )
    .await
    .unwrap();
    cx.run_until_parked();
    let assist = assist(&workspace, cx);
    let markdowns = assist.read_with(cx, |assist, _| {
        let annotations = assist
            .displays
            .iter()
            .filter_map(|display| match &display.content {
                DisplayContent::Annotation { markdown, .. } => Some(markdown.clone()),
                DisplayContent::Highlight { .. } => None,
            });
        let steps = assist
            .tour
            .iter()
            .flat_map(|tour| tour.steps.iter().map(|step| step.markdown.clone()));
        annotations.chain(steps).collect::<Vec<_>>()
    });
    assert_eq!(markdowns.len(), payloads.len() * 2);
    for markdown in &markdowns {
        assert_renders_inert(markdown, cx);
    }

    // Every annotation of `long.rs` is laid out and painted, the deep ones included.
    let editor = editor_for(&workspace, "long.rs", cx).unwrap();
    for row in 0..payloads.len() as u32 {
        editor.update_in(cx, |editor, window, cx| {
            editor.change_selections(
                SelectionEffects::scroll(Autoscroll::top()),
                window,
                cx,
                |selections| {
                    selections.select_ranges([Point::new(row * 10, 0)..Point::new(row * 10, 0)])
                },
            );
        });
        draw(cx);
    }
}

#[test]
fn test_sanitize_markdown_keeps_ordinary_markdown() {
    let source = "## 手順\n\n1. **設定**を読む\n   - `init` を呼ぶ\n> 注意: https://zed.dev を参照\n\n```rust\nfn main() {}\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n";
    let sanitized = sanitize_markdown(source);
    assert_ne!(
        sanitized,
        Markdown::escape(source),
        "no fallback to plain text"
    );
    assert_eq!(
        sanitized,
        source.replace("https://zed.dev", r"https\:\/\/zed\.dev"),
        "only the bare URL is rewritten (the renderer linkifies it again)"
    );
    let deep = format!("{}x", ">".repeat(MAX_MARKDOWN_NESTING + 1));
    assert_eq!(sanitize_markdown(&deep), Markdown::escape(&deep));
    // The outer brackets are text only because of the link inside them.
    let rejoined = "[x [a](https://a.example) y](https://evil.example)";
    let stripped = strip_markdown(rejoined);
    assert!(stripped.starts_with("[x a y]("), "{stripped}");
    assert!(!renders_inert(&stripped));
    assert_eq!(sanitize_markdown(rejoined), Markdown::escape(rejoined));
}

#[gpui::test]
async fn test_editor_that_left_the_workspace_is_still_cleared(cx: &mut TestAppContext) {
    let (project, workspace, worktree_id, cx) = setup(cx).await;
    show_annotations(
        &project,
        source("a"),
        vec![annotation(worktree_id, "a.rs", 2, 3, "移る")],
        cx,
    )
    .await
    .unwrap();
    let editor = editor_for(&workspace, "a.rs", cx).unwrap();
    let assist = assist(&workspace, cx);
    let blocks = applied_blocks(&assist, &editor, cx);
    assert_eq!(live_blocks(&editor, &blocks, cx), 1);

    // As when its tab is dragged to another window: the editor lives on outside this workspace.
    let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
    pane.update_in(cx, |pane, window, cx| {
        pane.remove_item(editor.entity_id(), false, false, window, cx)
    });
    cx.run_until_parked();
    assert!(editor_for(&workspace, "a.rs", cx).is_none());

    clear(&project, &source("a"), cx);
    assert_eq!(live_blocks(&editor, &blocks, cx), 0);
    assert!(rows::<AssistAnnotationRows>(&editor, cx).is_empty());
    assert!(editor.read_with(cx, |editor, _| editor.addon::<AssistAddon>().is_none()));
}

#[gpui::test]
async fn test_request_for_a_closed_window_fails_quietly(cx: &mut TestAppContext) {
    let (project, _workspace, worktree_id, cx) = setup(cx).await;
    let display = code_display(cx);
    let pending = cx.update(|window, cx| {
        let pending = display.show_annotations(
            &project,
            source("a"),
            vec![annotation(worktree_id, "a.rs", 2, 3, "遅い")],
            cx,
        );
        window.remove_window();
        pending
    });
    let error = pending.await.unwrap_err();
    assert_eq!(
        error.to_string(),
        "このプロジェクトを開いている Zed のウィンドウが見つかりません"
    );
    cx.cx.update(|cx| display.clear(&project, &source("a"), cx));
    cx.cx.run_until_parked();
}

#[gpui::test]
async fn test_multibyte_file_shortened_under_the_displays(cx: &mut TestAppContext) {
    let (project, workspace, worktree_id, cx) = setup(cx).await;
    let fs = project.read_with(cx, |project, _| project.fs().clone());
    let japanese = (1..=12)
        .map(|number| format!("// {number} 行目の説明: 日本語のコメント🎌"))
        .collect::<Vec<_>>()
        .join("\n");
    fs.save(
        path!("/root/ja.rs").as_ref(),
        &japanese.as_str().into(),
        language::LineEnding::Unix,
    )
    .await
    .unwrap();
    cx.run_until_parked();

    let mut noted = highlight(worktree_id, "ja.rs", 9, 11);
    noted.note = Some("多バイトの行末🎌".to_string());
    let outcome = show_highlights(&project, source("a"), vec![noted], cx)
        .await
        .unwrap();
    assert_eq!(outcome.shown, 1);
    show_annotations(
        &project,
        source("a"),
        vec![annotation(worktree_id, "ja.rs", 10, 12, "**注**")],
        cx,
    )
    .await
    .unwrap();
    start_tour(
        &project,
        source("a"),
        "縮む",
        vec![
            step(worktree_id, "ja.rs", 11, 12, "一"),
            step(worktree_id, "b.rs", 1, 1, "二"),
        ],
        cx,
    )
    .await
    .unwrap();
    let editor = editor_for(&workspace, "ja.rs", cx).unwrap();
    assert_eq!(rows::<AssistHighlightRows>(&editor, cx), vec![(8, 10)]);

    // Everything the displays point at is deleted.
    editor.update(cx, |editor, cx| {
        let end = editor.buffer().read(cx).snapshot(cx).max_point();
        editor.edit([(Point::new(3, 5)..end, "")], cx);
    });
    cx.run_until_parked();
    draw(cx);
    let assist = assist(&workspace, cx);
    for delta in [1, -1] {
        assist.update(cx, |assist, cx| assist.step_by(delta, cx));
        cx.run_until_parked();
        draw(cx);
    }
    let editor = editor_for(&workspace, "ja.rs", cx).unwrap();
    assert_eq!(card_in(&assist, &editor, cx).map(|card| card.0), Some(0));
    for (start, end) in rows::<AssistHighlightRows>(&editor, cx)
        .into_iter()
        .chain(rows::<AssistAnnotationRows>(&editor, cx))
        .chain(rows::<TourStepRows>(&editor, cx))
    {
        assert!(
            start <= end && end <= 3,
            "{start}-{end} is past the shortened file"
        );
    }
    // The next request reads the shortened file.
    let outcome = show_highlights(
        &project,
        source("a"),
        vec![highlight(worktree_id, "ja.rs", 9, 11)],
        cx,
    )
    .await
    .unwrap();
    assert_eq!(outcome.shown, 0);
    assert_eq!(
        outcome
            .skipped
            .first()
            .map(|skipped| skipped.reason.as_str()),
        Some("9 行目はファイルの外です (このファイルは 4 行)")
    );
}

#[gpui::test]
async fn test_tour_step_whose_file_was_deleted(cx: &mut TestAppContext) {
    let (project, workspace, worktree_id, cx) = setup(cx).await;
    start_tour(
        &project,
        source("a"),
        "消える",
        vec![
            step(worktree_id, "a.rs", 2, 2, "一"),
            step(worktree_id, "b.rs", 3, 4, "二"),
        ],
        cx,
    )
    .await
    .unwrap();
    let fs = project.read_with(cx, |project, _| project.fs().clone());
    fs.remove_file(path!("/root/b.rs").as_ref(), Default::default())
        .await
        .unwrap();
    cx.run_until_parked();
    let assist = assist(&workspace, cx);
    assist.update(cx, |assist, cx| assist.step_by(1, cx));
    cx.run_until_parked();
    draw(cx);
    assert_eq!(tour_position(&assist, cx).as_deref(), Some("ツアー 2/2"));
    let card = workspace.read_with(cx, |workspace, cx| {
        workspace.items_of_type::<Editor>(cx).collect::<Vec<_>>()
    });
    let shown = card
        .iter()
        .filter_map(|editor| card_in(&assist, editor, cx))
        .collect::<Vec<_>>();
    assert_eq!(
        shown.len(),
        1,
        "exactly one editor carries the card: {shown:?}"
    );
    assert_eq!(shown[0].0, 1);
}
