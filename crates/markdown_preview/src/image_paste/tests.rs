use super::*;
use editor::actions::{ConfirmCompletion, Paste, ShowCompletions};
use editor::{MultiBufferOffset, ToOffset as _};
use fs::{FakeFs, Fs as _};
use gpui::{Modifiers, Pixels, TestAppContext, VisualContext as _, VisualTestContext, point, px};
use serde_json::json;
use std::path::{Path, PathBuf};
use util::path;
use util::rel_path::rel_path;
use util::test::marked_text_ranges;
use workspace::{MultiWorkspace, SelectedEntry, Workspace};

const PNG_BYTES: [u8; 8] = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];

/// A whole 1x1 grey PNG, for the paths that decode what they read.
const DECODABLE_PNG: [u8; 67] = [
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x00, 0x00, 0x00, 0x00, 0x3a, 0x7e, 0x9b,
    0x55, 0x00, 0x00, 0x00, 0x0a, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x60, 0x00, 0x00, 0x00,
    0x02, 0x00, 0x01, 0x48, 0xaf, 0xa4, 0x71, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae,
    0x42, 0x60, 0x82,
];

fn query(marked: &str) -> Option<PathQuery> {
    let (line, cursor) = marked
        .split_once('ˇ')
        .map(|(before, after)| (format!("{before}{after}"), before.len()))
        .expect("the line marks the cursor");
    path_query(&line, cursor)
}

#[test]
fn finds_the_path_being_typed() {
    assert_eq!(
        query("![alt](imaˇ"),
        Some(PathQuery {
            slot: PathSlot::MarkdownImage,
            start: 7,
            end: 10,
        })
    );
    assert_eq!(
        query("see [the guide](guˇ) here"),
        Some(PathQuery {
            slot: PathSlot::MarkdownLink,
            start: 16,
            end: 18,
        })
    );
    // The completion replaces the rest of the destination, not the text after it.
    assert_eq!(
        query("![a](imˇages/x.png) tail"),
        Some(PathQuery {
            slot: PathSlot::MarkdownImage,
            start: 5,
            end: 17,
        })
    );
    assert_eq!(
        query("![a [nested] alt](pˇ").map(|found| found.slot),
        Some(PathSlot::MarkdownImage)
    );
    assert_eq!(
        query("![写真](画ˇ"),
        Some(PathQuery {
            slot: PathSlot::MarkdownImage,
            start: "![写真](".len(),
            end: "![写真](画".len(),
        })
    );

    assert_eq!(query("![a](x.png) and moreˇ"), None);
    assert_eq!(query("[a\\](pˇ"), None);
    assert_eq!(query("[a](https:ˇ"), None);
    assert_eq!(query("[a](#sectˇ"), None);
    assert_eq!(query("plain textˇ"), None);

    assert_eq!(
        query("<img alt=\"x\" src=\"imaˇ\">"),
        Some(PathQuery {
            slot: PathSlot::HtmlImage,
            start: 18,
            end: 21,
        })
    );
    assert_eq!(
        query("<a href='doˇ"),
        Some(PathQuery {
            slot: PathSlot::HtmlLink,
            start: 9,
            end: 11,
        })
    );
    assert_eq!(query("<img src=\"a.png\" alt=\"ˇ"), None);
    assert_eq!(query("<img src=\"a.png\">ˇ"), None);
    assert_eq!(query("<img src=aˇ"), None);
    // A link typed after an HTML tag on the same line is the one being typed.
    assert_eq!(
        query("<br> [x](aˇ").map(|found| found.slot),
        Some(PathSlot::MarkdownLink)
    );
}

#[test]
fn ranks_paths_from_the_document() {
    let files = [
        "docs/readme.md",
        "docs/guide.md",
        "docs/images/photo.png",
        "docs/my file.png",
        "assets/logo.svg",
        "notes.txt",
    ]
    .map(|path| rel_path(path).into_arc());
    let ranked = |slot| {
        let mut candidates =
            path_candidates(rel_path("docs/readme.md"), files.iter().cloned(), slot);
        candidates.sort_by(|left, right| left.rank.cmp(&right.rank));
        candidates
            .into_iter()
            .map(|candidate| candidate.display)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        ranked(PathSlot::MarkdownImage),
        [
            "my file.png",
            "images/photo.png",
            "../assets/logo.svg",
            "guide.md",
            "../notes.txt",
        ],
        "an image slot lists the pictures first, nearest first, and never the document itself"
    );
    assert_eq!(
        ranked(PathSlot::HtmlLink),
        [
            "guide.md",
            "my file.png",
            "../notes.txt",
            "images/photo.png",
            "../assets/logo.svg",
        ],
        "a link lists the Markdown files first"
    );
}

#[test]
fn names_pasted_images_by_the_time() {
    let time = chrono::NaiveDate::from_ymd_opt(2026, 9, 25)
        .and_then(|date| date.and_hms_opt(10, 5, 3))
        .expect("a valid time");
    assert_eq!(pasted_image_name(time, "png"), "image-20260925-100503.png");
    assert_eq!(alt_text(rel_path("images/a[1].png")), "a\\[1\\]");
}

fn init_test(cx: &mut TestAppContext) -> Arc<workspace::AppState> {
    cx.update(|cx| {
        let app_state = workspace::AppState::test(cx);
        editor::init(cx);
        crate::init(cx);
        app_state
    })
}

fn markdown_language() -> Arc<language::Language> {
    Arc::new(language::Language::new(
        language::LanguageConfig {
            name: "Markdown".into(),
            matcher: Arc::new(language::LanguageMatcher {
                path_suffixes: vec!["md".into()],
                ..Default::default()
            }),
            ..Default::default()
        },
        None,
    ))
}

struct Opened {
    workspace: Entity<Workspace>,
    editor: Entity<Editor>,
}

async fn project_tree(fs: &Arc<FakeFs>, readme: &str) {
    fs.insert_tree(
        path!("/project"),
        json!({
            "docs": {
                "readme.md": readme,
                "guide.md": "# Guide",
                "images": {"photo.png": "photo"},
                "my file.png": "spaced",
            },
            "assets": {"logo.svg": "<svg/>", "folder": {}},
            "notes.txt": "notes",
        }),
    )
    .await;
}

/// Opens `path` of `project` in a workspace window, with the cursor and
/// selection that `marked` marks and the editor focused and laid out.
async fn open_in_workspace<'a>(
    project: Entity<Project>,
    path: &str,
    marked: &str,
    cx: &'a mut TestAppContext,
) -> (Opened, &'a mut VisualTestContext) {
    project
        .read_with(cx, |project, _| project.languages().clone())
        .add(markdown_language());
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace =
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
    cx.run_until_parked();
    let worktree_id = project.read_with(cx, |project, cx| {
        project
            .worktrees(cx)
            .next()
            .map(|worktree| worktree.read(cx).id())
            .expect("the project has a worktree")
    });
    let item = workspace
        .update_in(cx, |workspace, window, cx| {
            workspace.open_path((worktree_id, rel_path(path)), None, true, window, cx)
        })
        .await
        .unwrap_or_else(|error| panic!("{path} should open: {error:#}"));
    let editor = item
        .downcast::<Editor>()
        .expect("the file opens in an editor");
    cx.run_until_parked();
    select_marked(&editor, marked, cx);
    (Opened { workspace, editor }, cx)
}

fn select_marked(editor: &Entity<Editor>, marked: &str, cx: &mut VisualTestContext) {
    let (text, selections) = marked_text_ranges(marked, true);
    editor.update_in(cx, |editor, window, cx| {
        if editor.text(cx) != text {
            editor.set_text(text, window, cx);
        }
        editor.change_selections(Default::default(), window, cx, |selections_collection| {
            selections_collection.select_ranges(
                selections
                    .iter()
                    .map(|range| MultiBufferOffset(range.start)..MultiBufferOffset(range.end)),
            );
        });
        window.focus(&editor.focus_handle(cx), cx);
    });
    cx.update(|window, cx| {
        window.refresh();
        window.draw(cx).clear(cx);
    });
    cx.run_until_parked();
}

async fn open_local<'a>(
    marked: &str,
    cx: &'a mut TestAppContext,
) -> (Opened, Arc<FakeFs>, &'a mut VisualTestContext) {
    let _app_state = init_test(cx);
    let fs = FakeFs::new(cx.executor());
    project_tree(&fs, "").await;
    fs.insert_tree(
        path!("/outside"),
        json!({"pic.png": "outside picture", "shot.jpg": "outside shot", "notes.txt": "x"}),
    )
    .await;
    let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
    let (opened, cx) = open_in_workspace(project, "docs/readme.md", marked, cx).await;
    (opened, fs, cx)
}

fn text(opened: &Opened, cx: &mut VisualTestContext) -> String {
    opened.editor.read_with(cx, |editor, cx| editor.text(cx))
}

fn cursor(opened: &Opened, cx: &mut VisualTestContext) -> usize {
    opened.editor.update(cx, |editor, cx| {
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        editor
            .selections
            .newest_anchor()
            .head()
            .to_offset(&snapshot)
            .0
    })
}

fn has_notice(opened: &Opened, cx: &mut VisualTestContext) -> bool {
    opened.workspace.read_with(cx, |workspace, _| {
        workspace
            .notification_ids()
            .contains(&NotificationId::unique::<ImageInsertNotice>())
    })
}

fn image_item(format: gpui::ImageFormat) -> ClipboardItem {
    ClipboardItem::new_image(&gpui::Image::from_bytes(format, PNG_BYTES.to_vec()))
}

fn paste(opened: &Opened, item: &ClipboardItem, cx: &mut VisualTestContext) {
    opened
        .editor
        .update_in(cx, |editor, window, cx| editor.paste_item(item, window, cx));
}

/// The file name in `![name](images/name.ext)`, checking the link's shape.
fn pasted_name(text: &str, prefix: &str, suffix: &str) -> String {
    let link = text
        .strip_prefix(prefix)
        .and_then(|rest| rest.strip_suffix(suffix))
        .unwrap_or_else(|| panic!("{text:?} should be {prefix:?}, a link, {suffix:?}"));
    let (alt, destination) = link
        .strip_prefix("![")
        .and_then(|rest| rest.strip_suffix(')'))
        .and_then(|rest| rest.split_once("]("))
        .unwrap_or_else(|| panic!("{link:?} should be an image link"));
    let name = destination
        .strip_prefix("images/")
        .unwrap_or_else(|| panic!("{destination:?} should be in images/"));
    assert!(
        name.starts_with("image-") && name.len() == "image-YYYYMMDD-HHMMSS.png".len(),
        "{name:?} should be named after the time"
    );
    assert_eq!(
        Some(alt),
        name.strip_suffix(".png"),
        "the alt text is the name"
    );
    name.to_string()
}

#[gpui::test]
async fn pastes_a_lone_image_into_images_next_to_the_document(cx: &mut TestAppContext) {
    let (opened, fs, cx) = open_local("Intro ˇ\nmore\n", cx).await;
    cx.write_to_clipboard(image_item(gpui::ImageFormat::Png));
    cx.dispatch_action(Paste);
    cx.run_until_parked();

    let text = text(&opened, cx);
    let name = pasted_name(&text, "Intro ", "\nmore\n");
    assert_eq!(
        fs.load_bytes(&Path::new(path!("/project/docs/images")).join(&name))
            .await
            .ok(),
        Some(PNG_BYTES.to_vec())
    );
    assert_eq!(
        cursor(&opened, cx),
        text.len() - "\nmore\n".len(),
        "the cursor ends up after the link"
    );
    assert!(!has_notice(&opened, cx));
}

#[gpui::test]
async fn text_on_the_clipboard_wins_over_an_image(cx: &mut TestAppContext) {
    let (opened, fs, cx) = open_local("Intro ˇ\n", cx).await;
    let mut item = image_item(gpui::ImageFormat::Png);
    item.entries.insert(
        0,
        ClipboardEntry::String(gpui::ClipboardString::new("cells".into())),
    );
    paste(&opened, &item, cx);
    cx.run_until_parked();

    assert_eq!(text(&opened, cx), "Intro cells\n");
    assert_eq!(
        fs.files()
            .iter()
            .filter(|file| file.to_string_lossy().contains("image-"))
            .count(),
        0,
        "nothing is stored"
    );
}

#[gpui::test]
async fn a_second_paste_waits_for_the_first(cx: &mut TestAppContext) {
    let (opened, fs, cx) = open_local("ˇ\n", cx).await;
    let item = image_item(gpui::ImageFormat::Jpeg);
    paste(&opened, &item, cx);
    paste(&opened, &item, cx);
    cx.run_until_parked();

    let text = text(&opened, cx);
    assert_eq!(text.matches("![").count(), 1, "one link in {text:?}");
    assert!(
        text.contains(".jpg)"),
        "the format decides the extension: {text:?}"
    );
    assert_eq!(
        fs.files()
            .iter()
            .filter(|file| file.to_string_lossy().contains("image-"))
            .count(),
        1
    );
    assert!(
        has_notice(&opened, cx),
        "the second paste says why it did nothing"
    );
}

#[gpui::test]
async fn an_unsaved_document_gets_a_reason_instead_of_an_image(cx: &mut TestAppContext) {
    let (opened, fs, cx) = open_local("ˇ\n", cx).await;
    let project = opened
        .workspace
        .read_with(cx, |workspace, _| workspace.project().clone());
    let buffer = project.update(cx, |project, cx| {
        project.create_local_buffer("", Some(markdown_language()), false, cx)
    });
    let untitled = opened.workspace.update_in(cx, |workspace, window, cx| {
        let editor = cx.new(|cx| Editor::for_buffer(buffer, Some(project.clone()), window, cx));
        workspace.add_item_to_active_pane(Box::new(editor.clone()), None, true, window, cx);
        editor
    });
    let untitled = Opened {
        workspace: opened.workspace.clone(),
        editor: untitled,
    };
    paste(&untitled, &image_item(gpui::ImageFormat::Png), cx);
    cx.run_until_parked();

    assert_eq!(text(&untitled, cx), "");
    assert!(has_notice(&untitled, cx));
    assert_eq!(
        fs.files()
            .iter()
            .filter(|file| file.to_string_lossy().contains("image-"))
            .count(),
        0
    );
}

#[gpui::test]
async fn other_files_paste_images_as_before(cx: &mut TestAppContext) {
    let _app_state = init_test(cx);
    let fs = FakeFs::new(cx.executor());
    project_tree(&fs, "").await;
    let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
    let (opened, cx) = open_in_workspace(project, "notes.txt", "noteˇs", cx).await;
    paste(&opened, &image_item(gpui::ImageFormat::Png), cx);
    cx.run_until_parked();

    assert_eq!(text(&opened, cx), "notes");
    assert!(!has_notice(&opened, cx));
    assert!(!fs.is_dir(Path::new(path!("/project/images"))).await);
}

fn editor_point(
    opened: &Opened,
    x_fraction: f32,
    cx: &mut VisualTestContext,
) -> gpui::Point<Pixels> {
    let bounds = opened
        .editor
        .read_with(cx, |editor, _| editor.last_bounds().copied())
        .expect("the editor has been laid out");
    point(
        bounds.origin.x + bounds.size.width * x_fraction,
        bounds.origin.y + px(2.),
    )
}

/// Offers `dropped` to the item the pointer is over at `position`, as the pane
/// does, and returns whether the item took it.
fn drop_at(
    opened: &Opened,
    position: gpui::Point<Pixels>,
    dropped: &dyn Any,
    cx: &mut VisualTestContext,
) -> bool {
    cx.simulate_mouse_move(position, None, Modifiers::none());
    let pane = opened
        .workspace
        .read_with(cx, |workspace, _| workspace.active_pane().clone());
    let taken = pane.update_in(cx, |pane, window, cx| {
        let item = pane.active_item().expect("the document is open");
        item.handle_drop(pane, dropped, window, cx)
    });
    cx.run_until_parked();
    taken
}

fn external(paths: &[&str]) -> ExternalPaths {
    ExternalPaths(paths.iter().map(PathBuf::from).collect())
}

#[gpui::test]
async fn dropping_images_from_the_os_copies_and_links_them(cx: &mut TestAppContext) {
    let (opened, fs, cx) = open_local("first lineˇ\nsecond\n", cx).await;

    // Past the end of the first line.
    let position = editor_point(&opened, 0.7, cx);
    assert!(drop_at(
        &opened,
        position,
        &external(&[path!("/outside/pic.png")]),
        cx
    ));
    assert_eq!(
        text(&opened, cx),
        "first line ![pic](images/pic.png)\nsecond\n",
        "a space keeps the link off the word before it"
    );
    assert_eq!(
        fs.load(Path::new(path!("/project/docs/images/pic.png")))
            .await
            .ok()
            .as_deref(),
        Some("outside picture")
    );

    // Over the gutter the drop lands at the start of the line, one link a line.
    let position = editor_point(&opened, 0.005, cx);
    assert!(drop_at(
        &opened,
        position,
        &external(&[path!("/outside/pic.png"), path!("/outside/shot.jpg")]),
        cx
    ));
    assert_eq!(
        text(&opened, cx),
        "![pic-1](images/pic-1.png)\n![shot](images/shot.jpg) first line ![pic](images/pic.png)\nsecond\n",
        "a space keeps the last link off the word after it, and none goes before a line"
    );
    assert!(
        fs.is_file(Path::new(path!("/project/docs/images/pic-1.png")))
            .await
    );
}

#[gpui::test]
async fn dropping_from_the_project_panel_links_without_copying(cx: &mut TestAppContext) {
    let (opened, fs, cx) = open_local("ˇ\n", cx).await;
    let project = opened
        .workspace
        .read_with(cx, |workspace, _| workspace.project().clone());
    let selected = |path: &str, cx: &mut VisualTestContext| {
        project.read_with(cx, |project, cx| {
            let worktree = project.worktrees(cx).next().expect("a worktree");
            let worktree = worktree.read(cx);
            SelectedEntry {
                worktree_id: worktree.id(),
                entry_id: worktree
                    .entry_for_path(rel_path(path))
                    .unwrap_or_else(|| panic!("{path} is in the project"))
                    .id,
            }
        })
    };
    let logo = selected("assets/logo.svg", cx);
    let photo = selected("docs/images/photo.png", cx);
    let dropped = DraggedSelection {
        active_selection: logo,
        marked_selections: Arc::new([logo, photo]),
    };

    let position = editor_point(&opened, 0.5, cx);
    assert!(drop_at(&opened, position, &dropped, cx));
    assert_eq!(
        text(&opened, cx),
        "![logo](../assets/logo.svg)\n![photo](images/photo.png)\n"
    );
    assert!(
        !fs.is_file(Path::new(path!("/project/docs/images/logo.svg")))
            .await,
        "a project file is linked where it is"
    );
}

#[gpui::test]
async fn dropping_anything_but_images_opens_as_before(cx: &mut TestAppContext) {
    let (opened, _fs, cx) = open_local("ˇ\n", cx).await;
    let project = opened
        .workspace
        .read_with(cx, |workspace, _| workspace.project().clone());
    let folder = project.read_with(cx, |project, cx| {
        let worktree = project.worktrees(cx).next().expect("a worktree");
        let worktree = worktree.read(cx);
        SelectedEntry {
            worktree_id: worktree.id(),
            entry_id: worktree
                .entry_for_path(rel_path("assets/folder"))
                .expect("the folder is in the project")
                .id,
        }
    });
    let position = editor_point(&opened, 0.5, cx);

    assert!(!drop_at(
        &opened,
        position,
        &external(&[path!("/outside/notes.txt")]),
        cx
    ));
    assert!(!drop_at(
        &opened,
        position,
        &DraggedSelection {
            active_selection: folder,
            marked_selections: Arc::new([]),
        },
        cx
    ));
    // Above the editor (the tab bar) an image opens as before too.
    let bounds = opened
        .editor
        .read_with(cx, |editor, _| editor.last_bounds().copied())
        .expect("laid out");
    assert!(!drop_at(
        &opened,
        point(bounds.origin.x + px(40.), bounds.origin.y - px(4.)),
        &external(&[path!("/outside/pic.png")]),
        cx
    ));
    assert_eq!(text(&opened, cx), "\n");
}

fn show_completions(opened: &Opened, cx: &mut VisualTestContext) -> Vec<String> {
    opened.editor.update_in(cx, |editor, window, cx| {
        editor.show_completions(&ShowCompletions, window, cx)
    });
    cx.run_until_parked();
    completion_labels(opened, cx)
}

fn completion_labels(opened: &Opened, cx: &mut VisualTestContext) -> Vec<String> {
    opened.editor.read_with(cx, |editor, _| {
        editor
            .current_completions()
            .unwrap_or_default()
            .into_iter()
            .map(|completion| completion.label.text)
            .collect()
    })
}

fn confirm_first(opened: &Opened, cx: &mut VisualTestContext) {
    let task = opened.editor.update_in(cx, |editor, window, cx| {
        editor.confirm_completion(&ConfirmCompletion::default(), window, cx)
    });
    cx.run_until_parked();
    assert!(task.is_some(), "a completion was confirmed");
}

#[gpui::test]
async fn completes_paths_in_a_markdown_image(cx: &mut TestAppContext) {
    let (opened, _fs, cx) = open_local("![](ˇ)\n", cx).await;
    assert_eq!(
        show_completions(&opened, cx),
        [
            "my file.png",
            "images/photo.png",
            "../assets/logo.svg",
            "guide.md",
            "../notes.txt",
        ]
    );
    confirm_first(&opened, cx);
    assert_eq!(text(&opened, cx), "![](my%20file.png)\n");

    select_marked(&opened.editor, "![](../aˇ)\n", cx);
    let labels = show_completions(&opened, cx);
    assert_eq!(
        labels.first().map(String::as_str),
        Some("../assets/logo.svg"),
        "{labels:?}"
    );
    assert!(
        labels.iter().all(|label| label.starts_with("../")),
        "typing ../ climbs out of the document's folder: {labels:?}"
    );
    confirm_first(&opened, cx);
    assert_eq!(text(&opened, cx), "![](../assets/logo.svg)\n");
}

#[gpui::test]
async fn typing_a_link_destination_offers_markdown_files_first(cx: &mut TestAppContext) {
    let (opened, _fs, cx) = open_local("see [the guide]ˇ\n", cx).await;
    opened.editor.update_in(cx, |editor, window, cx| {
        editor.handle_input("(", window, cx)
    });
    cx.run_until_parked();
    let labels = completion_labels(&opened, cx);
    assert_eq!(
        labels.first().map(String::as_str),
        Some("guide.md"),
        "{labels:?}"
    );
    confirm_first(&opened, cx);
    assert!(
        text(&opened, cx).starts_with("see [the guide](guide.md"),
        "{:?}",
        text(&opened, cx)
    );
}

#[gpui::test]
async fn completes_image_sources_in_html(cx: &mut TestAppContext) {
    let (opened, _fs, cx) = open_local("<img alt=\"x\" src=\"ˇ\">\n", cx).await;
    let labels = show_completions(&opened, cx);
    assert_eq!(
        labels.first().map(String::as_str),
        Some("my file.png"),
        "{labels:?}"
    );
    confirm_first(&opened, cx);
    assert_eq!(text(&opened, cx), "<img alt=\"x\" src=\"my%20file.png\">\n");
}

#[gpui::test]
async fn offers_no_paths_outside_links(cx: &mut TestAppContext) {
    let (opened, _fs, cx) = open_local("plain ˇ text\n", cx).await;
    assert_eq!(show_completions(&opened, cx), Vec::<String>::new());
    let offered = opened.editor.update(cx, |editor, cx| {
        let provider = editor.completion_provider().expect("a provider");
        let buffer = editor
            .buffer()
            .read(cx)
            .as_singleton()
            .expect("a single buffer");
        let position = buffer.read(cx).anchor_before(6);
        provider.is_completion_trigger(&buffer, position, "(", false, cx)
    });
    assert!(
        !offered,
        "a parenthesis outside a link does not ask for paths"
    );
}

async fn remote_project(
    server_fs: Arc<FakeFs>,
    local_fs: Arc<FakeFs>,
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) -> (Entity<Project>, Entity<remote_server::HeadlessProject>) {
    cx.update(|cx| release_channel::init(semver::Version::new(0, 0, 0), cx));
    server_cx.update(|cx| release_channel::init(semver::Version::new(0, 0, 0), cx));
    let (options, server_session, connect_guard) = remote::RemoteClient::fake_server(cx, server_cx);
    server_cx.update(remote_server::HeadlessProject::init);
    let server_languages = Arc::new(language::LanguageRegistry::new(server_cx.executor()));
    let headless = server_cx.new(|cx| {
        remote_server::HeadlessProject::new(
            remote_server::HeadlessAppState {
                session: server_session,
                fs: server_fs,
                http_client: Arc::new(http_client::BlockedHttpClient),
                node_runtime: node_runtime::NodeRuntime::unavailable(),
                languages: server_languages,
                extension_host_proxy: Arc::new(extension::ExtensionHostProxy::new()),
                startup_time: std::time::Instant::now(),
            },
            false,
            cx,
        )
    });
    drop(connect_guard);
    let remote_client = remote::RemoteClient::connect_mock(options, cx).await;
    let app_state = cx.update(workspace::AppState::test);
    let project = cx.update(|cx| {
        Project::remote(
            remote_client,
            app_state.client.clone(),
            node_runtime::NodeRuntime::unavailable(),
            app_state.user_store.clone(),
            Arc::new(language::LanguageRegistry::test(
                cx.background_executor().clone(),
            )),
            local_fs,
            false,
            cx,
        )
    });
    project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path!("/project"), true, cx)
        })
        .await
        .unwrap_or_else(|error| panic!("the remote folder should open: {error:#}"));
    cx.run_until_parked();
    (project, headless)
}

#[gpui::test]
async fn pastes_drops_and_completes_in_a_remote_document(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let server_fs = FakeFs::new(server_cx.executor());
    project_tree(&server_fs, "").await;
    let local_fs = FakeFs::new(cx.executor());
    local_fs
        .insert_tree(path!("/outside"), json!({"pic.png": "on this machine"}))
        .await;
    // Held to the end: the host side has to outlive the client's project.
    let (project, _headless) =
        remote_project(server_fs.clone(), local_fs.clone(), cx, server_cx).await;
    cx.update(|cx| {
        editor::init(cx);
        crate::init(cx);
    });
    let (opened, cx) = open_in_workspace(project, "docs/readme.md", "Intro ˇ\n", cx).await;

    paste(&opened, &image_item(gpui::ImageFormat::Png), cx);
    cx.run_until_parked();
    server_cx.run_until_parked();
    cx.run_until_parked();
    let name = pasted_name(&text(&opened, cx), "Intro ", "\n");
    assert_eq!(
        server_fs
            .load_bytes(&Path::new(path!("/project/docs/images")).join(&name))
            .await
            .ok(),
        Some(PNG_BYTES.to_vec()),
        "the pasted image is stored on the remote host"
    );

    select_marked(&opened.editor, "ˇ\n", cx);
    let position = editor_point(&opened, 0.5, cx);
    assert!(drop_at(
        &opened,
        position,
        &external(&[path!("/outside/pic.png")]),
        cx
    ));
    server_cx.run_until_parked();
    cx.run_until_parked();
    assert_eq!(text(&opened, cx), "![pic](images/pic.png)\n");
    assert_eq!(
        server_fs
            .load(Path::new(path!("/project/docs/images/pic.png")))
            .await
            .ok()
            .as_deref(),
        Some("on this machine"),
        "a file dropped from this machine is uploaded"
    );

    select_marked(&opened.editor, "![](ˇ)\n", cx);
    let labels = show_completions(&opened, cx);
    assert!(
        labels.iter().any(|label| label == "images/photo.png"),
        "the remote worktree's files are offered: {labels:?}"
    );
}

#[gpui::test]
async fn a_prompt_box_in_markdown_pastes_as_before(cx: &mut TestAppContext) {
    let (opened, fs, cx) = open_local("ˇ\n", cx).await;
    let buffer = opened.editor.read_with(cx, |editor, cx| {
        editor
            .buffer()
            .read(cx)
            .as_singleton()
            .expect("a single buffer")
    });
    let project = opened
        .workspace
        .read_with(cx, |workspace, _| workspace.project().clone());
    // The saved document itself, shown in an auto-height box the way the
    // agent panel shows its Markdown prompt.
    let prompt = cx.new_window_entity(|window, cx| {
        let multi_buffer = cx.new(|cx| editor::MultiBuffer::singleton(buffer, cx));
        Editor::new(
            editor::EditorMode::AutoHeight {
                min_lines: 1,
                max_lines: Some(4),
            },
            multi_buffer,
            Some(project),
            window,
            cx,
        )
    });
    let prompt = Opened {
        workspace: opened.workspace,
        editor: prompt,
    };
    paste(&prompt, &image_item(gpui::ImageFormat::Png), cx);
    cx.run_until_parked();

    assert_eq!(text(&prompt, cx), "\n");
    assert!(!has_notice(&prompt, cx));
    assert_eq!(
        fs.files()
            .iter()
            .filter(|file| file.to_string_lossy().contains("image-"))
            .count(),
        0,
        "nothing is stored"
    );
}

#[gpui::test]
async fn an_image_dropped_on_the_tab_bar_of_an_unsaved_document_opens(cx: &mut TestAppContext) {
    let (opened, _fs, cx) = open_local("ˇ\n", cx).await;
    let project = opened
        .workspace
        .read_with(cx, |workspace, _| workspace.project().clone());
    let buffer = project.update(cx, |project, cx| {
        project.create_local_buffer("", Some(markdown_language()), false, cx)
    });
    let untitled = opened.workspace.update_in(cx, |workspace, window, cx| {
        let editor = cx.new(|cx| Editor::for_buffer(buffer, Some(project.clone()), window, cx));
        workspace.add_item_to_active_pane(Box::new(editor.clone()), None, true, window, cx);
        editor
    });
    let untitled = Opened {
        workspace: opened.workspace.clone(),
        editor: untitled,
    };
    select_marked(&untitled.editor, "ˇ", cx);
    let bounds = untitled
        .editor
        .read_with(cx, |editor, _| editor.last_bounds().copied())
        .expect("laid out");
    assert!(!drop_at(
        &untitled,
        point(bounds.origin.x + px(40.), bounds.origin.y - px(4.)),
        &external(&[path!("/outside/pic.png")]),
        cx
    ));
    assert!(
        !has_notice(&untitled, cx),
        "a drop meant for the tab bar says nothing about the document"
    );

    let position = editor_point(&untitled, 0.5, cx);
    assert!(
        drop_at(
            &untitled,
            position,
            &external(&[path!("/outside/pic.png")]),
            cx
        ),
        "over the text, the drop is taken to say why no link is inserted"
    );
    assert!(has_notice(&untitled, cx));
    assert_eq!(text(&untitled, cx), "");
}

/// The project paths of the items open in the pane the document is in.
fn open_paths(opened: &Opened, cx: &mut VisualTestContext) -> Vec<String> {
    let pane = opened
        .workspace
        .read_with(cx, |workspace, _| workspace.active_pane().clone());
    pane.read_with(cx, |pane, cx| {
        pane.items()
            .filter_map(|item| item.project_path(cx))
            .map(|project_path| project_path.path.as_unix_str().to_string())
            .collect()
    })
}

#[gpui::test]
async fn a_mixed_drop_links_the_images_and_opens_the_rest(cx: &mut TestAppContext) {
    let (opened, _fs, cx) = open_local("ˇ\n", cx).await;
    let project = opened
        .workspace
        .read_with(cx, |workspace, _| workspace.project().clone());
    let selected = |path: &str, cx: &mut VisualTestContext| {
        project.read_with(cx, |project, cx| {
            let worktree = project.worktrees(cx).next().expect("a worktree");
            let worktree = worktree.read(cx);
            SelectedEntry {
                worktree_id: worktree.id(),
                entry_id: worktree
                    .entry_for_path(rel_path(path))
                    .unwrap_or_else(|| panic!("{path} is in the project"))
                    .id,
            }
        })
    };
    let photo = selected("docs/images/photo.png", cx);
    let notes = selected("notes.txt", cx);
    let guide = selected("docs/guide.md", cx);
    let folder = selected("assets/folder", cx);
    let position = editor_point(&opened, 0.5, cx);

    assert!(drop_at(
        &opened,
        position,
        &DraggedSelection {
            active_selection: notes,
            marked_selections: Arc::new([photo, notes, folder, guide]),
        },
        cx
    ));
    assert_eq!(text(&opened, cx), "![photo](images/photo.png)\n");
    assert!(
        opened
            .workspace
            .read_with(cx, |workspace, _| workspace.notification_ids().is_empty()),
        "a folder selected alongside is not opened as a file"
    );
    let open = open_paths(&opened, cx);
    assert!(
        open.iter().any(|path| path == "notes.txt")
            && open.iter().any(|path| path == "docs/guide.md"),
        "every file besides the image opens: {open:?}"
    );
    assert!(
        !open.iter().any(|path| path == "docs/images/photo.png"),
        "the image is linked, not opened: {open:?}"
    );

    // From the OS as well: the image is copied and linked, the text opens.
    opened.workspace.update_in(cx, |workspace, window, cx| {
        workspace.activate_item(&opened.editor, true, true, window, cx);
    });
    select_marked(&opened.editor, "ˇ\n", cx);
    let position = editor_point(&opened, 0.5, cx);
    assert!(drop_at(
        &opened,
        position,
        &external(&[path!("/outside/notes.txt"), path!("/outside/pic.png")]),
        cx
    ));
    assert_eq!(text(&opened, cx), "![pic](images/pic.png)\n");
    let absolute_paths: Vec<PathBuf> = opened.workspace.read_with(cx, |workspace, cx| {
        let project = workspace.project().read(cx);
        workspace
            .items(cx)
            .filter_map(|item| item.project_path(cx))
            .filter_map(|project_path| project.absolute_path(&project_path, cx))
            .collect()
    });
    assert!(
        absolute_paths.contains(&PathBuf::from(path!("/outside/notes.txt"))),
        "the dropped text file opens: {absolute_paths:?}"
    );

    // A selection with nothing but other files still opens as before.
    opened.workspace.update_in(cx, |workspace, window, cx| {
        workspace.activate_item(&opened.editor, true, true, window, cx);
    });
    let position = editor_point(&opened, 0.5, cx);
    assert!(!drop_at(
        &opened,
        position,
        &DraggedSelection {
            active_selection: notes,
            marked_selections: Arc::new([notes, guide]),
        },
        cx
    ));
    assert!(!drop_at(&opened, position, &notes.entry_id, cx));
}

fn accepts(opened: &Opened, dropped: &dyn Any, cx: &mut VisualTestContext) -> bool {
    let pane = opened
        .workspace
        .read_with(cx, |workspace, _| workspace.active_pane().clone());
    pane.read_with(cx, |pane, cx| {
        pane.active_item()
            .expect("the document is open")
            .accepts_drop(pane, dropped, cx)
    })
}

/// What the pane asks while files are dragged over it, to show its drop
/// target over SSH only when the document would take them.
#[gpui::test]
async fn tells_the_pane_which_drops_it_takes(cx: &mut TestAppContext) {
    let (opened, _fs, cx) = open_local("ˇ\n", cx).await;
    assert!(accepts(
        &opened,
        &external(&[path!("/outside/pic.png")]),
        cx
    ));
    assert!(accepts(
        &opened,
        &external(&[path!("/outside/notes.txt"), path!("/outside/pic.png")]),
        cx
    ));
    assert!(!accepts(
        &opened,
        &external(&[path!("/outside/notes.txt")]),
        cx
    ));

    let pane = opened
        .workspace
        .read_with(cx, |workspace, _| workspace.active_pane().clone());
    pane.update(cx, |pane, _| {
        pane.drag_split_direction = Some(workspace::SplitDirection::Left)
    });
    assert!(!accepts(
        &opened,
        &external(&[path!("/outside/pic.png")]),
        cx
    ));
    pane.update(cx, |pane, _| pane.drag_split_direction = None);

    opened
        .editor
        .update(cx, |editor, _| editor.set_read_only(true));
    assert!(!accepts(
        &opened,
        &external(&[path!("/outside/pic.png")]),
        cx
    ));
}

#[test]
fn reads_windows_paths_into_the_wsl_distribution() {
    let read = |path: &str| path_in_wsl_distribution(path, "Ubuntu-24.04");
    assert_eq!(
        read(r"\\wsl$\Ubuntu-24.04\home\me\pic.png").as_deref(),
        Some("/home/me/pic.png")
    );
    assert_eq!(
        read(r"\\wsl.localhost\ubuntu-24.04\home\me\写真 1.png").as_deref(),
        Some("/home/me/写真 1.png"),
        "the share name is compared without regard to case"
    );
    assert_eq!(
        read(r"\\?\UNC\wsl.localhost\Ubuntu-24.04\srv\a.svg").as_deref(),
        Some("/srv/a.svg")
    );
    assert_eq!(
        read(r"\\?\unc\wsl$\Ubuntu-24.04\srv\a.svg").as_deref(),
        Some("/srv/a.svg"),
        "the device prefix is compared without regard to case"
    );
    assert_eq!(
        read(r"\\.\UNC\wsl.localhost\Ubuntu-24.04\srv\a.svg").as_deref(),
        Some("/srv/a.svg")
    );
    assert_eq!(read(r"\\?\C:\Users\me\pic.png"), None);
    assert_eq!(read(r"\\.\pipe\wsl$\Ubuntu-24.04\a.png"), None);
    assert_eq!(read("//wsl$/Ubuntu-24.04/").as_deref(), Some("/"));
    assert_eq!(
        read(r"\\wsl$\Debian\home\me\pic.png"),
        None,
        "another distribution"
    );
    assert_eq!(read(r"\\server\Ubuntu-24.04\pic.png"), None);
    assert_eq!(read(r"C:\Users\me\pic.png"), None);
    assert_eq!(read(r"\\wsl$\\pic.png"), None);
}

#[gpui::test]
async fn a_drop_at_a_split_edge_or_on_a_read_only_document_opens(cx: &mut TestAppContext) {
    let (opened, _fs, cx) = open_local("ˇ\n", cx).await;
    let position = editor_point(&opened, 0.5, cx);
    let pane = opened
        .workspace
        .read_with(cx, |workspace, _| workspace.active_pane().clone());

    pane.update(cx, |pane, _| {
        pane.drag_split_direction = Some(workspace::SplitDirection::Right)
    });
    assert!(!drop_at(
        &opened,
        position,
        &external(&[path!("/outside/pic.png")]),
        cx
    ));
    pane.update(cx, |pane, _| pane.drag_split_direction = None);

    opened
        .editor
        .update(cx, |editor, _| editor.set_read_only(true));
    assert!(!drop_at(
        &opened,
        position,
        &external(&[path!("/outside/pic.png")]),
        cx
    ));
    assert_eq!(text(&opened, cx), "\n");
}

#[gpui::test]
async fn offers_no_paths_inside_a_fenced_code_block(cx: &mut TestAppContext) {
    let (opened, _fs, cx) = open_local("ˇ\n", cx).await;
    let project = opened
        .workspace
        .read_with(cx, |workspace, _| workspace.project().clone());
    project.read_with(cx, |project, _| {
        project.languages().add(language::rust_lang())
    });
    let text = "```rust\nlet link = [a](\n```\n[b](\n";
    let buffer = opened.editor.update_in(cx, |editor, window, cx| {
        editor.set_text(text, window, cx);
        editor
            .buffer()
            .read(cx)
            .as_singleton()
            .expect("a single buffer")
    });
    buffer.update(cx, |buffer, cx| {
        buffer.set_language(Some(language::markdown_lang()), cx)
    });
    cx.run_until_parked();
    let offered_after = |needle: &str, cx: &mut VisualTestContext| {
        let offset = text.find(needle).expect("the needle is in the text") + needle.len();
        opened.editor.update(cx, |editor, cx| {
            let provider = editor.completion_provider().expect("a provider");
            let position = buffer.read(cx).anchor_before(offset);
            provider.is_completion_trigger(&buffer, position, "(", false, cx)
        })
    };
    assert!(
        !offered_after("[a](", cx),
        "a link inside Rust code is code"
    );
    assert!(
        offered_after("[b](", cx),
        "the Markdown after the fence is a link"
    );
}

#[gpui::test]
async fn a_paste_follows_edits_made_while_the_image_is_stored(cx: &mut TestAppContext) {
    let (opened, fs, cx) = open_local("日本語 ˇ末尾\n二行目ˇ\n", cx).await;
    paste(&opened, &image_item(gpui::ImageFormat::Png), cx);
    // Typed before the store finishes: the links still go where the cursors
    // were, now further along.
    opened.editor.update_in(cx, |editor, window, cx| {
        editor.change_selections(Default::default(), window, cx, |selections| {
            selections.select_ranges([MultiBufferOffset(0)..MultiBufferOffset(0)])
        });
        editor.insert("前置き", window, cx);
    });
    cx.run_until_parked();

    let text = text(&opened, cx);
    let (first_line, rest) = text.split_once('\n').expect("two lines");
    let name = pasted_name(first_line, "前置き日本語 ", "末尾");
    assert_eq!(
        rest,
        format!(
            "二行目![{}](images/{name})\n",
            name.trim_end_matches(".png")
        ),
        "every cursor gets the link"
    );
    assert_eq!(
        fs.files()
            .iter()
            .filter(|file| file.to_string_lossy().contains("image-"))
            .count(),
        1,
        "one image is stored for all the cursors"
    );
}

#[gpui::test]
async fn an_editor_released_while_the_image_is_stored_writes_nothing(cx: &mut TestAppContext) {
    let (opened, fs, cx) = open_local(
        "ˇ
", cx,
    )
    .await;
    let buffer = opened.editor.read_with(cx, |editor, cx| {
        editor
            .buffer()
            .read(cx)
            .as_singleton()
            .expect("a single buffer")
    });
    let project = opened
        .workspace
        .read_with(cx, |workspace, _| workspace.project().clone());
    let second = cx.new_window_entity(|window, cx| {
        Editor::for_buffer(buffer.clone(), Some(project), window, cx)
    });
    second.update_in(cx, |editor, window, cx| {
        editor.paste_item(&image_item(gpui::ImageFormat::Png), window, cx)
    });
    let released = second.downgrade();
    drop(second);
    cx.run_until_parked();

    assert!(released.upgrade().is_none(), "the editor is gone");
    assert_eq!(
        text(&opened, cx),
        "
",
        "no link lands in another editor of the document"
    );
    assert_eq!(
        fs.files()
            .iter()
            .filter(|file| file.to_string_lossy().contains("image-"))
            .count(),
        0,
        "the image stored for the released editor is taken away again"
    );

    // The document takes images again once the released editor's store ended.
    paste(&opened, &image_item(gpui::ImageFormat::Png), cx);
    cx.run_until_parked();
    assert_eq!(text(&opened, cx).matches("![").count(), 1);
}

#[gpui::test]
async fn an_image_whose_link_cannot_go_in_is_taken_away(cx: &mut TestAppContext) {
    let (opened, fs, cx) = open_local("ˇ\n", cx).await;
    paste(&opened, &image_item(gpui::ImageFormat::Png), cx);
    opened
        .editor
        .update(cx, |editor, _| editor.set_read_only(true));
    cx.run_until_parked();

    assert_eq!(text(&opened, cx), "\n");
    assert!(has_notice(&opened, cx), "the reason is told");
    assert_eq!(
        fs.files()
            .iter()
            .filter(|file| file.to_string_lossy().contains("image-"))
            .count(),
        0,
        "no image is left without a link to it"
    );

    // One the link went in for stays.
    opened
        .editor
        .update(cx, |editor, _| editor.set_read_only(false));
    paste(&opened, &image_item(gpui::ImageFormat::Png), cx);
    cx.run_until_parked();
    assert_eq!(
        fs.files()
            .iter()
            .filter(|file| file.to_string_lossy().contains("image-"))
            .count(),
        1
    );
}

/// Only what the insert stored itself is taken away when the links do not go
/// in: a file the project already had is linked where it is and never removed.
#[gpui::test]
async fn an_image_already_in_the_project_stays_when_its_link_cannot_go_in(cx: &mut TestAppContext) {
    let (opened, fs, cx) = open_local("ˇ\n", cx).await;
    let project = opened
        .workspace
        .read_with(cx, |workspace, _| workspace.project().clone());
    let worktree_id = project.read_with(cx, |project, cx| {
        project
            .worktrees(cx)
            .next()
            .map(|worktree| worktree.read(cx).id())
            .expect("the project has a worktree")
    });
    let document = ProjectPath {
        worktree_id,
        path: rel_path("docs/readme.md").into(),
    };
    let photo = ProjectPath {
        worktree_id,
        path: rel_path("docs/images/photo.png").into(),
    };
    opened.editor.update_in(cx, |editor, window, cx| {
        let targets = editor.selections.disjoint_anchor_ranges().collect();
        insert_image_links(
            editor,
            project.clone(),
            document.clone(),
            vec![
                LinkSource::Existing(photo.clone()),
                LinkSource::Store {
                    image: ImageData::LocalFile(PathBuf::from(path!("/outside/pic.png"))),
                    preferred_name: None,
                },
            ],
            targets,
            false,
            window,
            cx,
        );
        editor.set_read_only(true);
    });
    cx.run_until_parked();

    assert_eq!(text(&opened, cx), "\n");
    assert_eq!(
        fs.load(Path::new(path!("/project/docs/images/photo.png")))
            .await
            .ok()
            .as_deref(),
        Some("photo"),
        "the project's own image is left alone"
    );
    assert!(
        !fs.is_file(Path::new(path!("/project/docs/images/pic.png")))
            .await,
        "the copy made for the link is taken away"
    );
    assert!(
        fs.is_file(Path::new(path!("/outside/pic.png"))).await,
        "the dropped original stays"
    );
}

#[gpui::test]
async fn path_completions_reuse_the_files_until_they_change(cx: &mut TestAppContext) {
    let (opened, fs, cx) = open_local("![](ˇ)\n", cx).await;
    let built = || CANDIDATE_LISTS_BUILT.with(|built| built.get());
    let before = built();

    let labels = show_completions(&opened, cx);
    assert!(labels.iter().any(|label| label == "images/photo.png"));
    select_marked(&opened.editor, "![](imˇ)\n", cx);
    let labels = show_completions(&opened, cx);
    assert!(labels.iter().any(|label| label == "images/photo.png"));
    assert_eq!(
        built() - before,
        1,
        "typing on in the same path does not list the files again"
    );

    fs.insert_file(path!("/project/docs/images/new.png"), b"new".to_vec())
        .await;
    cx.run_until_parked();
    select_marked(&opened.editor, "![](ˇ)\n", cx);
    let labels = show_completions(&opened, cx);
    assert!(
        labels.iter().any(|label| label == "images/new.png"),
        "a file added since is offered: {labels:?}"
    );
    assert_eq!(built() - before, 2);

    select_marked(&opened.editor, "[](ˇ)\n", cx);
    show_completions(&opened, cx);
    assert_eq!(built() - before, 3, "a link ranks the files another way");
}

#[gpui::test]
async fn pasting_inside_a_fenced_code_block_pastes_as_before(cx: &mut TestAppContext) {
    let (opened, fs, cx) = open_local("ˇ\n", cx).await;
    let project = opened
        .workspace
        .read_with(cx, |workspace, _| workspace.project().clone());
    project.read_with(cx, |project, _| {
        project.languages().add(language::rust_lang())
    });
    let buffer = opened.editor.read_with(cx, |editor, cx| {
        editor
            .buffer()
            .read(cx)
            .as_singleton()
            .expect("a single buffer")
    });
    buffer.update(cx, |buffer, cx| {
        buffer.set_language(Some(language::markdown_lang()), cx)
    });
    select_marked(&opened.editor, "```rust\nlet x = ˇ;\n```\n", cx);
    paste(&opened, &image_item(gpui::ImageFormat::Png), cx);
    cx.run_until_parked();
    assert_eq!(text(&opened, cx), "```rust\nlet x = ;\n```\n");
    assert!(!has_notice(&opened, cx));

    select_marked(&opened.editor, "```rust\nlet x = ;\n```\nˇ\n", cx);
    paste(&opened, &image_item(gpui::ImageFormat::Png), cx);
    cx.run_until_parked();
    assert_eq!(
        text(&opened, cx).matches("](images/image-").count(),
        1,
        "after the fence it is Markdown again: {:?}",
        text(&opened, cx)
    );
    assert_eq!(
        fs.files()
            .iter()
            .filter(|file| file.to_string_lossy().contains("image-"))
            .count(),
        1
    );
}

fn entry_in(
    project: &Entity<Project>,
    root: &str,
    path: &str,
    cx: &mut VisualTestContext,
) -> SelectedEntry {
    project.read_with(cx, |project, cx| {
        let worktree = project
            .worktrees(cx)
            .find(|worktree| worktree.read(cx).root_name_str() == root)
            .unwrap_or_else(|| panic!("{root} is a worktree"));
        let worktree = worktree.read(cx);
        SelectedEntry {
            worktree_id: worktree.id(),
            entry_id: worktree
                .entry_for_path(rel_path(path))
                .unwrap_or_else(|| panic!("{path} is in {root}"))
                .id,
        }
    })
}

#[gpui::test]
async fn images_of_another_worktree_are_copied_next_to_the_document(cx: &mut TestAppContext) {
    let _app_state = init_test(cx);
    let fs = FakeFs::new(cx.executor());
    project_tree(&fs, "").await;
    fs.insert_tree(
        path!("/other"),
        json!({"art.png": "other art", "shot.jpg": "other shot"}),
    )
    .await;
    let project = Project::test(
        fs.clone(),
        [path!("/project").as_ref(), path!("/other").as_ref()],
        cx,
    )
    .await;
    let (opened, cx) = open_in_workspace(project.clone(), "docs/readme.md", "ˇ\n", cx).await;
    let position = editor_point(&opened, 0.5, cx);

    let art = entry_in(&project, "other", "art.png", cx);
    assert!(drop_at(&opened, position, &art.entry_id, cx));
    assert_eq!(text(&opened, cx), "![art](images/art.png)\n");
    assert_eq!(
        fs.load(Path::new(path!("/project/docs/images/art.png")))
            .await
            .ok()
            .as_deref(),
        Some("other art"),
        "the project panel's file is copied, not linked across worktrees"
    );

    select_marked(&opened.editor, "ˇ\n", cx);
    assert!(drop_at(
        &opened,
        position,
        &external(&[path!("/other/shot.jpg")]),
        cx
    ));
    assert_eq!(text(&opened, cx), "![shot](images/shot.jpg)\n");
    assert_eq!(
        fs.load(Path::new(path!("/project/docs/images/shot.jpg")))
            .await
            .ok()
            .as_deref(),
        Some("other shot"),
        "a file of the other worktree dropped from the OS is copied too"
    );
    assert!(!has_notice(&opened, cx));
}

#[gpui::test]
async fn images_of_another_remote_worktree_are_copied_on_the_host(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let server_fs = FakeFs::new(server_cx.executor());
    project_tree(&server_fs, "").await;
    server_fs
        .insert_tree(
            path!("/other"),
            json!({"art.png": "", "logo.svg": "<svg/>"}),
        )
        .await;
    server_fs
        .write(Path::new(path!("/other/art.png")), &DECODABLE_PNG)
        .await
        .expect("the picture is written");
    let local_fs = FakeFs::new(cx.executor());
    let (project, _headless) =
        remote_project(server_fs.clone(), local_fs.clone(), cx, server_cx).await;
    project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path!("/other"), true, cx)
        })
        .await
        .unwrap_or_else(|error| panic!("the second remote folder should open: {error:#}"));
    cx.run_until_parked();
    cx.update(|cx| {
        editor::init(cx);
        crate::init(cx);
    });
    let (opened, cx) = open_in_workspace(project.clone(), "docs/readme.md", "ˇ\n", cx).await;
    let position = editor_point(&opened, 0.5, cx);
    let art = entry_in(&project, "other", "art.png", cx);

    assert!(drop_at(&opened, position, &art.entry_id, cx));
    for _ in 0..3 {
        server_cx.run_until_parked();
        cx.run_until_parked();
    }
    assert!(!has_notice(&opened, cx));
    assert_eq!(text(&opened, cx), "![art](images/art.png)\n");
    assert_eq!(
        server_fs
            .load_bytes(Path::new(path!("/project/docs/images/art.png")))
            .await
            .ok(),
        Some(DECODABLE_PNG.to_vec()),
        "the file is copied on the host next to the document there"
    );

    // An SVG, which the host's image store cannot read, is copied all the same.
    select_marked(&opened.editor, "ˇ\n", cx);
    let logo = entry_in(&project, "other", "logo.svg", cx);
    assert!(drop_at(&opened, position, &logo.entry_id, cx));
    for _ in 0..3 {
        server_cx.run_until_parked();
        cx.run_until_parked();
    }
    assert!(!has_notice(&opened, cx));
    assert_eq!(text(&opened, cx), "![logo](images/logo.svg)\n");
    assert_eq!(
        server_fs
            .load(Path::new(path!("/project/docs/images/logo.svg")))
            .await
            .ok()
            .as_deref(),
        Some("<svg/>")
    );
}
