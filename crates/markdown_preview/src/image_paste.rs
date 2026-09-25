//! Images reaching a Markdown editor without the image dialog: pasted from the
//! clipboard, dropped from the OS or the project panel, and file paths offered
//! as completions inside link destinations and `src` / `href` attributes.

use std::any::Any;
use std::cell::RefCell;
use std::collections::HashSet;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::Context as _;
use editor::{
    Anchor, CompletionProvider, Editor, EditorDropHandler, ImagePasteHandler, SelectionEffects,
};
use fuzzy::StringMatchCandidate;
use gpui::{
    App, AppContext as _, ClipboardEntry, ClipboardItem, Context, Entity, EntityId, ExternalPaths,
    Focusable as _, Global, Task, WeakEntity, Window,
};
use language::{Buffer, CodeLabel, ToOffset as _};
use project::{
    Completion, CompletionDisplayOptions, CompletionResponse, CompletionSource, Project,
    ProjectEntryId, ProjectPath, Worktree, WorktreeId,
};
use remote::RemoteConnectionOptions;
use util::ResultExt as _;
use util::rel_path::RelPath;
use workspace::notifications::NotificationId;
use workspace::{DraggedSelection, Pane, Toast};

use crate::image_dialog::has_image_extension;
use crate::image_files::{ImageData, link_destination, remove_stored_image, store_image_next_to};

pub(crate) fn init(cx: &mut App) {
    cx.set_global(ImagePasteHandler(Rc::new(paste_image)));
    cx.set_global(EditorDropHandler {
        handle: Rc::new(drop_images),
        accepts: Rc::new(accepts_dropped_images),
    });
    cx.observe_new(|editor: &mut Editor, _, _| wrap_completion_provider(editor))
        .detach();
}

const MARKDOWN_EXTENSIONS: [&str; 2] = ["md", "markdown"];

/// How many paths one completion request offers. The list is filtered again
/// on every keystroke, so the rest are a few characters away.
const MAXIMUM_PATH_COMPLETIONS: usize = 100;

fn has_extension(name: &str, extensions: &[&str]) -> bool {
    let file_name = name.rsplit(['/', '\\']).next().unwrap_or(name);
    file_name.rsplit_once('.').is_some_and(|(stem, extension)| {
        !stem.is_empty()
            && extensions
                .iter()
                .any(|known| extension.eq_ignore_ascii_case(known))
    })
}

/// The name a pasted image is stored under: `image-YYYYMMDD-HHMMSS` with the
/// extension of the clipboard's format.
fn pasted_image_name(time: chrono::NaiveDateTime, extension: &str) -> String {
    format!("image-{}.{extension}", time.format("%Y%m%d-%H%M%S"))
}

/// The alt text a new link starts with, which is what the image dialog
/// suggests too: the file name without its extension.
fn alt_text(path: &RelPath) -> String {
    let file_name = path.file_name().unwrap_or_default();
    let stem = match file_name.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem,
        _ => file_name,
    };
    let mut escaped = String::with_capacity(stem.len());
    for character in stem.chars() {
        if matches!(character, '\\' | '[' | ']') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

/// The Markdown document an editor shows, as far as storing images next to it
/// goes.
enum DocumentState {
    NotMarkdown,
    Unavailable(&'static str),
    Saved {
        project: Entity<Project>,
        path: ProjectPath,
    },
}

fn is_markdown(buffer: &Buffer) -> bool {
    buffer
        .language()
        .is_some_and(|language| language.name() == "Markdown")
}

/// Whether the newest cursor is in the Markdown itself rather than in a fenced
/// code block, whose language takes a paste as code.
fn is_markdown_at_cursor(editor: &Editor, cx: &App) -> bool {
    let snapshot = editor.buffer().read(cx).snapshot(cx);
    snapshot
        .language_at(editor.selections.newest_anchor().head())
        .is_some_and(|language| is_markdown_layer(language.name().as_ref()))
}

/// The layers of a Markdown document that are its text rather than embedded
/// code: the block and inline grammars and the HTML it may contain.
fn is_markdown_layer(language_name: &str) -> bool {
    matches!(language_name, "Markdown" | "Markdown-Inline" | "HTML")
}

fn markdown_document(editor: &Editor, cx: &App) -> DocumentState {
    // Prompt boxes such as the agent panel's are Markdown too, but they are not
    // documents, and they handle pasted images themselves.
    if !editor.mode().is_full() {
        return DocumentState::NotMarkdown;
    }
    let Some(buffer) = editor.buffer().read(cx).as_singleton() else {
        return DocumentState::NotMarkdown;
    };
    let buffer = buffer.read(cx);
    if !is_markdown(buffer) {
        return DocumentState::NotMarkdown;
    }
    let Some(project) = editor.project().cloned() else {
        return DocumentState::Unavailable(
            "プロジェクトに属さないバッファーのため、画像を保存する場所がありません。",
        );
    };
    let Some(file) = buffer.file() else {
        return DocumentState::Unavailable(
            "画像はドキュメントの隣の images フォルダーに保存します。先にファイルを保存してください。",
        );
    };
    DocumentState::Saved {
        project,
        path: ProjectPath {
            worktree_id: file.worktree_id(cx),
            path: file.path().clone(),
        },
    }
}

struct ImageInsertNotice;

fn show_notice(editor: &Editor, message: String, cx: &mut App) {
    let Some(workspace) = editor.workspace() else {
        log::warn!("{message}");
        return;
    };
    workspace.update(cx, |workspace, cx| {
        workspace.show_toast(
            Toast::new(NotificationId::unique::<ImageInsertNotice>(), message),
            cx,
        );
    });
}

/// Editors that are still storing images for an earlier paste or drop, so a
/// second one is not stored while the first is on its way.
#[derive(Default)]
struct PendingImageInserts(HashSet<EntityId>);

impl Global for PendingImageInserts {}

/// The active entry of the last project panel selection declined because it
/// held more than images. The pane offers that entry on its own right after,
/// and it has to be declined as well.
#[derive(Default)]
struct DeclinedSelectionDrop(Option<ProjectEntryId>);

impl Global for DeclinedSelectionDrop {}

/// Where a link's target comes from.
enum LinkSource {
    /// A file already in the project, linked where it is.
    Existing(ProjectPath),
    /// An image to store next to the document first.
    Store {
        image: ImageData,
        preferred_name: Option<String>,
    },
}

fn paste_image(
    editor: &mut Editor,
    item: &ClipboardItem,
    window: &mut Window,
    cx: &mut Context<Editor>,
) -> bool {
    // Text copied alongside a picture (a spreadsheet's cells, a web page's
    // selection) is what the user usually means to paste.
    let has_text = item.entries().iter().any(|entry| match entry {
        ClipboardEntry::String(text) => !text.text().is_empty(),
        _ => false,
    });
    if has_text {
        return false;
    }
    let Some(image) = item.entries().iter().find_map(|entry| match entry {
        ClipboardEntry::Image(image) if !image.bytes.is_empty() => Some(image),
        _ => None,
    }) else {
        return false;
    };
    let document = markdown_document(editor, cx);
    if matches!(document, DocumentState::NotMarkdown) || !is_markdown_at_cursor(editor, cx) {
        return false;
    }
    let (project, document) = match document {
        DocumentState::NotMarkdown => return false,
        DocumentState::Unavailable(reason) => {
            show_notice(editor, format!("画像を貼り付けられません: {reason}"), cx);
            return true;
        }
        DocumentState::Saved { project, path } => (project, path),
    };
    let extension = image.format.extension();
    let name = pasted_image_name(chrono::Local::now().naive_local(), extension);
    let targets = editor.selections.disjoint_anchor_ranges().collect();
    insert_image_links(
        editor,
        project,
        document,
        vec![LinkSource::Store {
            image: ImageData::Bytes {
                bytes: image.bytes.clone(),
                extension: extension.to_string(),
            },
            preferred_name: Some(name),
        }],
        targets,
        false,
        window,
        cx,
    );
    true
}

fn drop_images(
    editor: &Editor,
    handle: WeakEntity<Editor>,
    pane: &Pane,
    dropped: &dyn Any,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    // Dropped near an edge, the pane splits and opens what was dropped there.
    let declined_selection = cx.default_global::<DeclinedSelectionDrop>().0.take();
    if pane.drag_split_direction().is_some() || editor.read_only(cx) {
        return false;
    }
    // A project panel selection declined here comes back as its active entry
    // alone, which has to open as the whole selection would have.
    if let Some(entry_id) = dropped.downcast_ref::<ProjectEntryId>()
        && declined_selection == Some(*entry_id)
    {
        return false;
    }
    let Some(DroppedFiles { images, rest }) = dropped_images(editor, dropped, cx) else {
        if let Some(selection) = dropped.downcast_ref::<DraggedSelection>() {
            cx.default_global::<DeclinedSelectionDrop>().0 =
                Some(selection.active_selection.entry_id);
        }
        return false;
    };
    // Over the tab bar or the toolbar the drop is not meant for the text.
    let Some(position) = editor.anchor_at_window_position(window.mouse_position()) else {
        return false;
    };
    let (project, document) = match markdown_document(editor, cx) {
        DocumentState::NotMarkdown => return false,
        DocumentState::Unavailable(reason) => {
            show_notice(
                editor,
                format!("画像のリンクを挿入できません: {reason}"),
                cx,
            );
            return true;
        }
        DocumentState::Saved { project, path } => (project, path),
    };
    // The pane is still updating the editor it handed the drop to.
    window.defer(cx, move |window, cx| {
        let Some(editor) = handle.upgrade() else {
            return;
        };
        editor.update(cx, |editor, cx| {
            insert_image_links(
                editor,
                project,
                document,
                images,
                vec![position..position],
                true,
                window,
                cx,
            );
            window.focus(&editor.focus_handle(cx), cx);
        });
        open_in_pane(&editor, rest, window, cx);
    });
    true
}

fn accepts_dropped_images(editor: &Editor, pane: &Pane, dropped: &dyn Any, cx: &App) -> bool {
    pane.drag_split_direction().is_none()
        && !editor.read_only(cx)
        && !matches!(markdown_document(editor, cx), DocumentState::NotMarkdown)
        && dropped_images(editor, dropped, cx).is_some()
}

/// A drop split into the images that become links and the rest.
struct DroppedFiles {
    images: Vec<LinkSource>,
    rest: DroppedRest,
}

/// What was dropped besides images, which opens as it would have without them.
enum DroppedRest {
    External(Vec<PathBuf>),
    Entries(Vec<ProjectEntryId>),
}

/// Hands what was dropped besides images back to the pane the editor is in,
/// which opens it as it opens any drop (and refuses what it cannot open).
fn open_in_pane(editor: &Entity<Editor>, rest: DroppedRest, window: &mut Window, cx: &mut App) {
    let is_empty = match &rest {
        DroppedRest::External(paths) => paths.is_empty(),
        DroppedRest::Entries(entries) => entries.is_empty(),
    };
    if is_empty {
        return;
    }
    let Some(pane) = editor
        .read(cx)
        .workspace()
        .and_then(|workspace| workspace.read(cx).pane_for(editor))
    else {
        log::warn!("no pane to open the dropped files other than images in");
        return;
    };
    // Offered to the editor again first, which takes nothing without images.
    pane.update(cx, |pane, cx| match rest {
        DroppedRest::External(paths) => {
            pane.handle_external_paths_drop(&ExternalPaths(paths.into()), window, cx)
        }
        DroppedRest::Entries(entries) => {
            for entry_id in entries {
                pane.handle_project_entry_drop(&entry_id, None, window, cx);
            }
        }
    });
}

/// The images in what was dropped and the rest, or `None` when there is no
/// image file in it, which leaves the whole drop to open as before.
fn dropped_images(editor: &Editor, dropped: &dyn Any, cx: &App) -> Option<DroppedFiles> {
    let project = editor.project()?.read(cx);
    let mut images = Vec::new();
    let rest = if let Some(paths) = dropped.downcast_ref::<ExternalPaths>() {
        let mut rest = Vec::new();
        for path in paths.paths() {
            if !has_image_extension(&path.to_string_lossy()) {
                rest.push(path.clone());
                continue;
            }
            images.push(match dropped_file_in_project(project, path, cx) {
                Some(project_path) => LinkSource::Existing(project_path),
                None => LinkSource::Store {
                    image: ImageData::LocalFile(path.clone()),
                    preferred_name: None,
                },
            });
        }
        DroppedRest::External(rest)
    } else {
        let entry_ids: Vec<ProjectEntryId> =
            if let Some(selection) = dropped.downcast_ref::<DraggedSelection>() {
                selection
                    .items()
                    .map(|selected| selected.entry_id)
                    .collect()
            } else if let Some(entry_id) = dropped.downcast_ref::<ProjectEntryId>() {
                vec![*entry_id]
            } else {
                return None;
            };
        let mut rest = Vec::new();
        for entry_id in entry_ids {
            let image = project.path_for_entry(entry_id, cx).filter(|project_path| {
                has_image_extension(project_path.path.as_unix_str())
                    && project
                        .entry_for_path(project_path, cx)
                        .is_some_and(|entry| entry.is_file())
            });
            match image {
                Some(project_path) => images.push(LinkSource::Existing(project_path)),
                // A folder selected alongside has nothing for the pane to open.
                None if project
                    .path_for_entry(entry_id, cx)
                    .and_then(|project_path| project.entry_for_path(&project_path, cx))
                    .is_some_and(|entry| entry.is_file()) =>
                {
                    rest.push(entry_id)
                }
                None => {}
            }
        }
        DroppedRest::Entries(rest)
    };
    (!images.is_empty()).then_some(DroppedFiles { images, rest })
}

/// The project file that a file dropped from the file manager is, when the
/// document can link to it where it is: a file of a local project, or a file
/// of the WSL distribution the project is opened in, reached from Windows
/// through `\\wsl$` or `\\wsl.localhost`.
fn dropped_file_in_project(project: &Project, path: &Path, cx: &App) -> Option<ProjectPath> {
    let project_path = if project.is_local() {
        project.find_project_path(path, cx)?
    } else {
        let Some(RemoteConnectionOptions::Wsl(options)) = project.remote_connection_options(cx)
        else {
            return None;
        };
        let linux_path = path_in_wsl_distribution(&path.to_string_lossy(), &options.distro_name)?;
        project.find_project_path(Path::new(&linux_path), cx)?
    };
    project
        .entry_for_path(&project_path, cx)
        .is_some_and(|entry| entry.is_file())
        .then_some(project_path)
}

/// The path inside the WSL distribution `distribution` that a Windows path
/// into it (`\\wsl$\<distribution>\…` or `\\wsl.localhost\<distribution>\…`)
/// names, if it names one there.
fn path_in_wsl_distribution(windows_path: &str, distribution: &str) -> Option<String> {
    let normalized = windows_path.replace('\\', "/");
    let unc = normalized
        .strip_prefix("//?/UNC/")
        .or_else(|| normalized.strip_prefix("//"))?;
    let (host, rest) = unc.split_once('/')?;
    if !host.eq_ignore_ascii_case("wsl$") && !host.eq_ignore_ascii_case("wsl.localhost") {
        return None;
    }
    let (name, inside) = rest.split_once('/').unwrap_or((rest, ""));
    // Windows compares share names without regard to case.
    if name.is_empty() || !name.eq_ignore_ascii_case(distribution) {
        return None;
    }
    Some(format!("/{}", inside.trim_end_matches('/')))
}

/// Stores what needs storing, one image after another so that two of the same
/// name do not race for it, and then writes a link per image, one per line,
/// over `targets`. With `separate_from_words`, a space is put between the
/// links and any word they would otherwise touch.
///
/// When the links cannot go in after all (the editor was closed while the
/// images were stored, or turned read-only), the images stored for them are
/// taken away again, so nothing is left in `images` without a link to it.
fn insert_image_links(
    editor: &mut Editor,
    project: Entity<Project>,
    document: ProjectPath,
    sources: Vec<LinkSource>,
    targets: Vec<Range<Anchor>>,
    separate_from_words: bool,
    window: &mut Window,
    cx: &mut Context<Editor>,
) {
    let editor_id = cx.entity_id();
    if !cx
        .default_global::<PendingImageInserts>()
        .0
        .insert(editor_id)
    {
        show_notice(
            editor,
            "前の画像を保存しています。終わってからもう一度お試しください。".to_string(),
            cx,
        );
        return;
    }
    let window_handle = window.window_handle();
    cx.spawn(async move |this, cx| {
        let mut links = Vec::new();
        let mut failures = Vec::new();
        // Images stored by this insert, which go again if their links do not
        // make it into the document.
        let mut stored_images = Vec::new();
        let mut unlinked_images = Vec::new();
        for source in sources {
            if this.upgrade().is_none() {
                break;
            }
            let stored = match source {
                LinkSource::Existing(project_path)
                    if project_path.worktree_id == document.worktree_id =>
                {
                    Ok((project_path, false))
                }
                // A link cannot leave the document's worktree, so an image
                // of another one is copied next to the document instead.
                LinkSource::Existing(project_path) => {
                    let source_name = project_path.path.as_unix_str().to_string();
                    cx.update(|cx| {
                        store_image_next_to(
                            &project,
                            &document,
                            ImageData::ProjectFile(project_path),
                            None,
                            cx,
                        )
                    })
                    .await
                    .with_context(|| source_name)
                    .map(|stored| (stored, true))
                }
                LinkSource::Store {
                    image,
                    preferred_name,
                } => cx
                    .update(|cx| {
                        store_image_next_to(&project, &document, image, preferred_name, cx)
                    })
                    .await
                    .map(|stored| (stored, true)),
            };
            match stored {
                Ok((image_path, is_stored)) => {
                    let destination =
                        cx.update(|cx| link_destination(&document, &image_path, true, cx));
                    match destination {
                        Some(destination) => {
                            links.push(format!("![{}]({destination})", alt_text(&image_path.path)));
                            if is_stored {
                                stored_images.push(image_path);
                            }
                        }
                        None => {
                            failures.push(format!(
                                "{}: ドキュメントからの相対パスを作れません",
                                image_path.path.as_unix_str()
                            ));
                            if is_stored {
                                unlinked_images.push(image_path);
                            }
                        }
                    }
                }
                Err(error) => failures.push(format!("{error:#}")),
            }
        }
        let inserted = cx.update_window(window_handle, |_, window, cx| {
            this.update(cx, |editor, cx| {
                if !failures.is_empty() {
                    show_notice(
                        editor,
                        format!("画像を保存できませんでした: {}", failures.join(" / ")),
                        cx,
                    );
                }
                if links.is_empty() {
                    return false;
                }
                if editor.read_only(cx) {
                    show_notice(
                        editor,
                        "バッファーが読み取り専用になったため、画像のリンクを挿入できませんでした。"
                            .to_string(),
                        cx,
                    );
                    return false;
                }
                let mut inserted = links.join("\n");
                if separate_from_words && let Some(target) = targets.first() {
                    let snapshot = editor.buffer().read(cx).snapshot(cx);
                    let touches_word =
                        |character: Option<char>| character.is_some_and(|c| !c.is_whitespace());
                    if touches_word(snapshot.reversed_chars_at(target.start).next()) {
                        inserted.insert(0, ' ');
                    }
                    if touches_word(snapshot.chars_at(target.end).next()) {
                        inserted.push(' ');
                    }
                }
                editor.change_selections(SelectionEffects::default(), window, cx, |selections| {
                    selections.select_anchor_ranges(targets)
                });
                editor.insert(&inserted, window, cx);
                true
            })
        });
        let inserted = matches!(inserted, Ok(Ok(true)));
        cx.update(|cx| {
            cx.default_global::<PendingImageInserts>()
                .0
                .remove(&editor_id)
        });
        if !inserted {
            unlinked_images.append(&mut stored_images);
        }
        for image in unlinked_images {
            cx.update(|cx| remove_stored_image(&project, &document, &image, cx))
                .await
                .with_context(|| {
                    format!("removing the unlinked image {}", image.path.as_unix_str())
                })
                .log_err();
        }
    })
    .detach();
}

/// Which kind of path the cursor is in, which decides what is offered first
/// and how the path is written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PathSlot {
    MarkdownLink,
    MarkdownImage,
    HtmlLink,
    HtmlImage,
}

impl PathSlot {
    fn in_markdown(self) -> bool {
        matches!(self, Self::MarkdownLink | Self::MarkdownImage)
    }

    fn prefers_images(self) -> bool {
        matches!(self, Self::MarkdownImage | Self::HtmlImage)
    }
}

/// A path being typed: where it starts and ends in its line, the end being
/// where a completion stops replacing.
#[derive(Debug, PartialEq, Eq)]
struct PathQuery {
    slot: PathSlot,
    start: usize,
    end: usize,
}

fn is_escaped(text: &str, index: usize) -> bool {
    text.get(..index).is_some_and(|before| {
        before
            .chars()
            .rev()
            .take_while(|character| *character == '\\')
            .count()
            % 2
            == 1
    })
}

/// Where the `[` starts that the `]` right after `text` closes.
fn opening_bracket(text: &str) -> Option<usize> {
    let mut depth = 0usize;
    for (index, character) in text.char_indices().rev() {
        if !matches!(character, '[' | ']') || is_escaped(text, index) {
            continue;
        }
        if character == ']' {
            depth += 1;
        } else if depth == 0 {
            return Some(index);
        } else {
            depth -= 1;
        }
    }
    None
}

fn markdown_destination_start(before: &str) -> Option<(PathSlot, usize)> {
    let close = before.rfind("](")?;
    if is_escaped(before, close) {
        return None;
    }
    let start = close + 2;
    let typed = before.get(start..)?;
    if typed.contains(|character: char| {
        character.is_whitespace() || matches!(character, '(' | ')' | '<' | '>')
    }) {
        return None;
    }
    let text = before.get(..close)?;
    let is_image = opening_bracket(text)
        .and_then(|open| text.get(..open))
        .is_some_and(|before_open| before_open.ends_with('!'));
    let slot = if is_image {
        PathSlot::MarkdownImage
    } else {
        PathSlot::MarkdownLink
    };
    Some((slot, start))
}

/// The attribute value the end of `before` is inside, when it is a quoted
/// `src` or `href` of the tag last opened on the line: its slot, where the
/// value starts and the quote that ends it.
fn html_attribute_start(before: &str) -> Option<(PathSlot, usize, char)> {
    let tag_start = before.rfind('<')?;
    let tag = before.get(tag_start + 1..)?;
    let name_end = tag.find(char::is_whitespace)?;
    let tag_name = tag.get(..name_end)?;
    if tag_name.is_empty()
        || !tag_name
            .chars()
            .all(|character| character.is_ascii_alphanumeric())
    {
        return None;
    }
    let characters: Vec<(usize, char)> = tag.char_indices().collect();
    let mut index = characters
        .iter()
        .position(|(offset, _)| *offset == name_end)?;
    let skip_whitespace = |index: &mut usize| {
        while characters
            .get(*index)
            .is_some_and(|(_, character)| character.is_whitespace())
        {
            *index += 1;
        }
    };
    loop {
        skip_whitespace(&mut index);
        let name_start = characters.get(index)?.0;
        while characters.get(index).is_some_and(|(_, character)| {
            !character.is_whitespace() && !matches!(character, '=' | '>' | '"' | '\'')
        }) {
            index += 1;
        }
        let (name_end, next) = *characters.get(index)?;
        if matches!(next, '>' | '"' | '\'') {
            return None;
        }
        let name = tag.get(name_start..name_end)?;
        skip_whitespace(&mut index);
        if characters.get(index)?.1 != '=' {
            continue;
        }
        index += 1;
        skip_whitespace(&mut index);
        let (quote_offset, quote) = *characters.get(index)?;
        if !matches!(quote, '"' | '\'') {
            // An unquoted value would end at the first space, which leaves
            // no room for a path with one.
            return None;
        }
        index += 1;
        match characters
            .get(index..)?
            .iter()
            .position(|(_, character)| *character == quote)
        {
            Some(length) => index += length + 1,
            None => {
                let slot = if name.eq_ignore_ascii_case("src") {
                    PathSlot::HtmlImage
                } else if name.eq_ignore_ascii_case("href") {
                    PathSlot::HtmlLink
                } else {
                    return None;
                };
                return Some((slot, tag_start + 1 + quote_offset + 1, quote));
            }
        }
    }
}

/// The path `cursor` (a byte offset into `line`) is typing, if any.
fn path_query(line: &str, cursor: usize) -> Option<PathQuery> {
    let before = line.get(..cursor)?;
    let after = line.get(cursor..)?;
    let markdown = markdown_destination_start(before);
    let html = html_attribute_start(before);
    // Whichever opened last is the one the cursor is in.
    let (slot, start, length_after) = match (markdown, html) {
        (Some((slot, start)), html) if html.is_none_or(|(_, html_start, _)| start > html_start) => {
            let length = after
                .find(|character: char| character.is_whitespace() || character == ')')
                .unwrap_or(after.len());
            (slot, start, length)
        }
        (_, Some((slot, start, quote))) => {
            let length = after.find(quote).unwrap_or(after.len());
            (slot, start, length)
        }
        _ => return None,
    };
    let typed = before.get(start..)?;
    // A URL, a Windows drive or an anchor in the document is not a project path.
    if typed.contains(':') || typed.starts_with('#') {
        return None;
    }
    Some(PathQuery {
        slot,
        start,
        end: cursor + length_after,
    })
}

/// The path from the folder whose components are `from` to `file`, both in
/// one worktree, and how many folders apart they are.
fn relative_display(from: &[&str], file: &RelPath) -> (String, usize) {
    let to: Vec<&str> = file.components().collect();
    let common = from
        .iter()
        .zip(&to)
        .take_while(|(left, right)| left == right)
        .count();
    let ups = from.len() - common;
    let mut parts = vec![".."; ups];
    parts.extend(to.iter().skip(common).copied());
    let downs = to.len().saturating_sub(common + 1);
    (parts.join("/"), ups + downs)
}

#[derive(Clone)]
struct PathCandidate {
    display: String,
    path: Arc<RelPath>,
    rank: String,
}

/// The paths offered for one document and slot, made from the files of one
/// version of its worktree.
struct CandidateList {
    candidates: Vec<PathCandidate>,
    match_candidates: Vec<StringMatchCandidate>,
}

impl CandidateList {
    fn new(candidates: Vec<PathCandidate>) -> Self {
        let match_candidates = candidates
            .iter()
            .enumerate()
            .map(|(id, candidate)| StringMatchCandidate::new(id, &candidate.display))
            .collect();
        Self {
            candidates,
            match_candidates,
        }
    }
}

/// What a [`CandidateList`] was made from. Any change to the worktree's files
/// starts a new scan, so the same scan with the same number of files lists the
/// same files.
#[derive(Clone, Debug, PartialEq, Eq)]
struct CandidateKey {
    worktree_id: WorktreeId,
    scan_id: usize,
    file_count: usize,
    document: Arc<RelPath>,
    slot: PathSlot,
}

impl CandidateKey {
    /// `None` while the worktree is still being scanned, when its files change
    /// without a new scan.
    fn for_worktree(worktree: &Worktree, document: &Arc<RelPath>, slot: PathSlot) -> Option<Self> {
        (worktree.completed_scan_id() >= worktree.scan_id()).then(|| Self {
            worktree_id: worktree.id(),
            scan_id: worktree.scan_id(),
            file_count: worktree.file_count(),
            document: document.clone(),
            slot,
        })
    }
}

/// The list made for the latest request, which the next keystrokes in the
/// same path reuse instead of walking every file of the worktree again.
#[derive(Default)]
struct CandidateCache {
    latest: Option<(CandidateKey, Arc<CandidateList>)>,
}

impl CandidateCache {
    fn get(&self, key: &CandidateKey) -> Option<Arc<CandidateList>> {
        self.latest
            .as_ref()
            .filter(|(latest_key, _)| latest_key == key)
            .map(|(_, list)| list.clone())
    }
}

#[cfg(test)]
thread_local! {
    static CANDIDATE_LISTS_BUILT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The files of the document's worktree as paths from the document, ranked
/// with the kind the slot prefers first and nearer files before farther ones.
fn path_candidates(
    document: &RelPath,
    files: impl Iterator<Item = Arc<RelPath>>,
    slot: PathSlot,
) -> Vec<PathCandidate> {
    let from: Vec<&str> = document
        .parent()
        .map(|parent| parent.components().collect())
        .unwrap_or_default();
    files
        .filter(|path| path.as_ref() != document)
        .map(|path| {
            let (display, distance) = relative_display(&from, &path);
            let preferred = if slot.prefers_images() {
                has_image_extension(path.as_unix_str())
            } else {
                has_extension(path.as_unix_str(), &MARKDOWN_EXTENSIONS)
            };
            let rank = format!("{}{distance:05}{display}", if preferred { 0 } else { 1 });
            PathCandidate {
                display,
                path,
                rank,
            }
        })
        .collect()
}

fn wrap_completion_provider(editor: &mut Editor) {
    if !editor.mode().is_full() {
        return;
    }
    let (Some(project), Some(inner)) = (editor.project().cloned(), editor.completion_provider())
    else {
        return;
    };
    editor.set_completion_provider(Some(Rc::new(PathCompletionProvider {
        inner,
        project,
        candidates: Rc::default(),
    })));
}

/// Adds the project's files to what the editor's own provider offers while the
/// cursor is in a Markdown link destination or an HTML `src` / `href`.
struct PathCompletionProvider {
    inner: Rc<dyn CompletionProvider>,
    project: Entity<Project>,
    candidates: Rc<RefCell<CandidateCache>>,
}

impl PathCompletionProvider {
    /// The path being typed at `position` and the document it is relative
    /// to, when `buffer` is a saved Markdown file of the project.
    fn query_at(
        &self,
        buffer: &Entity<Buffer>,
        position: language::Anchor,
        cx: &App,
    ) -> Option<(PathQuery, ProjectPath, Range<usize>)> {
        let buffer = buffer.read(cx);
        if !is_markdown(buffer) {
            return None;
        }
        // A fenced code block is injected with its own language, where a `](`
        // is code rather than a link.
        let language_here = buffer.language_at(position)?;
        if !is_markdown_layer(language_here.name().as_ref()) {
            return None;
        }
        let file = buffer.file()?;
        if file.path().is_empty() {
            return None;
        }
        let snapshot = buffer.snapshot();
        let offset = position.to_offset(&snapshot);
        let row = snapshot.offset_to_point(offset).row;
        let line_start = snapshot.point_to_offset(language::Point::new(row, 0));
        let line_end = snapshot.point_to_offset(language::Point::new(row, snapshot.line_len(row)));
        let line: String = snapshot.text_for_range(line_start..line_end).collect();
        let query = path_query(&line, offset - line_start)?;
        let range = line_start + query.start..line_start + query.end;
        Some((
            query,
            ProjectPath {
                worktree_id: file.worktree_id(cx),
                path: file.path().clone(),
            },
            range,
        ))
    }

    fn path_completions(
        &self,
        buffer: &Entity<Buffer>,
        position: language::Anchor,
        cx: &mut Context<Editor>,
    ) -> Option<Task<anyhow::Result<CompletionResponse>>> {
        let (query, document, range) = self.query_at(buffer, position, cx)?;
        let worktree = self
            .project
            .read(cx)
            .worktree_for_id(document.worktree_id, cx)?;
        let slot = query.slot;
        let key = CandidateKey::for_worktree(worktree.read(cx), &document.path, slot);
        let cached = key
            .as_ref()
            .and_then(|key| self.candidates.borrow().get(key));
        let list_task = match cached {
            Some(list) => Task::ready(list),
            None => {
                let files: Vec<Arc<RelPath>> = worktree
                    .read(cx)
                    .files(false, 0)
                    .map(|entry| entry.path.clone())
                    .collect();
                #[cfg(test)]
                CANDIDATE_LISTS_BUILT.with(|built| built.set(built.get() + 1));
                let document = document.path.clone();
                cx.background_spawn(async move {
                    Arc::new(CandidateList::new(path_candidates(
                        &document,
                        files.into_iter(),
                        slot,
                    )))
                })
            }
        };
        let snapshot = buffer.read(cx).snapshot();
        let cursor = position.to_offset(&snapshot);
        let typed: String = snapshot.text_for_range(range.start..cursor).collect();
        let replace_range = snapshot.anchor_before(range.start)..snapshot.anchor_after(range.end);
        let match_start = snapshot.anchor_before(range.start);
        let cache = self.candidates.clone();
        Some(cx.spawn(async move |_, cx| {
            let list = list_task.await;
            if let Some(key) = key {
                cache.borrow_mut().latest = Some((key, list.clone()));
            }
            let executor = cx.background_executor().clone();
            let (chosen, is_incomplete) = cx
                .background_spawn(async move {
                    let matches = fuzzy::match_strings(
                        &list.match_candidates,
                        &typed,
                        false,
                        true,
                        usize::MAX,
                        &AtomicBool::new(false),
                        executor,
                    )
                    .await;
                    let total = matches.len();
                    let mut chosen: Vec<(f64, &PathCandidate)> = matches
                        .iter()
                        .filter_map(|found| {
                            list.candidates
                                .get(found.candidate_id)
                                .map(|candidate| (found.score, candidate))
                        })
                        .collect();
                    chosen.sort_by(|(left_score, left), (right_score, right)| {
                        right_score
                            .total_cmp(left_score)
                            .then_with(|| left.rank.cmp(&right.rank))
                    });
                    chosen.truncate(MAXIMUM_PATH_COMPLETIONS);
                    let chosen: Vec<PathCandidate> = chosen
                        .into_iter()
                        .map(|(_, candidate)| candidate.clone())
                        .collect();
                    (chosen, total > MAXIMUM_PATH_COMPLETIONS)
                })
                .await;
            let completions = cx.update(|cx| {
                chosen
                    .into_iter()
                    .filter_map(|candidate| {
                        let image = ProjectPath {
                            worktree_id: document.worktree_id,
                            path: candidate.path,
                        };
                        let new_text = link_destination(&document, &image, slot.in_markdown(), cx)?;
                        Some(Completion {
                            replace_range: replace_range.clone(),
                            new_text,
                            label: CodeLabel::plain(candidate.display, None),
                            documentation: None,
                            // The one source besides a language server's whose
                            // sort text the menu honours, which is how the
                            // preferred kind stays on top of an empty query.
                            source: CompletionSource::Dap {
                                sort_text: candidate.rank,
                            },
                            icon_path: None,
                            icon_color: None,
                            match_start: Some(match_start),
                            snippet_deduplication_key: None,
                            insert_text_mode: None,
                            confirm: None,
                            group: None,
                        })
                    })
                    .collect()
            });
            Ok(CompletionResponse {
                completions,
                display_options: CompletionDisplayOptions::default(),
                is_incomplete,
            })
        }))
    }
}

impl CompletionProvider for PathCompletionProvider {
    fn completions(
        &self,
        buffer: &Entity<Buffer>,
        buffer_position: language::Anchor,
        trigger: editor::CompletionContext,
        window: &mut Window,
        cx: &mut Context<Editor>,
    ) -> Task<anyhow::Result<Vec<CompletionResponse>>> {
        let inner = self
            .inner
            .completions(buffer, buffer_position, trigger, window, cx);
        let Some(paths) = self.path_completions(buffer, buffer_position, cx) else {
            return inner;
        };
        cx.background_spawn(async move {
            let mut responses = match inner.await {
                Ok(responses) => responses,
                Err(error) => {
                    // The paths are still worth offering when the language
                    // server fails.
                    log::error!("completions failed next to the path completions: {error:#}");
                    Vec::new()
                }
            };
            responses.push(paths.await?);
            Ok(responses)
        })
    }

    fn resolve_completions(
        &self,
        buffer: Entity<Buffer>,
        completion_indices: Vec<usize>,
        completions: Rc<RefCell<Box<[Completion]>>>,
        cx: &mut Context<Editor>,
    ) -> Task<anyhow::Result<bool>> {
        self.inner
            .resolve_completions(buffer, completion_indices, completions, cx)
    }

    fn apply_additional_edits_for_completion(
        &self,
        buffer: Entity<Buffer>,
        completions: Rc<RefCell<Box<[Completion]>>>,
        completion_index: usize,
        push_to_history: bool,
        all_commit_ranges: Vec<Range<language::Anchor>>,
        cx: &mut Context<Editor>,
    ) -> Task<anyhow::Result<Option<language::Transaction>>> {
        self.inner.apply_additional_edits_for_completion(
            buffer,
            completions,
            completion_index,
            push_to_history,
            all_commit_ranges,
            cx,
        )
    }

    fn is_completion_trigger(
        &self,
        buffer: &Entity<Buffer>,
        position: language::Anchor,
        text: &str,
        trigger_in_words: bool,
        cx: &mut Context<Editor>,
    ) -> bool {
        self.inner
            .is_completion_trigger(buffer, position, text, trigger_in_words, cx)
            || (!text.is_empty() && self.query_at(buffer, position, cx).is_some())
    }

    fn selection_changed(
        &self,
        string_match: Option<&fuzzy::StringMatch>,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.inner.selection_changed(string_match, window, cx);
    }

    fn sort_completions(&self) -> bool {
        self.inner.sort_completions()
    }

    fn filter_completions(&self) -> bool {
        self.inner.filter_completions()
    }

    fn show_snippets(&self) -> bool {
        self.inner.show_snippets()
    }
}

#[cfg(test)]
mod tests;
