//! ADR-F68: in C and C++, the hover popover also shows what the hovered name's type holds
//! (members, enumerators, a macro's definition), found by name in the symbol index.
//!
//! The section is its own popover, added after the language server's: it keeps the language
//! server's popover where it was when it arrives later. Members expand in place, one level at a
//! time, for as long as the popover is open.

use std::{ops::Range, sync::Arc, time::Duration};

use collections::HashMap;
use futures::{
    FutureExt as _,
    future::{Either, LocalBoxFuture},
};
use gpui::{
    AnyElement, App, AsyncApp, AsyncWindowContext, ClipboardItem, Context, ElementId, Entity,
    FontWeight, Pixels, ScrollHandle, SharedString, Size, Subscription, Task, TaskExt as _,
    WeakEntity, Window, div, px,
};
use language::{
    Bias, Buffer, BufferSnapshot, CharKind, LanguageName, LanguageRegistry, Point, ToOffset as _,
};
use markdown::{CopyButtonVisibility, Markdown, MarkdownElement};
use project::{
    HoverBlockKind, Project, ProjectPath, Worktree, WorktreeId, WorktreeSettings,
    project_settings::ProjectSettings,
    symbol_index_store::{
        Confidence, MemberExpansion, MemberExpansionTarget, MemberSelector, MemberSummary,
        MemberSummaryKind, ResolvedTypes, SCRATCH_MAX_FILE_BYTES, SCRATCH_MAX_FILES,
        SCRATCH_MAX_INCLUDE_DEPTH, SourceLanguage, SymbolIndexStatus, SymbolIndexStore,
        SymbolIndexUnavailable, TagKind, TypeHover, TypeHoverSubject, TypeRef, TypeSummary,
        TypeSummaryKind, TypeTreeOptions, TypeTreeSubject, type_name_of_text,
    },
};
use settings::Settings as _;
use theme_settings::ThemeSettings;
use ui::{CommonAnimationExt as _, TextSize, Tooltip, prelude::*};
use util::{ResultExt as _, paths::PathStyle, rel_path::RelPath};

use crate::{
    Anchor, DisplayPoint, Editor, HighlightKey, SelectionEffects,
    hover_popover::{
        HOVER_POPOVER_GAP, HoverState, InfoPopover, hover_markdown_style, parse_blocks,
    },
    scroll::Autoscroll,
};

/// A section that answers within this of the language server is shown with it rather than
/// added below it afterwards.
const GRACE_PERIOD: Duration = Duration::from_millis(50);
/// Rows of one type shown before "N more".
const INITIAL_ROWS: usize = 30;
/// Rows each "N more" adds. The rows are plain elements laid out on every frame of the editor,
/// so a type with a thousand members is not shown all at once.
const MORE_ROWS: usize = 100;
const COMMENT_CHARS: usize = 60;
const NAME_COLUMN_CHARS: usize = 24;
const TYPE_COLUMN_CHARS: usize = 40;

#[cfg(test)]
thread_local! {
    static QUERIES_FOR_TESTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn queries_for_tests() -> usize {
    QUERIES_FOR_TESTS.with(|queries| queries.get())
}

/// A question to the index about the name under the pointer, asked alongside the language
/// server's hover.
pub(crate) struct TypeContentsQuery {
    describe: Task<anyhow::Result<Option<TypeHover>>>,
    store: WeakEntity<SymbolIndexStore>,
    project: WeakEntity<Project>,
    path: ProjectPath,
    source_language: SourceLanguage,
    language_name: LanguageName,
    snapshot: BufferSnapshot,
    position: Point,
    gap: Option<IndexGap>,
}

/// What the popover adds below the language server's.
pub(crate) enum Section {
    Contents(TypeContentsData),
    /// Nothing was found because the index cannot answer for this file.
    IndexGap {
        gap: IndexGap,
        range: Range<Point>,
        snapshot: BufferSnapshot,
    },
}

/// A section being looked up, and when the language server's popover stops waiting for it.
pub(crate) struct PendingTypeContents {
    future: LocalBoxFuture<'static, Option<Section>>,
    grace_period: Task<()>,
}

pub(crate) struct TypeContentsData {
    hover: TypeHover,
    /// The type as the language server wrote it, when the index's own guess was not good
    /// enough.
    type_from_language_server: bool,
    language_server_answered: bool,
    lookup: Option<Lookup>,
    store: WeakEntity<SymbolIndexStore>,
    project: WeakEntity<Project>,
    language_name: LanguageName,
    snapshot: BufferSnapshot,
}

/// Starts looking up the name at `position` when the buffer is C or C++, the file is in the
/// project and the setting allows it.
pub(crate) fn query(
    editor: &Editor,
    buffer: &Entity<Buffer>,
    position: text::Anchor,
    cx: &mut App,
) -> Option<TypeContentsQuery> {
    let project = editor.project()?.clone();
    let buffer = buffer.read(cx);
    let language = buffer.language()?;
    let source_language = match language.name().as_ref() {
        "C" => SourceLanguage::C,
        "C++" => SourceLanguage::Cpp,
        _ => return None,
    };
    let file = buffer.file()?;
    let path = ProjectPath {
        worktree_id: file.worktree_id(cx),
        path: file.path().clone(),
    };
    if !ProjectSettings::get(Some((&path).into()), cx)
        .symbol_index
        .hover_type_contents
    {
        return None;
    }
    let language_name = language.name();
    let snapshot = buffer.snapshot();
    let offset = position.to_offset(&snapshot);
    let position = snapshot.offset_to_point(offset);
    let gap = index_gap(project.read(cx), &path, cx);
    let store = project.read(cx).symbol_index_store().clone();
    #[cfg(test)]
    QUERIES_FOR_TESTS.with(|queries| queries.set(queries.get() + 1));
    let describe = store.update(cx, |store, cx| {
        store.describe_type_at(
            path.clone(),
            position,
            Some(snapshot.clone()),
            Some(source_language),
            1,
            cx,
        )
    });
    Some(TypeContentsQuery {
        describe,
        store: store.downgrade(),
        project: project.downgrade(),
        path,
        source_language,
        language_name,
        snapshot,
        position,
        gap,
    })
}

/// Why the index cannot describe the names of a file. Told in the popover: without it, the
/// section is just missing, which reads as the feature being broken.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum IndexGap {
    /// A single file opened on its own, or a file outside the project's folders.
    NotInAFolder,
    /// Git ignores the file and `symbol_index.include_ignored` is off.
    IgnoredFile,
    NotReady,
    Unavailable(SymbolIndexUnavailable),
}

fn index_gap(project: &Project, path: &ProjectPath, cx: &App) -> Option<IndexGap> {
    let worktree = project.worktree_for_id(path.worktree_id, cx)?;
    let worktree = worktree.read(cx);
    let entry = worktree.entry_for_path(&path.path)?;
    // Private and excluded files are left out on purpose; the popover does not dwell on them.
    // Neither does it on a file reached through a link: the index made for the hover would
    // hold another file under the link's name, which the settings may hide under its own.
    if entry.is_private || entry.is_external || !is_reached_by_its_own_name(worktree, &path.path) {
        return None;
    }
    if worktree.is_single_file() || !worktree.is_visible() {
        return Some(IndexGap::NotInAFolder);
    }
    let settings = &ProjectSettings::get(Some(path.into()), cx).symbol_index;
    if entry.is_ignored && !settings.include_ignored {
        return Some(IndexGap::IgnoredFile);
    }
    match project
        .symbol_index_store()
        .read(cx)
        .status(path.worktree_id)
    {
        SymbolIndexStatus::Ready { .. } => None,
        SymbolIndexStatus::Scanning | SymbolIndexStatus::Indexing { .. } => {
            Some(IndexGap::NotReady)
        }
        // Turned off on purpose, or a language the index does not take.
        SymbolIndexStatus::Disabled => None,
        SymbolIndexStatus::Unavailable { reason } => Some(IndexGap::Unavailable(reason)),
    }
}

/// Whether no link lies on the way to `path`, so that the settings judge the file that is
/// read by the name it is read under.
fn is_reached_by_its_own_name(worktree: &Worktree, path: &RelPath) -> bool {
    path.ancestors().all(|ancestor| {
        worktree
            .entry_for_path(ancestor)
            .is_some_and(|entry| entry.canonical_path.is_none())
    })
}

impl IndexGap {
    fn message(&self) -> String {
        match self {
            IndexGap::NotInAFolder => i18n::t!(
                "No definition found in this file. Open the folder that holds it to search the whole folder."
            )
            .to_string(),
            IndexGap::IgnoredFile => i18n::t!(
                "No definition found in this file or the headers it includes. Git ignores this file, so the folder's symbol index leaves it out; turn on the `symbol_index.include_ignored` setting to include it."
            )
            .to_string(),
            IndexGap::NotReady => i18n::t!(
                "Type contents: the symbol index is still being built. Hover again in a moment."
            )
            .to_string(),
            IndexGap::Unavailable(SymbolIndexUnavailable::InUseByAnotherWindow) => i18n::t!(
                "Type contents are not shown: another window has this folder's symbol index open."
            )
            .to_string(),
            IndexGap::Unavailable(SymbolIndexUnavailable::UnsupportedHost) => i18n::t!(
                "Type contents are not shown: the remote server does not support the symbol index."
            )
            .to_string(),
            IndexGap::Unavailable(SymbolIndexUnavailable::Collaboration) => i18n::t!(
                "Type contents are not shown: the symbol index is not available in a shared project."
            )
            .to_string(),
            IndexGap::Unavailable(SymbolIndexUnavailable::Error(error)) => i18n::t!(
                "Type contents are not shown: the symbol index failed ({error}).",
                error = error
            ),
        }
    }
}

/// The file of `snapshot` and the headers it includes by a relative path (`#include "..."`,
/// not `<...>`), as far as the worktree can name them and the settings let them be read:
/// what a hover indexes on the spot for a file the worktree's index leaves out. Open buffers
/// are read with their unsaved edits.
async fn read_included_files(
    project: &WeakEntity<Project>,
    path: &ProjectPath,
    snapshot: &BufferSnapshot,
    cx: &mut AsyncApp,
) -> Vec<(Arc<RelPath>, Vec<u8>)> {
    let mut files = vec![(
        path.path.clone(),
        snapshot.text_with_line_endings().into_bytes(),
    )];
    // A single file's worktree cannot name the headers next to it.
    if path.path.is_empty() {
        return files;
    }
    let mut queue = std::collections::VecDeque::from([(0, path.path.clone())]);
    let mut seen = collections::HashSet::from_iter([path.path.clone()]);
    while let Some((depth, including)) = queue.pop_front() {
        if depth >= SCRATCH_MAX_INCLUDE_DEPTH {
            continue;
        }
        let Some(contents) = files
            .iter()
            .find(|(file, _)| *file == including)
            .map(|(_, contents)| String::from_utf8_lossy(contents).into_owned())
        else {
            continue;
        };
        for header in quoted_includes(&contents) {
            if files.len() >= SCRATCH_MAX_FILES {
                return files;
            }
            let Some(header) = resolve_include(&including, header) else {
                continue;
            };
            if !seen.insert(header.clone()) {
                continue;
            }
            let header_path = ProjectPath {
                worktree_id: path.worktree_id,
                path: header.clone(),
            };
            scan_path(project, &header_path, cx).await;
            let readable = project
                .read_with(cx, |project, cx| may_read_header(project, &header_path, cx))
                .unwrap_or(false);
            if !readable {
                continue;
            }
            let Some(text) = read_header(project, header_path, cx).await else {
                continue;
            };
            files.push((header.clone(), text.into_bytes()));
            queue.push_back((depth + 1, header));
        }
    }
    files
}

/// Has a local worktree scan the folders on the way to `path` it has not scanned yet: it leaves
/// the folders Git ignores alone until asked, and so knows nothing of the headers in them.
async fn scan_path(project: &WeakEntity<Project>, path: &ProjectPath, cx: &mut AsyncApp) {
    let refresh = project
        .update(cx, |project, cx| {
            let worktree = project.worktree_for_id(path.worktree_id, cx)?;
            if worktree.read(cx).entry_for_path(&path.path).is_some() {
                return None;
            }
            worktree.update(cx, |worktree, cx| {
                Some(
                    worktree
                        .as_local()?
                        .refresh_entry(path.path.clone(), None, cx),
                )
            })
        })
        .ok()
        .flatten();
    if let Some(refresh) = refresh {
        match refresh.await {
            Ok(_) => {}
            // A header that is not there is just not read.
            Err(_) => {}
        }
    }
}

/// The text of a header: an open buffer's with its unsaved edits, otherwise the file's. A local
/// file is read without opening a buffer for it, which would detect its language and parse it
/// on every hover, and outlive the hover in a remote project's host.
async fn read_header(
    project: &WeakEntity<Project>,
    path: ProjectPath,
    cx: &mut AsyncApp,
) -> Option<String> {
    enum Source {
        Buffer(Entity<Buffer>),
        Load(Task<Option<String>>),
        Open(Task<anyhow::Result<Entity<Buffer>>>),
    }
    let source = project
        .update(cx, |project, cx| {
            if let Some(buffer) = project.get_open_buffer(&path, cx) {
                return Some(Source::Buffer(buffer));
            }
            let worktree = project.worktree_for_id(path.worktree_id, cx)?;
            if worktree.read(cx).is_local() {
                let load = worktree.update(cx, |worktree, cx| worktree.load_file(&path.path, cx));
                Some(Source::Load(cx.background_spawn(async move {
                    load.await.log_err().map(|loaded| loaded.text.to_string())
                })))
            } else {
                Some(Source::Open(project.open_buffer(path, cx)))
            }
        })
        .ok()??;
    let buffer = match source {
        Source::Buffer(buffer) => buffer,
        Source::Load(load) => return load.await,
        Source::Open(open) => open.await.log_err()?,
    };
    Some(buffer.read_with(cx, |buffer, _| buffer.text_with_line_endings()))
}

/// The paths of the `#include "..."` lines of `text`.
fn quoted_includes(text: &str) -> impl Iterator<Item = &str> {
    text.lines().filter_map(|line| {
        let rest = line.trim_start().strip_prefix('#')?.trim_start();
        let rest = rest.strip_prefix("include")?.trim_start();
        let rest = rest.strip_prefix('"')?;
        let end = rest.find('"')?;
        rest.get(..end)
    })
}

/// `header` as included from `including`, both relative to the worktree, when it stays in it.
fn resolve_include(including: &RelPath, header: &str) -> Option<Arc<RelPath>> {
    if header.starts_with(['/', '\\']) || header.contains(':') {
        return None;
    }
    let mut components = including
        .parent()
        .map(|parent| parent.components().map(str::to_string).collect::<Vec<_>>())
        .unwrap_or_default();
    for component in header.split(['/', '\\']) {
        match component {
            "" | "." => {}
            ".." => {
                components.pop()?;
            }
            component => components.push(component.to_string()),
        }
    }
    Some(
        RelPath::from_unix_str(&components.join("/"))
            .ok()?
            .into_arc(),
    )
}

/// Whether a hover may read the header at `path`: a file of the worktree, not private, not
/// excluded, not outside it, and not too large. Git ignoring it does not matter.
fn may_read_header(project: &Project, path: &ProjectPath, cx: &App) -> bool {
    let Some(worktree) = project.worktree_for_id(path.worktree_id, cx) else {
        return false;
    };
    let worktree = worktree.read(cx);
    let Some(entry) = worktree.entry_for_path(&path.path) else {
        return false;
    };
    let settings = WorktreeSettings::get(Some(path.into()), cx);
    entry.is_file()
        && !entry.is_private
        && !entry.is_external
        && is_reached_by_its_own_name(worktree, &path.path)
        && entry.size <= SCRATCH_MAX_FILE_BYTES
        && !settings.is_path_excluded(&path.path)
        && !settings.is_path_private(&path.path)
}

/// Types of the C and C++ standard libraries and POSIX, defined in headers a hover does not
/// read (`#include <...>`): a note that their definition is missing would only be noise.
const STANDARD_TYPES: &[&str] = &[
    "DIR",
    "FILE",
    "bool",
    "char16_t",
    "char32_t",
    "char8_t",
    "clock_t",
    "div_t",
    "errno_t",
    "fpos_t",
    "gid_t",
    "jmp_buf",
    "ldiv_t",
    "lldiv_t",
    "max_align_t",
    "mbstate_t",
    "mode_t",
    "nullptr_t",
    "off_t",
    "pid_t",
    "pthread_attr_t",
    "pthread_cond_t",
    "pthread_mutex_t",
    "pthread_t",
    "ptrdiff_t",
    "sig_atomic_t",
    "size_t",
    "socklen_t",
    "ssize_t",
    "string",
    "time_t",
    "uid_t",
    "va_list",
    "wchar_t",
    "wint_t",
];

/// Whether a note is worth adding when the definition of the type `name` was not found.
fn is_name_worth_a_note(name: &str) -> bool {
    let is_identifier = name
        .starts_with(|character: char| character == '_' || character.is_alphabetic())
        && name
            .chars()
            .all(|character| character == '_' || character == ':' || character.is_alphanumeric());
    is_identifier
        && !STANDARD_TYPES.contains(&name)
        && !name.starts_with("std::")
        && !name.starts_with("__")
        && !is_integer_type_of_stdint(name)
}

/// `int32_t`, `uint_least8_t`, `int_fast64_t`, `intptr_t` and the like.
fn is_integer_type_of_stdint(name: &str) -> bool {
    let Some(rest) = name.strip_suffix("_t") else {
        return false;
    };
    let rest = rest.strip_prefix('u').unwrap_or(rest);
    let Some(rest) = rest.strip_prefix("int") else {
        return false;
    };
    let rest = rest
        .strip_prefix("_least")
        .or_else(|| rest.strip_prefix("_fast"))
        .unwrap_or(rest);
    matches!(rest, "ptr" | "max") || (!rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()))
}

/// The type whose contents the hover could have shown: the one the index or the language
/// server names, unless it is a builtin type.
fn expected_type_name(
    hover: Option<&TypeHover>,
    language_server_type: Option<String>,
    source_language: SourceLanguage,
) -> Option<String> {
    let from_index = hover.and_then(|hover| match &hover.subject {
        TypeHoverSubject::TypeName { name, .. } => Some(name.clone()),
        TypeHoverSubject::Variable { type_text, .. } => {
            type_name_of_text(type_text, source_language).map(|(name, _)| name)
        }
        TypeHoverSubject::Member { steps, .. } => steps.last()?.member.as_ref()?.core_name.clone(),
        TypeHoverSubject::Macro { .. } => None,
    });
    from_index.or(language_server_type)
}

impl TypeContentsQuery {
    /// Completes the index's answer with the type the language server's hover names, when the
    /// index could only guess the type by name or found nothing.
    pub(crate) fn finish(
        self,
        language_server_hovers: &[project::Hover],
        language_server_late: bool,
        cx: &AsyncApp,
    ) -> PendingTypeContents {
        let type_text = language_server_type_text(language_server_hovers);
        // A late answer is taken to say what this one would: a builtin type is left to it.
        let language_server_answered =
            language_server_late || language_server_hovers.iter().any(|hover| !hover.is_empty());
        let mut cx = cx.clone();
        // Counted from the language server's answer: the question was asked before the hover
        // delay ran out, so a period counted from then would be over before any answer.
        let grace_period = cx.background_executor().timer(GRACE_PERIOD);
        let Self {
            describe,
            store,
            project,
            path,
            source_language,
            language_name,
            snapshot,
            position,
            gap,
        } = self;
        let future = async move {
            let language_server_type = type_text
                .as_deref()
                .and_then(|text| type_name_of_text(text, source_language))
                .map(|(name, _)| name);
            let mut hover = describe.await.log_err().flatten();
            let mut type_from_language_server = false;
            if needs_language_server_type(hover.as_ref())
                && let Some(type_text) = type_text
                && let Some((core_name, tag)) = type_name_of_text(&type_text, source_language)
            {
                let resolve = store
                    .update(&mut cx, |store, cx| {
                        store.resolve_type(&core_name, tag, Some(path.clone()), 1, cx)
                    })
                    .ok()?;
                if let Some(types) = resolve.await.log_err()
                    && !types.types.is_empty()
                {
                    let (name, range) = match &hover {
                        Some(hover) => (hover.name.clone(), hover.range.clone()),
                        None => word_at(&snapshot, position)?,
                    };
                    hover = Some(TypeHover {
                        name,
                        range,
                        subject: TypeHoverSubject::Variable {
                            type_text,
                            core_name: Some(core_name),
                            confidence: Confidence::NameOnly,
                            declaration: None,
                        },
                        types,
                    });
                    type_from_language_server = true;
                }
            }
            // Where the index cannot answer, a variable's type without its contents gives way to
            // the note that tells why they are missing.
            let builtin_left_out = language_server_answered || gap.is_some();
            let worth_showing = |hover: &Option<TypeHover>| {
                hover
                    .as_ref()
                    .is_some_and(|hover| is_hover_worth_showing(hover, builtin_left_out))
            };
            if !worth_showing(&hover)
                && let Some(IndexGap::IgnoredFile | IndexGap::NotInAFolder) = gap
            {
                let files = read_included_files(&project, &path, &snapshot, &mut cx).await;
                let describe = store
                    .update(&mut cx, |store, cx| {
                        store.describe_type_in_files(
                            path.clone(),
                            position,
                            snapshot.clone(),
                            source_language,
                            files,
                            1,
                            cx,
                        )
                    })
                    .ok()?;
                hover = describe.await.log_err().flatten();
            }
            let hover = if gap.is_some() && !worth_showing(&hover) {
                // Only where the type's contents were to be expected: on any other name (a
                // function, a local of a builtin type, `size_t`), the note would be noise.
                let expected =
                    expected_type_name(hover.as_ref(), language_server_type, source_language);
                if !expected.is_some_and(|name| is_name_worth_a_note(&name)) {
                    return None;
                }
                None
            } else {
                hover
            };
            let Some(hover) = hover else {
                let gap = gap?;
                let (_, range) = word_at(&snapshot, position)?;
                return Some(Section::IndexGap {
                    gap,
                    range,
                    snapshot,
                });
            };
            let lookup = root_lookup(&hover.subject, source_language, &path);
            Some(Section::Contents(TypeContentsData {
                hover,
                type_from_language_server,
                language_server_answered,
                lookup,
                store,
                project,
                language_name,
                snapshot,
            }))
        }
        .boxed_local();
        PendingTypeContents {
            future,
            grace_period,
        }
    }
}

/// The section, when it is ready within [`GRACE_PERIOD`]; otherwise what is still to come.
pub(crate) async fn within_grace_period(
    pending: Option<PendingTypeContents>,
) -> (
    Option<Section>,
    Option<LocalBoxFuture<'static, Option<Section>>>,
) {
    let Some(PendingTypeContents {
        future,
        grace_period,
    }) = pending
    else {
        return (None, None);
    };
    match futures::future::select(future, Box::pin(grace_period)).await {
        Either::Left((data, _)) => (data, None),
        Either::Right(((), future)) => (None, Some(future)),
    }
}

/// How long a C or C++ hover waits for the language server before it shows the type's
/// contents without its answer, which is added above them when it comes: a busy clangd (still
/// indexing a large project) would otherwise hold the section back for as long as it takes.
const LANGUAGE_SERVER_WAIT: Duration = Duration::from_millis(400);

pub(crate) type LateHovers = LocalBoxFuture<'static, Vec<project::Hover>>;

/// The language server's answer, or, for a hover with a type contents section, nothing and the
/// answer still to come when the server does not answer within [`LANGUAGE_SERVER_WAIT`].
pub(crate) async fn language_server_hovers(
    request: Option<Task<Option<Vec<project::Hover>>>>,
    has_type_contents: bool,
    cx: &AsyncWindowContext,
) -> (Vec<project::Hover>, Option<LateHovers>) {
    let Some(request) = request else {
        return (Vec::new(), None);
    };
    let request = request.map(|hovers| hovers.unwrap_or_default());
    if !has_type_contents {
        return (request.await, None);
    }
    let wait = cx.background_executor().timer(LANGUAGE_SERVER_WAIT);
    match futures::future::select(request.boxed_local(), Box::pin(wait)).await {
        Either::Left((hovers, _)) => (hovers, None),
        Either::Right(((), request)) => (Vec::new(), Some(request)),
    }
}

/// Adds what is still to come once it is ready: the section below the popovers already shown,
/// a late language server's popovers above them. Runs in the hover's task, so a hover that
/// moves on or closes drops it.
pub(crate) async fn append_later(
    later: Option<LocalBoxFuture<'static, Option<Section>>>,
    late_hovers: Option<LateHovers>,
    language_registry: Option<Arc<LanguageRegistry>>,
    editor: &WeakEntity<Editor>,
    anchor: Anchor,
    keyboard_grace: bool,
    cx: &mut AsyncWindowContext,
) {
    let mut hovers_cx = cx.clone();
    futures::join!(
        append_section(later, editor, anchor, keyboard_grace, cx),
        insert_late_hovers(
            late_hovers,
            language_registry,
            editor,
            anchor,
            keyboard_grace,
            &mut hovers_cx,
        ),
    );
}

async fn insert_late_hovers(
    late_hovers: Option<LateHovers>,
    language_registry: Option<Arc<LanguageRegistry>>,
    editor: &WeakEntity<Editor>,
    anchor: Anchor,
    keyboard_grace: bool,
    cx: &mut AsyncWindowContext,
) {
    let Some(late_hovers) = late_hovers else {
        return;
    };
    let hovers = late_hovers.await;
    if hovers.is_empty() {
        return;
    }
    let parsed = hovers
        .into_iter()
        .map(|hover| {
            let content = parse_blocks(
                &hover.contents,
                language_registry.as_ref(),
                hover.language,
                cx,
            );
            (hover.range, content)
        })
        .collect::<Vec<_>>();
    editor
        .update_in(cx, |editor, window, cx| {
            let snapshot = editor.snapshot(window, cx);
            let buffer_snapshot = snapshot.buffer_snapshot();
            let mut highlights = Vec::new();
            let mut popovers = Vec::new();
            for (range, content) in parsed {
                // As the hover places the popovers of an answer in time.
                let range = range
                    .and_then(|range| buffer_snapshot.buffer_anchor_range_to_anchor_range(range))
                    .or_else(|| {
                        let range = buffer_snapshot.syntax_ancestor(anchor..anchor)?.1;
                        Some(
                            buffer_snapshot.anchor_before(range.start)
                                ..buffer_snapshot.anchor_after(range.end),
                        )
                    })
                    .unwrap_or(anchor..anchor);
                highlights.push(range.clone());
                if let Some(content) = content {
                    popovers.push(InfoPopover::for_note(
                        range,
                        content,
                        anchor,
                        keyboard_grace,
                        cx,
                    ));
                }
            }
            editor.hover_state.info_popovers.splice(0..0, popovers);
            editor.highlight_background(
                HighlightKey::HoverState,
                &highlights,
                |_, theme| theme.colors().element_hover,
                cx,
            );
            cx.notify();
            window.refresh();
        })
        .ok();
}

async fn append_section(
    later: Option<LocalBoxFuture<'static, Option<Section>>>,
    editor: &WeakEntity<Editor>,
    anchor: Anchor,
    keyboard_grace: bool,
    cx: &mut AsyncWindowContext,
) {
    let Some(later) = later else {
        return;
    };
    let Some(data) = later.await else {
        return;
    };
    editor
        .update_in(cx, |editor, window, cx| {
            if let Some(popover) = popover(data, editor, anchor, keyboard_grace, window, cx) {
                editor.hover_state.info_popovers.push(popover);
                cx.notify();
                window.refresh();
            }
        })
        .ok();
}

/// The popover of the section, or `None` when the index had nothing worth adding.
pub(crate) fn popover(
    section: Section,
    editor: &Editor,
    anchor: Anchor,
    keyboard_grace: bool,
    _window: &mut Window,
    cx: &mut Context<Editor>,
) -> Option<InfoPopover> {
    let data = match section {
        Section::Contents(data) => data,
        Section::IndexGap {
            gap,
            range,
            snapshot,
        } => {
            let symbol_range = symbol_range(editor, &snapshot, range, anchor, cx);
            let markdown = cx.new(|cx| Markdown::new(gap.message().into(), None, None, cx));
            return Some(InfoPopover::for_note(
                symbol_range,
                markdown,
                anchor,
                keyboard_grace,
                cx,
            ));
        }
    };
    if !is_worth_showing(&data) {
        return None;
    }
    let symbol_range = symbol_range(editor, &data.snapshot, data.hover.range.clone(), anchor, cx);
    let editor_handle = cx.entity().downgrade();
    let contents = cx.new(|cx| TypeContents::new(data, editor_handle, cx));
    Some(InfoPopover::for_type_contents(
        symbol_range,
        contents,
        anchor,
        keyboard_grace,
        cx,
    ))
}

fn symbol_range(
    editor: &Editor,
    snapshot: &BufferSnapshot,
    range: Range<Point>,
    anchor: Anchor,
    cx: &App,
) -> Range<Anchor> {
    let start = snapshot.clip_point(range.start, Bias::Left);
    let end = snapshot.clip_point(range.end, Bias::Left);
    let buffer_range = snapshot.anchor_after(start)..snapshot.anchor_before(end.max(start));
    editor
        .buffer()
        .read(cx)
        .snapshot(cx)
        .buffer_anchor_range_to_anchor_range(buffer_range)
        .unwrap_or(anchor..anchor)
}

/// Where a stack with a type contents section was drawn, relative to the hovered point.
#[derive(Clone, Copy, Debug)]
pub(crate) struct KeptStackLayout {
    position: DisplayPoint,
    /// The stack's top left corner.
    pub(crate) offset: gpui::Point<Pixels>,
    /// Above the line, the popover nearest the line is the lowest.
    pub(crate) above: bool,
}

/// Where a stack can end: bounds ending exactly at the limit are not contained in it.
fn lowest_bottom(bottom_limit: Pixels) -> Pixels {
    bottom_limit - px(1.)
}

/// How far the kept stack, `stack_height` tall, would reach past `bottom_limit` with its top
/// where it was.
pub(crate) fn kept_stack_overflow(
    kept: Option<KeptStackLayout>,
    position: DisplayPoint,
    hovered_point: gpui::Point<Pixels>,
    stack_height: Pixels,
    bottom_limit: Pixels,
) -> Option<Pixels> {
    let kept = kept.filter(|kept| kept.position == position)?;
    let overflow = hovered_point.y + kept.offset.y + stack_height - lowest_bottom(bottom_limit);
    (overflow > Pixels::ZERO).then_some(overflow)
}

/// Where to draw a stack with a type contents section, `None` leaving it to upstream's
/// placement. Members are expanded while the stack is read, so it goes below the line where it
/// fits, growing away from the line. Once shown, its top and left edges stay where they were
/// (moved only to stay on screen): growing or shrinking neither moves the row being read nor
/// carries the stack out from under the pointer, which would hide it.
pub(crate) fn place_stack(
    kept: Option<KeptStackLayout>,
    position: DisplayPoint,
    hovered_point: gpui::Point<Pixels>,
    line_height: Pixels,
    stack_size: Size<Pixels>,
    bottom_limit: Pixels,
    right_limit: Pixels,
    fits: impl Fn(gpui::Bounds<Pixels>) -> bool,
) -> Option<KeptStackLayout> {
    let max_left = right_limit - stack_size.width;
    let layout = |origin: gpui::Point<Pixels>, above: bool| {
        fits(gpui::Bounds::new(origin, stack_size)).then(|| KeptStackLayout {
            position,
            offset: origin - hovered_point,
            above,
        })
    };
    if let Some(kept) = kept.filter(|kept| kept.position == position) {
        let origin = gpui::point(
            (hovered_point.x + kept.offset.x).min(max_left),
            (hovered_point.y + kept.offset.y).min(lowest_bottom(bottom_limit) - stack_size.height),
        );
        if let Some(layout) = layout(origin, kept.above) {
            return Some(layout);
        }
    }
    let left = hovered_point.x.min(max_left);
    layout(gpui::point(left, hovered_point.y + line_height), false)
        .or_else(|| layout(gpui::point(left, hovered_point.y - stack_size.height), true))
}

/// Whether `point` is on one of the shown popovers or in a gap between two stacked ones. Only
/// the popovers stop the pointer from reaching the editor; in a gap, it would otherwise count
/// as leaving them, and start hiding them or hover the text underneath.
pub(crate) fn is_in_popover_stack(hover_state: &HoverState, point: gpui::Point<Pixels>) -> bool {
    let mut bounds = hover_state
        .info_popovers
        .iter()
        .filter_map(|popover| popover.last_bounds.get())
        .chain(
            hover_state
                .diagnostic_popover
                .as_ref()
                .and_then(|popover| popover.last_bounds.get()),
        )
        .collect::<Vec<_>>();
    if bounds.iter().any(|bounds| bounds.contains(&point)) {
        return true;
    }
    bounds.sort_by(|left, right| {
        left.top()
            .partial_cmp(&right.top())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    bounds
        .iter()
        .zip(bounds.iter().skip(1))
        .any(|(upper, lower)| {
            let gap = lower.top() - upper.bottom();
            // Stacked popovers are a gap (and their borders, outside the bounds) apart; those on
            // both sides of the line are further.
            if gap < px(0.) || gap > HOVER_POPOVER_GAP + px(4.) {
                return false;
            }
            let left = upper.left().min(lower.left());
            let right = upper.right().max(lower.right());
            point.y >= upper.bottom()
                && point.y <= lower.top()
                && point.x >= left
                && point.x <= right
        })
}

fn is_worth_showing(data: &TypeContentsData) -> bool {
    is_hover_worth_showing(&data.hover, data.language_server_answered)
}

fn is_hover_worth_showing(hover: &TypeHover, language_server_answered: bool) -> bool {
    let has_types = !hover.types.types.is_empty();
    match &hover.subject {
        TypeHoverSubject::Macro { macros } => !macros.is_empty(),
        TypeHoverSubject::Member { steps, .. } => !steps.is_empty(),
        TypeHoverSubject::TypeName { .. } => has_types,
        // A builtin type adds nothing to what the language server already says.
        TypeHoverSubject::Variable { .. } => has_types || !language_server_answered,
    }
}

fn needs_language_server_type(hover: Option<&TypeHover>) -> bool {
    match hover {
        None => true,
        Some(hover) => match &hover.subject {
            TypeHoverSubject::Variable {
                confidence: Confidence::NameOnly,
                ..
            } => true,
            TypeHoverSubject::Variable { .. } | TypeHoverSubject::TypeName { .. } => {
                hover.types.types.is_empty()
            }
            TypeHoverSubject::Member { .. } | TypeHoverSubject::Macro { .. } => false,
        },
    }
}

fn word_at(snapshot: &BufferSnapshot, position: Point) -> Option<(String, Range<Point>)> {
    let offset = snapshot.point_to_offset(position);
    let (range, kind) = snapshot.surrounding_word(offset, None);
    if kind != Some(CharKind::Word) || range.is_empty() {
        return None;
    }
    let name = snapshot.text_for_range(range.clone()).collect::<String>();
    Some((
        name,
        snapshot.offset_to_point(range.start)..snapshot.offset_to_point(range.end),
    ))
}

/// The type a language server's hover names: clangd writes `Type: `...`` for variables and
/// members, and a declaration in its first code block. Only a starting point for the index;
/// the text is checked by [`type_name_of_text`].
fn language_server_type_text(hovers: &[project::Hover]) -> Option<String> {
    let blocks = hovers.iter().flat_map(|hover| hover.contents.iter());
    for block in blocks.clone() {
        if let Some(type_text) = block.text.lines().find_map(type_line) {
            return Some(type_text);
        }
    }
    for block in blocks {
        let code = match &block.kind {
            HoverBlockKind::Code { .. } => Some(block.text.as_str()),
            HoverBlockKind::Markdown | HoverBlockKind::PlainText => first_fenced_code(&block.text),
        };
        if let Some(declaration) = code.and_then(declaration_line) {
            return Some(declaration);
        }
    }
    None
}

fn type_line(line: &str) -> Option<String> {
    let rest = line.trim().strip_prefix("Type:")?.trim();
    let rest = match rest.split_once('`') {
        Some((_, quoted)) => quoted.split('`').next().unwrap_or_default(),
        None => rest.split(" (aka").next().unwrap_or_default(),
    };
    let rest = rest.trim();
    (!rest.is_empty()).then(|| rest.to_string())
}

fn first_fenced_code(markdown: &str) -> Option<&str> {
    let start = markdown.find("```")?;
    let after_fence = &markdown[start..];
    let body_start = after_fence.find('\n')? + 1;
    let body = &after_fence[body_start..];
    let end = body.find("```").unwrap_or(body.len());
    Some(&body[..end])
}

fn declaration_line(code: &str) -> Option<String> {
    let line = code
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with("//"))?;
    let line = line.trim_end_matches(['{', ';', ' ']);
    (!line.is_empty()).then(|| line.to_string())
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Lookup {
    name: String,
    tag: Option<TagKind>,
    near: Option<ProjectPath>,
}

fn root_lookup(
    subject: &TypeHoverSubject,
    source_language: SourceLanguage,
    near: &ProjectPath,
) -> Option<Lookup> {
    let (name, tag) = match subject {
        TypeHoverSubject::TypeName { name, tag } => (name.clone(), *tag),
        TypeHoverSubject::Variable {
            type_text,
            core_name,
            ..
        } => type_name_of_text(type_text, source_language)
            .or_else(|| core_name.clone().map(|name| (name, None)))?,
        TypeHoverSubject::Member { steps, .. } => {
            let member = steps.last()?.member.as_ref()?;
            (member.core_name.clone()?, member.tag)
        }
        TypeHoverSubject::Macro { .. } => return None,
    };
    Some(Lookup {
        name,
        tag,
        near: Some(near.clone()),
    })
}

fn member_lookup(member: &MemberSummary, owner: &TypeSummary) -> Option<Lookup> {
    Some(Lookup {
        name: member.core_name.clone()?,
        tag: member.tag,
        near: Some(owner.path.clone()),
    })
}

/// A member within the tree: at each level, the candidate shown and the member's index.
type MemberPath = Vec<(usize, usize)>;

struct Candidate {
    summary: TypeSummary,
    members: Vec<MemberState>,
    shown_rows: usize,
    load_failed: bool,
}

impl Candidate {
    fn new(summary: TypeSummary) -> Self {
        let members = summary
            .members
            .iter()
            .map(|member| match &member.inline {
                Some(body) => MemberState {
                    open: true,
                    expansion: Expansion::Ready(Box::new(CandidatesView::single((**body).clone()))),
                },
                None => MemberState {
                    open: false,
                    expansion: Expansion::None,
                },
            })
            .collect();
        Self {
            summary,
            members,
            shown_rows: INITIAL_ROWS,
            load_failed: false,
        }
    }
}

struct MemberState {
    open: bool,
    expansion: Expansion,
}

enum Expansion {
    None,
    Loading(#[allow(dead_code)] Task<()>),
    Ready(Box<CandidatesView>),
    NotFound,
    NotExpandable,
    Recursive,
}

/// Same-name definitions of one type, one of them shown.
struct CandidatesView {
    /// A body defined in place, shown under its member without a heading of its own.
    inline: bool,
    lookup: Option<Lookup>,
    candidates: Vec<Candidate>,
    total: usize,
    truncated: bool,
    selected: usize,
    loading: Option<(usize, Task<()>)>,
}

impl CandidatesView {
    fn new(types: ResolvedTypes, lookup: Option<Lookup>) -> Self {
        Self {
            inline: false,
            lookup,
            total: types.total.max(types.types.len()),
            truncated: types.truncated,
            candidates: types.types.into_iter().map(Candidate::new).collect(),
            selected: 0,
            loading: None,
        }
    }

    fn single(summary: TypeSummary) -> Self {
        Self {
            inline: true,
            lookup: None,
            candidates: vec![Candidate::new(summary)],
            total: 1,
            truncated: false,
            selected: 0,
            loading: None,
        }
    }

    fn selected(&self) -> Option<(usize, &Candidate)> {
        let index = self.selected.min(self.candidates.len().checked_sub(1)?);
        Some((index, self.candidates.get(index)?))
    }
}

struct MacroView {
    macros: Vec<project::symbol_index_store::MacroSummary>,
    markdown: Vec<Entity<Markdown>>,
    selected: usize,
}

enum Heading {
    None,
    Variable {
        name: String,
        type_text: String,
        confidence: Option<Confidence>,
    },
    Member {
        root_type: Option<String>,
        steps: Vec<project::symbol_index_store::ChainStepSummary>,
        complete: bool,
    },
}

pub(crate) struct TypeContents {
    editor: WeakEntity<Editor>,
    project: WeakEntity<Project>,
    store: WeakEntity<SymbolIndexStore>,
    heading: Heading,
    root: Option<CandidatesView>,
    macros: Option<MacroView>,
    path_labels: PathLabels,
    copy_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

/// How locations are written: with the project's separator, and with the worktree's name in
/// front when the project has several (a relative path alone could be in any of them).
struct PathLabels {
    path_style: PathStyle,
    root_names: Option<HashMap<WorktreeId, String>>,
    /// The names of single files opened on their own, whose paths are empty.
    single_files: HashMap<WorktreeId, String>,
}

impl PathLabels {
    fn new(project: Option<&Project>, cx: &App) -> Self {
        let Some(project) = project else {
            return Self {
                path_style: PathStyle::local(),
                root_names: None,
                single_files: HashMap::default(),
            };
        };
        let worktrees = project.visible_worktrees(cx).collect::<Vec<_>>();
        let root_names = (worktrees.len() > 1).then(|| {
            worktrees
                .iter()
                .map(|worktree| {
                    let worktree = worktree.read(cx);
                    (worktree.id(), worktree.root_name_str().to_string())
                })
                .collect()
        });
        let single_files = project
            .worktrees(cx)
            .filter_map(|worktree| {
                let worktree = worktree.read(cx);
                worktree
                    .is_single_file()
                    .then(|| (worktree.id(), worktree.root_name_str().to_string()))
            })
            .collect();
        Self {
            path_style: project.path_style(cx),
            root_names,
            single_files,
        }
    }
}

impl TypeContents {
    fn new(data: TypeContentsData, editor: WeakEntity<Editor>, cx: &mut Context<Self>) -> Self {
        let TypeContentsData {
            hover,
            type_from_language_server,
            lookup,
            store,
            project,
            language_name,
            language_server_answered: _,
            snapshot: _,
        } = data;
        let mut subscriptions = Vec::new();
        let mut macros = None;
        let mut root = None;
        let heading = match hover.subject {
            TypeHoverSubject::TypeName { .. } => Heading::None,
            TypeHoverSubject::Variable {
                type_text,
                confidence,
                ..
            } => Heading::Variable {
                name: hover.name.clone(),
                type_text,
                confidence: (!type_from_language_server).then_some(confidence),
            },
            TypeHoverSubject::Member {
                root_type,
                steps,
                complete,
                ..
            } => {
                if hover.types.types.is_empty()
                    && let Some(body) = steps
                        .last()
                        .and_then(|step| step.member.as_ref())
                        .and_then(|member| member.inline.as_ref())
                {
                    root = Some(CandidatesView::single((**body).clone()));
                }
                Heading::Member {
                    root_type,
                    steps,
                    complete,
                }
            }
            TypeHoverSubject::Macro { macros: found } => {
                let language_registry = project
                    .upgrade()
                    .map(|project| project.read(cx).languages().clone());
                let markdown = found
                    .iter()
                    .map(|found| {
                        let markdown = cx.new(|cx| {
                            Markdown::new(
                                code_block(&found.text, &language_name).into(),
                                language_registry.clone(),
                                Some(language_name.clone()),
                                cx,
                            )
                        });
                        subscriptions.push(cx.observe(&markdown, |_, _, cx| cx.notify()));
                        markdown
                    })
                    .collect();
                macros = Some(MacroView {
                    macros: found,
                    markdown,
                    selected: 0,
                });
                Heading::None
            }
        };
        if root.is_none() && !hover.types.types.is_empty() {
            root = Some(CandidatesView::new(hover.types, lookup));
        }
        let path_labels = PathLabels::new(
            project.upgrade().as_ref().map(|project| project.read(cx)),
            cx,
        );
        Self {
            editor,
            project,
            store,
            heading,
            root,
            macros,
            path_labels,
            copy_task: None,
            _subscriptions: subscriptions,
        }
    }

    fn view_mut(&mut self, path: &[(usize, usize)]) -> Option<&mut CandidatesView> {
        let mut view = self.root.as_mut()?;
        for &(candidate, member) in path {
            let member = view
                .candidates
                .get_mut(candidate)?
                .members
                .get_mut(member)?;
            match &mut member.expansion {
                Expansion::Ready(child) => view = child,
                _ => return None,
            }
        }
        Some(view)
    }

    fn view(&self, path: &[(usize, usize)]) -> Option<&CandidatesView> {
        let mut view = self.root.as_ref()?;
        for &(candidate, member) in path {
            let member = view.candidates.get(candidate)?.members.get(member)?;
            match &member.expansion {
                Expansion::Ready(child) => view = child,
                _ => return None,
            }
        }
        Some(view)
    }

    /// The definitions a member at `path` is inside, outermost first.
    fn ancestor_refs(&self, path: &[(usize, usize)]) -> Vec<TypeRef> {
        let mut refs = Vec::new();
        for depth in 0..path.len() {
            let Some(view) = self.view(&path[..depth]) else {
                break;
            };
            let Some((candidate, _)) = path.get(depth) else {
                break;
            };
            if let Some(type_ref) = view
                .candidates
                .get(*candidate)
                .and_then(|candidate| candidate.summary.type_ref.clone())
            {
                refs.push(type_ref);
            }
        }
        refs
    }

    fn toggle_member(&mut self, path: MemberPath, cx: &mut Context<Self>) {
        let Some(((candidate_index, member_index), parent)) = path.split_last() else {
            return;
        };
        let (candidate_index, member_index) = (*candidate_index, *member_index);
        let ancestors = self.ancestor_refs(&path);
        let store = self.store.clone();
        let Some(view) = self.view_mut(parent) else {
            return;
        };
        let Some(candidate) = view.candidates.get_mut(candidate_index) else {
            return;
        };
        let Some(member) = candidate.summary.members.get(member_index).cloned() else {
            return;
        };
        let lookup = member_lookup(&member, &candidate.summary);
        let owner = candidate.summary.type_ref.clone();
        let Some(state) = candidate.members.get_mut(member_index) else {
            return;
        };
        match &state.expansion {
            Expansion::Loading(_) | Expansion::NotExpandable | Expansion::Recursive => {}
            Expansion::Ready(_) | Expansion::NotFound => state.open = !state.open,
            Expansion::None => {
                let Some(owner) = owner else {
                    state.expansion = Expansion::NotExpandable;
                    cx.notify();
                    return;
                };
                let selector = match (&member.name, member.index_path.is_empty()) {
                    (_, false) => MemberSelector::Path(member.index_path.clone()),
                    (Some(name), true) => MemberSelector::Name(name.clone()),
                    (None, true) => {
                        state.expansion = Expansion::NotExpandable;
                        cx.notify();
                        return;
                    }
                };
                let Ok(expand) =
                    store.update(cx, |store, cx| store.expand_member(owner, selector, 1, cx))
                else {
                    return;
                };
                let task_path = path.clone();
                let task = cx.spawn(async move |this, cx| {
                    let expansion = expand.await;
                    this.update(cx, |this, cx| {
                        let expansion = match expansion {
                            Ok(expansion) => expansion_from(expansion, &ancestors, lookup),
                            Err(error) => {
                                log::error!("expanding a member for the hover: {error:#}");
                                Expansion::NotFound
                            }
                        };
                        if let Some(state) = this.member_state_mut(&task_path) {
                            state.expansion = expansion;
                            cx.notify();
                        }
                    })
                    .ok();
                });
                state.open = true;
                state.expansion = Expansion::Loading(task);
            }
        }
        cx.notify();
    }

    fn member_state_mut(&mut self, path: &[(usize, usize)]) -> Option<&mut MemberState> {
        let ((candidate, member), parent) = path.split_last()?;
        self.view_mut(parent)?
            .candidates
            .get_mut(*candidate)?
            .members
            .get_mut(*member)
    }

    fn select_candidate(&mut self, path: MemberPath, forward: bool, cx: &mut Context<Self>) {
        let store = self.store.clone();
        let Some(view) = self.view_mut(&path) else {
            return;
        };
        let count = view.candidates.len();
        if count < 2 {
            return;
        }
        let current = view.selected.min(count - 1);
        let selected = if forward {
            (current + 1) % count
        } else {
            (current + count - 1) % count
        };
        // What was opened under a definition is about that definition: coming back to it starts
        // closed again, and the questions still pending for it stop.
        if let Some(previous) = view.candidates.get_mut(current)
            && previous.summary.members_read
        {
            *previous = Candidate::new(previous.summary.clone());
        }
        view.selected = selected;
        cx.notify();
        let Some(candidate) = view.candidates.get_mut(selected) else {
            return;
        };
        if candidate.summary.members_read || candidate.load_failed {
            return;
        }
        let Some(lookup) = view.lookup.clone() else {
            candidate.load_failed = true;
            return;
        };
        let wanted = candidate.summary.clone();
        // The index reads the best-ranked definitions only, and ranks those in the file asked
        // from first: asking from the wanted definition's own file reads it even when it is
        // ranked past the read limit from where the hover was.
        let near = Some(wanted.path.clone());
        let Ok(resolve) = store.update(cx, |store, cx| {
            store.resolve_type(
                &lookup.name,
                lookup.tag,
                near,
                project::symbol_index_store::MAX_TYPE_READ_LIMIT,
                cx,
            )
        }) else {
            return;
        };
        let task = cx.spawn(async move |this, cx| {
            let resolved = resolve.await.log_err();
            this.update(cx, |this, cx| {
                let Some(view) = this.view_mut(&path) else {
                    return;
                };
                if view.loading.as_ref().map(|(index, _)| *index) == Some(selected) {
                    view.loading = None;
                }
                let found = resolved.and_then(|resolved| read_definition(resolved, &wanted));
                if let Some(candidate) = view.candidates.get_mut(selected) {
                    match found {
                        Some(summary) => *candidate = Candidate::new(summary),
                        None => candidate.load_failed = true,
                    }
                }
                cx.notify();
            })
            .ok();
        });
        view.loading = Some((selected, task));
    }

    fn show_more_rows(&mut self, path: MemberPath, candidate: usize, cx: &mut Context<Self>) {
        if let Some(candidate) = self
            .view_mut(&path)
            .and_then(|view| view.candidates.get_mut(candidate))
        {
            candidate.shown_rows = candidate.shown_rows.saturating_add(MORE_ROWS);
            cx.notify();
        }
    }

    fn select_macro(&mut self, forward: bool, cx: &mut Context<Self>) {
        if let Some(macros) = &mut self.macros {
            let count = macros.macros.len();
            if count > 1 {
                let current = macros.selected.min(count - 1);
                macros.selected = if forward {
                    (current + 1) % count
                } else {
                    (current + count - 1) % count
                };
                cx.notify();
            }
        }
    }

    /// Puts the shown type's members, two levels of named types down, on the clipboard.
    fn copy_type_contents(&mut self, cx: &mut Context<Self>) {
        let Some(root) = &self.root else {
            return;
        };
        let Some((_, candidate)) = root.selected() else {
            return;
        };
        let subject = match (&candidate.summary.type_ref, &root.lookup) {
            (Some(type_ref), _) => TypeTreeSubject::Ref(type_ref.clone()),
            (None, Some(lookup)) => TypeTreeSubject::Name {
                name: lookup.name.clone(),
                tag: lookup.tag,
                near: lookup.near.clone(),
            },
            (None, None) => return,
        };
        let Ok(tree) = self.store.update(cx, |store, cx| {
            store.type_tree_text(subject, TypeTreeOptions::default(), cx)
        }) else {
            return;
        };
        self.copy_task = Some(cx.spawn(async move |this, cx| {
            let tree = tree.await.log_err().flatten();
            this.update(cx, |this, cx| {
                this.copy_task = None;
                if let Some(tree) = tree {
                    cx.write_to_clipboard(ClipboardItem::new_string(tree.text));
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn lines(&self) -> Vec<Line> {
        let mut lines = Vec::new();
        match &self.heading {
            Heading::None => {}
            Heading::Variable {
                name,
                type_text,
                confidence,
            } => lines.push(Line::Variable {
                name: name.clone(),
                type_text: type_text.clone(),
                confidence: confidence_label(*confidence),
            }),
            Heading::Member {
                root_type,
                steps,
                complete,
            } => lines.push(Line::Chain(chain_segments(
                root_type.as_deref(),
                steps,
                *complete,
            ))),
        }
        if let Some(macros) = &self.macros
            && let Some(index) = macros.macros.len().checked_sub(1)
        {
            let selected = macros.selected.min(index);
            if let Some(found) = macros.macros.get(selected) {
                lines.push(Line::Macro {
                    name: found.name.clone(),
                    location: location_label(&found.path, found.range.start, &self.path_labels),
                    jump: (found.path.clone(), found.range.start),
                    selected,
                    total: macros.macros.len(),
                    truncated: found.truncated,
                });
            }
        }
        if let Some(root) = &self.root {
            let mut ancestors = Vec::new();
            self.push_candidates(root, &mut Vec::new(), 0, &mut ancestors, &mut lines);
        }
        lines
    }

    fn push_candidates<'a>(
        &self,
        view: &'a CandidatesView,
        path: &mut MemberPath,
        depth: usize,
        ancestors: &mut Vec<&'a TypeSummary>,
        lines: &mut Vec<Line>,
    ) {
        let Some((selected, candidate)) = view.selected() else {
            lines.push(Line::Note {
                depth,
                text: i18n::t!("Definition not found").into(),
            });
            return;
        };
        let summary = &candidate.summary;
        if !view.inline || depth == 0 {
            lines.push(Line::TypeHeading {
                depth,
                view_path: path.clone(),
                name: summary.name.clone(),
                location: location_label(&summary.path, summary.range.start, &self.path_labels),
                jump: (summary.path.clone(), summary.range.start),
                selected,
                listed: view.candidates.len(),
                total: view.total,
                details: type_details(summary),
                condition: summary.condition.clone(),
                copying: depth == 0 && self.copy_task.is_some(),
            });
        }
        if view.truncated {
            lines.push(Line::Note {
                depth,
                text: i18n::t!("Only part is shown").into(),
            });
        }
        if view.loading.as_ref().map(|(index, _)| *index) == Some(selected) {
            lines.push(Line::Loading { depth });
            return;
        }
        if !summary.members_read {
            if summary.kind != TypeSummaryKind::Typedef || summary.alias.is_none() {
                lines.push(Line::Note {
                    depth,
                    text: i18n::t!("Only the location of this definition is known").into(),
                });
            }
            return;
        }
        let row_count = summary.members.len() + summary.enumerators.len();
        let mut shown = 0;
        ancestors.push(summary);
        for (index, member) in summary.members.iter().enumerate() {
            if shown >= candidate.shown_rows {
                break;
            }
            shown += 1;
            let Some(state) = candidate.members.get(index) else {
                break;
            };
            let mut member_path = path.clone();
            member_path.push((selected, index));
            let can_expand =
                member.inline.is_some() || (member.expandable && summary.type_ref.is_some());
            let toggle = if !can_expand {
                Toggle::None
            } else if matches!(state.expansion, Expansion::Recursive)
                || (member.inline.is_none() && refers_to_ancestor(member, ancestors))
            {
                Toggle::Recursive
            } else {
                match &state.expansion {
                    Expansion::Loading(_) => Toggle::Loading,
                    Expansion::NotExpandable => Toggle::None,
                    _ if state.open => Toggle::Expanded,
                    _ => Toggle::Collapsed,
                }
            };
            lines.push(member_line(
                member,
                summary,
                depth,
                member_path.clone(),
                toggle,
            ));
            if state.open && toggle != Toggle::Recursive {
                match &state.expansion {
                    Expansion::Ready(child) => {
                        path.push((selected, index));
                        self.push_candidates(child, path, depth + 1, ancestors, lines);
                        path.pop();
                    }
                    Expansion::NotFound => lines.push(Line::Note {
                        depth: depth + 1,
                        text: i18n::t!("Definition not found").into(),
                    }),
                    Expansion::None
                    | Expansion::Loading(_)
                    | Expansion::NotExpandable
                    | Expansion::Recursive => {}
                }
            }
        }
        ancestors.pop();
        for enumerator in &summary.enumerators {
            if shown >= candidate.shown_rows {
                break;
            }
            shown += 1;
            let value = if enumerator.value_is_inferred {
                enumerator
                    .value
                    .map(|value| (value.to_string(), true))
                    .or_else(|| enumerator.value_text.clone().map(|text| (text, false)))
            } else {
                enumerator
                    .value_text
                    .clone()
                    .map(|text| (text, false))
                    .or_else(|| enumerator.value.map(|value| (value.to_string(), false)))
            };
            lines.push(Line::Enumerator {
                depth,
                name: enumerator.name.clone(),
                value,
                comment: enumerator.comment.as_deref().map(first_line_clipped),
                condition: enumerator.condition.clone(),
                jump: (summary.path.clone(), enumerator.range.start),
            });
        }
        if shown < row_count {
            lines.push(Line::More {
                depth,
                view_path: path.clone(),
                candidate: selected,
                hidden: row_count - shown,
            });
        }
        if summary.truncated {
            lines.push(Line::Note {
                depth,
                text: i18n::t!("Only part is shown").into(),
            });
        }
    }
}

fn expansion_from(
    expansion: MemberExpansion,
    ancestors: &[TypeRef],
    lookup: Option<Lookup>,
) -> Expansion {
    match expansion.target {
        MemberExpansionTarget::NotFound => Expansion::NotFound,
        MemberExpansionTarget::NotExpandable => Expansion::NotExpandable,
        MemberExpansionTarget::Inline(body) => {
            Expansion::Ready(Box::new(CandidatesView::single(*body)))
        }
        MemberExpansionTarget::Types(types) => {
            let first = types.types.first();
            if first.is_none() {
                Expansion::NotFound
            } else if first
                .and_then(|summary| summary.type_ref.as_ref())
                .is_some_and(|type_ref| ancestors.contains(type_ref))
            {
                Expansion::Recursive
            } else {
                Expansion::Ready(Box::new(CandidatesView::new(types, lookup)))
            }
        }
    }
}

/// The read one of `resolved` that is `wanted`, a definition listed by its location alone.
fn read_definition(resolved: ResolvedTypes, wanted: &TypeSummary) -> Option<TypeSummary> {
    let mut read = resolved
        .types
        .into_iter()
        .filter(|summary| summary.members_read)
        .collect::<Vec<_>>();
    if let Some(position) = read
        .iter()
        .position(|summary| same_definition(summary, wanted))
    {
        return Some(read.swap_remove(position));
    }
    // A typedef that defines the type it names in place (`typedef struct a { .. } a_t;`) is
    // read as that type, with no typedef step to tell it by. Asked from its file, the one read
    // there is it; with several there, which one is not known.
    if wanted.kind != TypeSummaryKind::Typedef {
        return None;
    }
    let mut in_file = read
        .into_iter()
        .filter(|summary| summary.path == wanted.path);
    let first = in_file.next()?;
    in_file.next().is_none().then_some(first)
}

fn same_definition(found: &TypeSummary, wanted: &TypeSummary) -> bool {
    // A definition listed by its location alone is the one the index has under the name: for
    // a typedef, the typedef, where a read one is what it names.
    let through_typedef = found
        .typedef_chain
        .first()
        .is_some_and(|step| step.path == wanted.path && step.range == wanted.range);
    through_typedef
        || match (&found.type_ref, &wanted.type_ref) {
            (Some(found), Some(wanted)) => found == wanted,
            _ => found.path == wanted.path && found.range == wanted.range,
        }
}

/// Whether a member names one of the types it is inside (`struct node *next` in
/// `struct node`), which would expand forever. `ancestors` ends with the member's own type.
///
/// Only types defined in the member's own file are compared by name: in another file, the
/// same name is often a different type of a large project (each module's `struct node`). Those
/// are told apart by [`TypeRef`] once the member is expanded.
fn refers_to_ancestor(member: &MemberSummary, ancestors: &[&TypeSummary]) -> bool {
    let Some(core_name) = member.core_name.as_deref() else {
        return false;
    };
    let Some(owner) = ancestors.last() else {
        return false;
    };
    ancestors.iter().any(|ancestor| {
        if ancestor.path != owner.path {
            return false;
        }
        let tag_matches = match member.tag {
            None => true,
            Some(TagKind::Struct) => ancestor.kind == TypeSummaryKind::Struct,
            Some(TagKind::Union) => ancestor.kind == TypeSummaryKind::Union,
            Some(TagKind::Enum) => ancestor.kind == TypeSummaryKind::Enum,
            Some(TagKind::Class) => {
                matches!(
                    ancestor.kind,
                    TypeSummaryKind::Class | TypeSummaryKind::Struct
                )
            }
        };
        if !tag_matches {
            return false;
        }
        let tag_name = ancestor
            .name
            .split_once(' ')
            .map_or(ancestor.name.as_str(), |(_, name)| name);
        tag_name == core_name
            || ancestor
                .type_ref
                .as_ref()
                .is_some_and(|type_ref| type_ref.name == core_name)
            || (member.tag.is_none()
                && ancestor
                    .typedef_chain
                    .iter()
                    .any(|step| step.name == core_name))
    })
}

fn type_details(summary: &TypeSummary) -> Option<String> {
    let mut details = Vec::new();
    if !summary.typedef_chain.is_empty() {
        let mut chain = summary
            .typedef_chain
            .iter()
            .map(|step| step.name.as_str())
            .collect::<Vec<_>>();
        chain.push(&summary.name);
        details.push(chain.join(" → "));
    }
    if let Some(alias) = &summary.alias {
        details.push(format!("= {alias}"));
    }
    if !summary.base_classes.is_empty() {
        details.push(format!(": {}", summary.base_classes.join(", ")));
    }
    if summary.is_forward_declaration {
        details.push(i18n::t!("forward declaration").to_string());
    }
    (!details.is_empty()).then(|| details.join("  "))
}

fn member_line(
    member: &MemberSummary,
    owner: &TypeSummary,
    depth: usize,
    path: MemberPath,
    toggle: Toggle,
) -> Line {
    let unparsed = member.kind == MemberSummaryKind::Unparsed;
    let name = match (&member.name, member.kind) {
        (Some(name), _) => name.clone(),
        (None, MemberSummaryKind::AnonymousAggregate) => member
            .inline
            .as_ref()
            .map(|body| body.name.clone())
            .unwrap_or_else(|| i18n::t!("(anonymous)").to_string()),
        (None, _) => String::new(),
    };
    let type_text = if unparsed {
        member
            .unparsed_text
            .clone()
            .unwrap_or_else(|| member.text.clone())
    } else if member.is_function_pointer || member.kind == MemberSummaryKind::Method {
        member
            .signature
            .clone()
            .unwrap_or_else(|| member.type_text.clone())
    } else {
        member.type_text.clone()
    };
    Line::Member {
        depth,
        path,
        name,
        type_text,
        bit_width: member.bit_width.clone(),
        comment: member.comment.as_deref().map(first_line_clipped),
        condition: member.condition.clone(),
        toggle,
        unparsed,
        jump: (owner.path.clone(), member.range.start),
    }
}

fn first_line_clipped(comment: &str) -> String {
    let line = comment.lines().map(str::trim).find(|line| !line.is_empty());
    let line = line.unwrap_or_default();
    match line.char_indices().nth(COMMENT_CHARS) {
        Some((end, _)) => format!("{}…", &line[..end]),
        None => line.to_string(),
    }
}

fn confidence_label(confidence: Option<Confidence>) -> SharedString {
    match confidence {
        None => i18n::t!("type named by the language server").into(),
        Some(Confidence::Declaration) => i18n::t!("at its declaration").into(),
        Some(Confidence::Local) => i18n::t!("declared in this function").into(),
        Some(Confidence::Parameter) => i18n::t!("a parameter of this function").into(),
        Some(Confidence::FileScope) => i18n::t!("declared at file scope").into(),
        Some(Confidence::NameOnly) => i18n::t!("guessed by name only").into(),
    }
}

struct ChainSegment {
    text: String,
    type_text: Option<String>,
    found: bool,
}

fn chain_segments(
    root_type: Option<&str>,
    steps: &[project::symbol_index_store::ChainStepSummary],
    complete: bool,
) -> Vec<ChainSegment> {
    let mut segments = Vec::new();
    // The type found for the start of the chain says more than how it was written
    // (`struct outer` rather than `alias_t *`).
    let root = steps
        .first()
        .and_then(|step| step.owner.as_ref())
        .map(|owner| owner.display_name.clone())
        .or_else(|| root_type.map(str::to_string));
    if let Some(root) = root {
        segments.push(ChainSegment {
            text: root,
            type_text: None,
            found: true,
        });
    }
    for step in steps {
        let found = step.member.is_some();
        segments.push(ChainSegment {
            text: format!(".{}", step.name),
            type_text: step.member.as_ref().map(|member| {
                if member.is_function_pointer {
                    member
                        .signature
                        .clone()
                        .unwrap_or_else(|| member.type_text.clone())
                } else {
                    member.type_text.clone()
                }
            }),
            found,
        });
        if !found {
            break;
        }
    }
    if !complete && segments.last().is_some_and(|segment| segment.found) {
        segments.push(ChainSegment {
            text: "…".to_string(),
            type_text: None,
            found: false,
        });
    }
    segments
}

fn location_label(path: &ProjectPath, position: Point, labels: &PathLabels) -> (String, String) {
    if path.path.is_empty()
        && let Some(name) = labels.single_files.get(&path.worktree_id)
    {
        return (String::new(), format!("{name}:{}", position.row + 1));
    }
    let relative = path.path.display(labels.path_style);
    let shown = match labels
        .root_names
        .as_ref()
        .and_then(|root_names| root_names.get(&path.worktree_id))
    {
        Some(root_name) => format!(
            "{root_name}{}{relative}",
            labels.path_style.primary_separator()
        ),
        None => relative.into_owned(),
    };
    let (folder, file) = labels.path_style.split(&shown);
    (
        folder.unwrap_or_default().to_string(),
        format!("{file}:{}", position.row + 1),
    )
}

fn code_block(text: &str, language_name: &LanguageName) -> String {
    let mut longest_run = 0;
    let mut run = 0;
    for character in text.chars() {
        if character == '`' {
            run += 1;
            longest_run = longest_run.max(run);
        } else {
            run = 0;
        }
    }
    let fence = "`".repeat((longest_run + 1).max(3));
    let language = language_name.as_ref().to_lowercase().replace("++", "pp");
    format!("{fence}{language}\n{text}\n{fence}")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Toggle {
    None,
    Collapsed,
    Expanded,
    Loading,
    Recursive,
}

type Jump = (ProjectPath, Point);

enum Line {
    Variable {
        name: String,
        type_text: String,
        confidence: SharedString,
    },
    Chain(Vec<ChainSegment>),
    Macro {
        name: String,
        location: (String, String),
        jump: Jump,
        selected: usize,
        total: usize,
        truncated: bool,
    },
    TypeHeading {
        depth: usize,
        view_path: MemberPath,
        name: String,
        location: (String, String),
        jump: Jump,
        selected: usize,
        listed: usize,
        total: usize,
        details: Option<String>,
        condition: Option<String>,
        copying: bool,
    },
    Member {
        depth: usize,
        path: MemberPath,
        name: String,
        type_text: String,
        bit_width: Option<String>,
        comment: Option<String>,
        condition: Option<String>,
        toggle: Toggle,
        unparsed: bool,
        jump: Jump,
    },
    Enumerator {
        depth: usize,
        name: String,
        /// The value, and whether it was counted rather than written.
        value: Option<(String, bool)>,
        comment: Option<String>,
        condition: Option<String>,
        jump: Jump,
    },
    More {
        depth: usize,
        view_path: MemberPath,
        candidate: usize,
        hidden: usize,
    },
    Note {
        depth: usize,
        text: SharedString,
    },
    Loading {
        depth: usize,
    },
}

fn definition_position_label(selected: usize, total: usize) -> String {
    let index = selected + 1;
    i18n::t!("Definition {index} of {total}")
}

/// The heading's narrow form of [`definition_position_label`], which is its tooltip.
fn short_position_label(selected: usize, total: usize) -> String {
    format!("{} / {}", selected + 1, total)
}

/// Where the shown definition is among the others, kept on one line: the heading wraps
/// between its parts, not inside this one.
fn position_element(id: ElementId, selected: usize, total: usize) -> impl IntoElement {
    div()
        .id(id)
        .flex_none()
        .tooltip(Tooltip::text(definition_position_label(selected, total)))
        .child(
            Label::new(short_position_label(selected, total))
                .size(LabelSize::XSmall)
                .color(Color::Muted)
                .single_line(),
        )
}

fn variable_label(name: &str) -> String {
    i18n::t!("Type of {name}:")
}

fn more_label(hidden: usize) -> String {
    i18n::tn!(hidden, "{count} more", "{count} more")
}

fn open_location(
    editor: &WeakEntity<Editor>,
    project: &WeakEntity<Project>,
    (path, position): Jump,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(workspace) = editor
        .upgrade()
        .and_then(|editor| editor.read(cx).workspace())
    else {
        return;
    };
    let Some(project) = project.upgrade() else {
        return;
    };
    let open = project.update(cx, |project, cx| project.open_buffer(path, cx));
    window
        .spawn(cx, async move |cx| {
            let buffer = open.await?;
            workspace.update_in(cx, |workspace, window, cx| {
                let position = buffer.read(cx).clip_point(position, Bias::Left);
                let editor = workspace
                    .open_project_item::<Editor>(None, buffer, true, true, true, true, window, cx);
                editor.update(cx, |editor, cx| {
                    editor.change_selections(
                        SelectionEffects::scroll(Autoscroll::center()),
                        window,
                        cx,
                        |selections| selections.select_ranges([position..position]),
                    );
                });
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
}

/// The section's rows, scrollable within the popover's height.
pub(crate) fn render(
    contents: &Entity<TypeContents>,
    scroll_handle: &ScrollHandle,
    max_size: Size<Pixels>,
    window: &mut Window,
    cx: &mut Context<Editor>,
) -> AnyElement {
    let (lines, macro_markdown, editor, project) = {
        let this = contents.read(cx);
        let markdown = this.macros.as_ref().and_then(|macros| {
            let index = macros.selected.min(macros.markdown.len().checked_sub(1)?);
            macros.markdown.get(index).cloned()
        });
        (
            this.lines(),
            markdown,
            this.editor.clone(),
            this.project.clone(),
        )
    };
    let contents = contents.downgrade();
    let text_size = TextSize::Small.rems(cx).to_pixels(window.rem_size());
    let buffer_font = ThemeSettings::get_global(cx).buffer_font.clone();
    let font_id = window.text_system().resolve_font(&buffer_font);
    let em_width = window
        .text_system()
        .em_advance(font_id, text_size)
        .unwrap_or(text_size * 0.6_f32);
    let (name_columns, type_columns) = column_widths(&lines);
    let name_width = em_width * name_columns as f32 + px(4.);
    let type_width = em_width * type_columns as f32 + px(4.);
    let indent = |depth: usize| px(14.) * depth as f32;
    let muted = Color::Muted;
    let hover_background = cx.theme().colors().element_hover;

    let jump_handler = |jump: Jump| {
        let editor = editor.clone();
        let project = project.clone();
        move |_: &gpui::ClickEvent, window: &mut Window, cx: &mut App| {
            open_location(&editor, &project, jump.clone(), window, cx);
        }
    };

    let mut rows: Vec<AnyElement> = Vec::new();
    for (row_index, line) in lines.into_iter().enumerate() {
        let row_id =
            |kind: &str| ElementId::Name(format!("type-contents-{kind}-{row_index}").into());
        let row = match line {
            Line::Variable {
                name,
                type_text,
                confidence,
            } => h_flex()
                .gap_1()
                .flex_wrap()
                .child(Label::new(variable_label(&name)).size(LabelSize::Small))
                .child(Label::new(type_text).size(LabelSize::Small).buffer_font(cx))
                .child(Label::new(confidence).size(LabelSize::XSmall).color(muted))
                .into_any_element(),
            Line::Chain(segments) => {
                let mut row = h_flex().gap_1().flex_wrap();
                for (index, segment) in segments.into_iter().enumerate() {
                    if index > 0 {
                        row = row.child(Label::new("→").size(LabelSize::Small).color(muted));
                    }
                    row = row.child(
                        Label::new(segment.text)
                            .size(LabelSize::Small)
                            .buffer_font(cx)
                            .color(if segment.found { Color::Default } else { muted }),
                    );
                    if let Some(type_text) = segment.type_text {
                        row = row.child(
                            Label::new(format!("({type_text})"))
                                .size(LabelSize::Small)
                                .buffer_font(cx)
                                .color(muted),
                        );
                    }
                    if !segment.found {
                        row = row.child(
                            Label::new(i18n::t!("could not be followed"))
                                .size(LabelSize::XSmall)
                                .color(muted),
                        );
                    }
                }
                row.into_any_element()
            }
            Line::Macro {
                name,
                location,
                jump,
                selected,
                total,
                truncated,
            } => {
                let contents = contents.clone();
                let contents_back = contents.clone();
                h_flex()
                    .gap_1()
                    .child(heading_name(name, cx))
                    .child(name_match_badge(cx))
                    .child(location_element(
                        row_id("macro-location"),
                        location,
                        jump_handler(jump),
                        cx,
                    ))
                    .when(total > 1, |row| {
                        row.child(candidate_switcher(
                            row_id("macro-switch"),
                            selected,
                            total,
                            move |cx| {
                                contents_back
                                    .update(cx, |this, cx| this.select_macro(false, cx))
                                    .ok();
                            },
                            move |cx| {
                                contents
                                    .update(cx, |this, cx| this.select_macro(true, cx))
                                    .ok();
                            },
                        ))
                    })
                    .when(truncated, |row| {
                        row.child(
                            Label::new(i18n::t!("Only part is shown"))
                                .size(LabelSize::XSmall)
                                .color(muted),
                        )
                    })
                    .into_any_element()
            }
            Line::TypeHeading {
                depth,
                view_path,
                name,
                location,
                jump,
                selected,
                listed,
                total,
                details,
                condition,
                copying,
            } => {
                let forward_contents = contents.clone();
                let back_contents = contents.clone();
                let copy_contents = contents.clone();
                let back_path = view_path.clone();
                v_flex()
                    .pl(indent(depth))
                    .when(depth == 0, |this| this.pb_0p5())
                    .child(
                        h_flex()
                            .when(depth == 0, |row| {
                                row.debug_selector(|| "type-contents-heading".into())
                            })
                            .gap_1()
                            .child(heading_name(name, cx))
                            .when(depth == 0, |row| row.child(name_match_badge(cx)))
                            .child(location_element(
                                row_id("type-location"),
                                location,
                                jump_handler(jump),
                                cx,
                            ))
                            .when(listed > 1, |row| {
                                row.child(candidate_switcher(
                                    row_id("type-switch"),
                                    selected,
                                    total,
                                    move |cx| {
                                        back_contents
                                            .update(cx, |this, cx| {
                                                this.select_candidate(back_path.clone(), false, cx)
                                            })
                                            .ok();
                                    },
                                    move |cx| {
                                        forward_contents
                                            .update(cx, |this, cx| {
                                                this.select_candidate(view_path.clone(), true, cx)
                                            })
                                            .ok();
                                    },
                                ))
                            })
                            .when(listed <= 1 && total > 1, |row| {
                                row.child(position_element(
                                    row_id("type-position"),
                                    selected,
                                    total,
                                ))
                            })
                            .when(depth == 0, |row| {
                                row.child(
                                    div().flex_none().child(
                                        IconButton::new(row_id("copy"), IconName::Copy)
                                            .icon_size(IconSize::XSmall)
                                            .icon_color(muted)
                                            .disabled(copying)
                                            .tooltip(Tooltip::text(i18n::t!("Copy Type Contents")))
                                            .on_click(move |_, _, cx| {
                                                copy_contents
                                                    .update(cx, |this, cx| {
                                                        this.copy_type_contents(cx)
                                                    })
                                                    .ok();
                                                cx.stop_propagation();
                                            }),
                                    ),
                                )
                            }),
                    )
                    .when_some(details, |this, details| {
                        this.child(
                            Label::new(details)
                                .size(LabelSize::XSmall)
                                .buffer_font(cx)
                                .color(muted),
                        )
                    })
                    .when_some(condition, |this, condition| {
                        this.child(
                            Label::new(format!("#if {condition}"))
                                .size(LabelSize::XSmall)
                                .buffer_font(cx)
                                .color(muted),
                        )
                    })
                    .into_any_element()
            }
            Line::Member {
                depth,
                path,
                name,
                type_text,
                bit_width,
                comment,
                condition,
                toggle,
                unparsed,
                jump,
            } => {
                let toggle_contents = contents.clone();
                let full_text = type_text.clone();
                h_flex()
                    .id(row_id("member"))
                    .pl(indent(depth))
                    .gap_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .hover(move |style| style.bg(hover_background))
                    .on_click(jump_handler(jump))
                    .child(toggle_element(row_id("toggle"), toggle, move |cx| {
                        toggle_contents
                            .update(cx, |this, cx| this.toggle_member(path.clone(), cx))
                            .ok();
                    }))
                    .child(
                        div().w(name_width).flex_none().overflow_hidden().child(
                            Label::new(name)
                                .size(LabelSize::Small)
                                .buffer_font(cx)
                                .single_line()
                                .truncate(),
                        ),
                    )
                    .child(
                        div()
                            .id(row_id("type"))
                            .w(type_width)
                            .min_w_0()
                            .flex_shrink(1.)
                            .overflow_hidden()
                            .tooltip(Tooltip::text(full_text))
                            .child(
                                Label::new(type_text)
                                    .size(LabelSize::Small)
                                    .buffer_font(cx)
                                    .single_line()
                                    .truncate()
                                    .color(if unparsed { muted } else { Color::Default }),
                            ),
                    )
                    .when_some(bit_width, |row, width| {
                        row.child(
                            Label::new(format!(": {width}"))
                                .size(LabelSize::Small)
                                .buffer_font(cx)
                                .color(muted),
                        )
                    })
                    .when_some(condition, |row, condition| {
                        row.child(condition_badge(condition, cx))
                    })
                    .when_some(comment, |row, comment| {
                        row.child(
                            Label::new(comment)
                                .size(LabelSize::XSmall)
                                .color(muted)
                                .single_line()
                                .truncate(),
                        )
                    })
                    .when(toggle == Toggle::Recursive, |row| {
                        row.child(
                            Label::new(i18n::t!("recursive"))
                                .size(LabelSize::XSmall)
                                .color(muted),
                        )
                    })
                    .into_any_element()
            }
            Line::Enumerator {
                depth,
                name,
                value,
                comment,
                condition,
                jump,
            } => h_flex()
                .id(row_id("enumerator"))
                .pl(indent(depth) + px(18.))
                .gap_1()
                .rounded_sm()
                .cursor_pointer()
                .hover(move |style| style.bg(hover_background))
                .on_click(jump_handler(jump))
                .child(
                    div().w(name_width).flex_none().overflow_hidden().child(
                        Label::new(name)
                            .size(LabelSize::Small)
                            .buffer_font(cx)
                            .single_line()
                            .truncate(),
                    ),
                )
                .when_some(value, |row, (value, inferred)| {
                    row.child(
                        Label::new(if inferred {
                            format!("≈ {value}")
                        } else {
                            format!("= {value}")
                        })
                        .size(LabelSize::Small)
                        .buffer_font(cx)
                        .color(if inferred {
                            muted
                        } else {
                            Color::Default
                        }),
                    )
                })
                .when_some(condition, |row, condition| {
                    row.child(condition_badge(condition, cx))
                })
                .when_some(comment, |row, comment| {
                    row.child(
                        Label::new(comment)
                            .size(LabelSize::XSmall)
                            .color(muted)
                            .single_line()
                            .truncate(),
                    )
                })
                .into_any_element(),
            Line::More {
                depth,
                view_path,
                candidate,
                hidden,
            } => {
                let contents = contents.clone();
                h_flex()
                    .id(row_id("more"))
                    .pl(indent(depth) + px(18.))
                    .cursor_pointer()
                    .on_click(move |_, _, cx| {
                        contents
                            .update(cx, |this, cx| {
                                this.show_more_rows(view_path.clone(), candidate, cx)
                            })
                            .ok();
                        cx.stop_propagation();
                    })
                    .child(
                        Label::new(more_label(hidden))
                            .size(LabelSize::XSmall)
                            .color(Color::Accent),
                    )
                    .into_any_element()
            }
            Line::Note { depth, text } => div()
                .pl(indent(depth) + px(18.))
                .child(Label::new(text).size(LabelSize::XSmall).color(muted))
                .into_any_element(),
            Line::Loading { depth } => h_flex()
                .pl(indent(depth) + px(18.))
                .gap_1()
                .child(
                    Icon::new(IconName::ArrowCircle)
                        .size(IconSize::XSmall)
                        .color(muted)
                        .with_keyed_rotate_animation(row_id("spinner"), 2),
                )
                .child(
                    Label::new(i18n::t!("Loading…"))
                        .size(LabelSize::XSmall)
                        .color(muted),
                )
                .into_any_element(),
        };
        rows.push(row);
    }
    if let Some(markdown) = macro_markdown {
        rows.push(
            div()
                .child(
                    MarkdownElement::new(markdown, hover_markdown_style(window, cx))
                        .code_block_renderer(markdown::CodeBlockRenderer::Default {
                            copy_button_visibility: CopyButtonVisibility::Hidden,
                            wrap_button_visibility: markdown::WrapButtonVisibility::Hidden,
                            border: false,
                        }),
                )
                .into_any_element(),
        );
    }

    div()
        .id("type-contents-container")
        .overflow_y_scroll()
        .max_w(max_size.width)
        .max_h(max_size.height)
        .track_scroll(scroll_handle)
        .child(v_flex().p_2().gap_0p5().children(rows))
        .into_any_element()
}

/// The widths of the member table's name and type columns, in the buffer font's half-width
/// columns: a wide character, as in a Japanese identifier, takes two.
fn column_widths(lines: &[Line]) -> (usize, usize) {
    let width = crate::markdown_actions::display_width;
    let (names, types) = lines
        .iter()
        .fold((4, 8), |(names, types), line| match line {
            Line::Member {
                name, type_text, ..
            } => (names.max(width(name)), types.max(width(type_text))),
            Line::Enumerator { name, .. } => (names.max(width(name)), types),
            _ => (names, types),
        });
    (names.min(NAME_COLUMN_CHARS), types.min(TYPE_COLUMN_CHARS))
}

/// A heading is one row, as wide as it needs up to the popover's limit: past it, the name
/// and the location are cut short rather than the parts going to rows of their own.
fn heading_name(name: impl Into<SharedString>, cx: &App) -> impl IntoElement {
    div().min_w_0().overflow_hidden().child(
        Label::new(name)
            .size(LabelSize::Small)
            .buffer_font(cx)
            .weight(FontWeight::SEMIBOLD)
            .single_line()
            .truncate(),
    )
}

fn name_match_badge(cx: &App) -> impl IntoElement {
    div()
        .flex_none()
        .px_1()
        .rounded_sm()
        .bg(cx.theme().colors().element_background)
        .child(
            Label::new(i18n::t!("Name match"))
                .size(LabelSize::XSmall)
                .color(Color::Muted),
        )
}

fn condition_badge(condition: String, cx: &App) -> impl IntoElement {
    div()
        .px_1()
        .rounded_sm()
        .border_1()
        .border_color(cx.theme().colors().border_variant)
        .child(
            Label::new(condition)
                .size(LabelSize::XSmall)
                .buffer_font(cx)
                .color(Color::Muted)
                .single_line()
                .truncate(),
        )
}

fn location_element(
    id: ElementId,
    (folder, file): (String, String),
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> impl IntoElement {
    let full = format!("{folder}{file}");
    h_flex()
        .id(id)
        .min_w_0()
        .overflow_hidden()
        .cursor_pointer()
        .rounded_sm()
        .hover(|style| style.bg(cx.theme().colors().element_hover))
        .tooltip(Tooltip::text(full))
        .on_click(move |event, window, cx| {
            on_click(event, window, cx);
            cx.stop_propagation();
        })
        .when(!folder.is_empty(), |row| {
            row.child(
                Label::new(folder)
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
                    .single_line()
                    .truncate_start(),
            )
        })
        .child(
            Label::new(file)
                .size(LabelSize::XSmall)
                .color(Color::Accent)
                .single_line(),
        )
}

fn candidate_switcher(
    id: ElementId,
    selected: usize,
    total: usize,
    on_back: impl Fn(&mut App) + 'static,
    on_forward: impl Fn(&mut App) + 'static,
) -> impl IntoElement {
    let back_id = ElementId::Name(format!("{id}-back").into());
    let forward_id = ElementId::Name(format!("{id}-forward").into());
    let position_id = ElementId::Name(format!("{id}-position").into());
    h_flex()
        .debug_selector(|| "type-contents-switcher".into())
        .flex_none()
        .gap_0p5()
        .child(
            IconButton::new(back_id, IconName::ChevronLeft)
                .icon_size(IconSize::XSmall)
                .tooltip(Tooltip::text(i18n::t!("Previous Definition")))
                .on_click(move |_, _, cx| {
                    on_back(cx);
                    cx.stop_propagation();
                }),
        )
        .child(position_element(position_id, selected, total))
        .child(
            IconButton::new(forward_id, IconName::ChevronRight)
                .icon_size(IconSize::XSmall)
                .tooltip(Tooltip::text(i18n::t!("Next Definition")))
                .on_click(move |_, _, cx| {
                    on_forward(cx);
                    cx.stop_propagation();
                }),
        )
}

fn toggle_element(
    id: ElementId,
    toggle: Toggle,
    on_toggle: impl Fn(&mut App) + 'static,
) -> AnyElement {
    let slot = div().w(px(16.)).flex_none();
    match toggle {
        Toggle::None | Toggle::Recursive => slot.into_any_element(),
        Toggle::Loading => slot
            .child(
                Icon::new(IconName::ArrowCircle)
                    .size(IconSize::XSmall)
                    .color(Color::Muted)
                    .with_keyed_rotate_animation(id, 2),
            )
            .into_any_element(),
        Toggle::Collapsed | Toggle::Expanded => slot
            .debug_selector({
                let id = id.clone();
                move || id.to_string()
            })
            .child(
                IconButton::new(
                    id,
                    if toggle == Toggle::Expanded {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    },
                )
                .icon_size(IconSize::XSmall)
                .on_click(move |_, _, cx| {
                    on_toggle(cx);
                    cx.stop_propagation();
                }),
            )
            .into_any_element(),
    }
}

/// Locations are written with the project's separator; tests compare them on every platform.
#[cfg(test)]
fn unix_folder(folder: &str) -> String {
    folder.replace('\\', "/")
}

#[cfg(test)]
impl TypeContents {
    /// The section as text: one line per row, two spaces per level.
    pub(crate) fn text_for_tests(&self) -> String {
        let indent = |depth: usize| "  ".repeat(depth);
        let mut text = Vec::new();
        for line in self.lines() {
            text.push(match line {
                Line::Variable {
                    name,
                    type_text,
                    confidence,
                } => format!("{} {type_text} ({confidence})", variable_label(&name)),
                Line::Chain(segments) => segments
                    .iter()
                    .map(|segment| {
                        let mut text = segment.text.clone();
                        if let Some(type_text) = &segment.type_text {
                            text.push_str(&format!(" ({type_text})"));
                        }
                        if !segment.found {
                            text.push_str(&format!(" {}", i18n::t!("could not be followed")));
                        }
                        text
                    })
                    .collect::<Vec<_>>()
                    .join(" → "),
                Line::Macro {
                    name,
                    location: (folder, file),
                    selected,
                    total,
                    ..
                } => {
                    let found = self
                        .macros
                        .as_ref()
                        .and_then(|macros| macros.macros.get(selected));
                    format!(
                        "{name} [{}] {}{file} ({}) {}",
                        i18n::t!("Name match"),
                        unix_folder(&folder),
                        definition_position_label(selected, total),
                        found.map(|found| found.text.as_str()).unwrap_or_default()
                    )
                }
                Line::TypeHeading {
                    depth,
                    name,
                    location: (folder, file),
                    selected,
                    total,
                    details,
                    condition,
                    ..
                } => {
                    let mut line = format!("{}{name}", indent(depth));
                    if depth == 0 {
                        line.push_str(&format!(" [{}]", i18n::t!("Name match")));
                    }
                    line.push_str(&format!(" {}{file}", unix_folder(&folder)));
                    if total > 1 {
                        line.push_str(&format!(
                            " ({})",
                            definition_position_label(selected, total)
                        ));
                    }
                    if let Some(details) = details {
                        line.push_str(&format!(" {{{details}}}"));
                    }
                    if let Some(condition) = condition {
                        line.push_str(&format!(" #if {condition}"));
                    }
                    line
                }
                Line::Member {
                    depth,
                    name,
                    type_text,
                    bit_width,
                    comment,
                    condition,
                    toggle,
                    ..
                } => {
                    let glyph = match toggle {
                        Toggle::None => " ",
                        Toggle::Collapsed => "▸",
                        Toggle::Expanded => "▾",
                        Toggle::Loading => "…",
                        Toggle::Recursive => "↺",
                    };
                    let mut line = format!("{}{glyph} {name} | {type_text}", indent(depth));
                    if let Some(width) = bit_width {
                        line.push_str(&format!(" : {width}"));
                    }
                    if let Some(condition) = condition {
                        line.push_str(&format!(" [{condition}]"));
                    }
                    if let Some(comment) = comment {
                        line.push_str(&format!(" // {comment}"));
                    }
                    if toggle == Toggle::Recursive {
                        line.push_str(&format!(" ({})", i18n::t!("recursive")));
                    }
                    line
                }
                Line::Enumerator {
                    depth, name, value, ..
                } => match value {
                    Some((value, true)) => format!("{}  {name} ≈ {value}", indent(depth)),
                    Some((value, false)) => format!("{}  {name} = {value}", indent(depth)),
                    None => format!("{}  {name}", indent(depth)),
                },
                Line::More { depth, hidden, .. } => {
                    format!("{}  {}", indent(depth), more_label(hidden))
                }
                Line::Note { depth, text } => format!("{}  {text}", indent(depth)),
                Line::Loading { depth } => {
                    format!("{}  {}", indent(depth), i18n::t!("Loading…"))
                }
            });
        }
        text.join("\n")
    }

    fn member_path_for_tests(&self, name: &str) -> MemberPath {
        self.lines()
            .into_iter()
            .find_map(|line| match line {
                Line::Member {
                    name: member, path, ..
                } if member == name => Some(path),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no member {name:?} in\n{}", self.text_for_tests()))
    }

    fn jump_for_tests(&self, name: &str) -> Jump {
        self.lines()
            .into_iter()
            .find_map(|line| match line {
                Line::Member {
                    name: shown, jump, ..
                }
                | Line::TypeHeading {
                    name: shown, jump, ..
                } if shown == name => Some(jump),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no row {name:?} in\n{}", self.text_for_tests()))
    }

    fn view_path_for_tests(&self, name: &str) -> MemberPath {
        self.lines()
            .into_iter()
            .find_map(|line| match line {
                Line::TypeHeading {
                    name: shown,
                    view_path,
                    ..
                } if shown == name => Some(view_path),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no type {name:?} in\n{}", self.text_for_tests()))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use futures::StreamExt as _;
    use gpui::{TestAppContext, UpdateGlobal as _};
    use language::{Language, LanguageConfig, LanguageMatcher};
    use multi_buffer::{MultiBufferOffset, ToPoint as _};
    use project::symbol_index_store::build_index_for_tests;
    use settings::SettingsStore;

    use super::*;
    use crate::{
        EditorSettings, Hover,
        editor_tests::init_test,
        hover_popover::{hide_hover, hover, hover_at},
        test::editor_lsp_test_context::EditorLspTestContext,
    };

    const TYPES_H: &str = "\
#define MAX_ITEMS 16
struct leaf { int value; };
struct inner { int depth; struct leaf leaf; };
struct node { struct node *next; int key; };
typedef struct outer {
    struct inner in; /* the inner part */
    struct { int a; int b; } anon;
    int (*callback)(int);
    unsigned flags : 3;
    struct missing *gone;
} outer_t;
typedef outer_t alias_t;
enum color { RED, GREEN = 5, BLUE };
struct dup { int in_header; };
";

    const FILE_C: &str = "\
#include \"types.h\"
struct dup { int in_main; };
static struct node *head;
static struct big *many;
int use(alias_t *value)
{
    struct outer local;
    enum color shade;
    return value->in.depth + local.flags + MAX_ITEMS + mystery + shade;
}
";

    fn big_h() -> String {
        let members = (0..35)
            .map(|index| format!("    int member_{index};\n"))
            .collect::<String>();
        format!("struct big {{\n{members}}};\n")
    }

    fn c_language() -> Language {
        Language::new(
            LanguageConfig {
                name: "C".into(),
                matcher: LanguageMatcher {
                    path_suffixes: vec!["c".to_string()],
                    ..LanguageMatcher::default()
                }
                .into(),
                ..LanguageConfig::default()
            },
            None,
        )
    }

    async fn c_context(with_hover: bool, cx: &mut TestAppContext) -> EditorLspTestContext {
        init_test(cx, |_| {});
        cx.executor().allow_parking();
        EditorLspTestContext::new(
            c_language(),
            lsp::ServerCapabilities {
                hover_provider: with_hover.then_some(lsp::HoverProviderCapability::Simple(true)),
                ..lsp::ServerCapabilities::default()
            },
            cx,
        )
        .await
    }

    /// Writes `others` next to the edited file and gives the project an index of them and of
    /// `edited`, the edited file's text.
    async fn install_index(
        cx: &mut EditorLspTestContext,
        edited: (&str, &str),
        others: &[(&str, &str)],
    ) -> tempfile::TempDir {
        let project = cx.update_workspace(|workspace, _, _| workspace.project().clone());
        let fs = cx.update_workspace(|workspace, _, cx| workspace.project().read(cx).fs().clone());
        for (path, text) in others {
            fs.as_fake()
                .insert_file(
                    EditorLspTestContext::root_path().join(path),
                    text.as_bytes().to_vec(),
                )
                .await;
        }
        cx.executor().run_until_parked();
        let directory = tempfile::tempdir().expect("a temporary directory");
        let mut files = vec![edited];
        files.extend_from_slice(others);
        let index = build_index_for_tests(directory.path(), &files).expect("building the index");
        cx.update_workspace(|_, _, cx| {
            let worktree_id = project
                .read(cx)
                .worktrees(cx)
                .next()
                .expect("a worktree")
                .read(cx)
                .id();
            project.update(cx, |project, cx| {
                project.symbol_index_store().update(cx, |store, cx| {
                    store.insert_index_for_tests(worktree_id, index, cx)
                })
            });
        });
        directory
    }

    /// A C file with the fixture's types, indexed.
    async fn setup(
        with_hover: bool,
        cx: &mut TestAppContext,
    ) -> (EditorLspTestContext, tempfile::TempDir) {
        let mut cx = c_context(with_hover, cx).await;
        cx.set_state(&format!("ˇ{FILE_C}"));
        let big = big_h();
        let index = install_index(
            &mut cx,
            ("dir/file.c", FILE_C),
            &[
                ("dir/types.h", TYPES_H),
                ("dir/big.h", big.as_str()),
                (
                    "dir/other_dup.h",
                    "struct dup { int in_other; };
",
                ),
            ],
        )
        .await;
        (cx, index)
    }

    fn anchor_of(cx: &mut EditorLspTestContext, needle: &str, occurrence: usize) -> Anchor {
        cx.update_editor(|editor, window, cx| {
            let snapshot = editor.snapshot(window, cx);
            let text = snapshot.buffer_snapshot().text();
            let offset = text
                .match_indices(needle)
                .nth(occurrence)
                .map(|(offset, _)| offset)
                .unwrap_or_else(|| panic!("{needle:?} #{occurrence} not in the buffer"));
            snapshot
                .buffer_snapshot()
                .anchor_before(MultiBufferOffset(offset))
        })
    }

    fn hover_delay(cx: &mut EditorLspTestContext) -> u64 {
        cx.update(|_, cx| EditorSettings::get_global(cx).hover_popover_delay.0)
    }

    /// Moves the pointer onto the `occurrence`-th `needle` without waiting.
    fn point_at(cx: &mut EditorLspTestContext, needle: &str, occurrence: usize) {
        let anchor = anchor_of(cx, needle, occurrence);
        cx.update_editor(|editor, window, cx| hover_at(editor, Some(anchor), None, window, cx));
    }

    /// Hovers the `occurrence`-th `needle` and waits for the popovers.
    fn hover_over(cx: &mut EditorLspTestContext, needle: &str, occurrence: usize) {
        point_at(cx, needle, occurrence);
        let delay = hover_delay(cx);
        cx.executor()
            .advance_clock(Duration::from_millis(delay + 100));
        cx.run_until_parked();
    }

    fn section(cx: &mut EditorLspTestContext) -> Option<Entity<TypeContents>> {
        cx.editor(|editor, _, _| {
            editor
                .hover_state
                .info_popovers
                .iter()
                .find_map(|popover| popover.type_contents.clone())
        })
    }

    fn section_text(cx: &mut EditorLspTestContext) -> String {
        let section = section(cx).expect("a type contents section");
        cx.update(|_, cx| section.read(cx).text_for_tests())
    }

    fn toggle(cx: &mut EditorLspTestContext, member: &str) {
        let section = section(cx).expect("a type contents section");
        cx.update(|_, cx| {
            section.update(cx, |section, cx| {
                let path = section.member_path_for_tests(member);
                section.toggle_member(path, cx);
            })
        });
        cx.run_until_parked();
    }

    fn set_hover_type_contents(cx: &mut EditorLspTestContext, enabled: bool) {
        cx.update(|_, cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings
                        .project
                        .symbol_index
                        .get_or_insert_default()
                        .hover_type_contents = Some(enabled);
                });
            });
        });
    }

    #[gpui::test]
    async fn test_a_type_name_shows_its_members_and_typedefs(cx: &mut TestAppContext) {
        let (mut cx, _index) = setup(false, cx).await;
        hover_over(&mut cx, "alias_t", 0);
        let text = section_text(&mut cx);
        assert_eq!(
            text.lines().next(),
            Some("struct outer [Name match] dir/types.h:5 {alias_t → outer_t → struct outer}"),
            "{text}"
        );
        for row in [
            "▸ in | struct inner // the inner part",
            "▾ anon | struct {...}
    a | int",
            "    b | int",
            "  callback | int (*)(int)",
            "  flags | unsigned : 3",
            "▸ gone | struct missing *",
        ] {
            assert!(text.contains(row), "{row:?} not in\n{text}");
        }
    }

    #[gpui::test]
    async fn test_variables_members_macros_and_enums_are_described(cx: &mut TestAppContext) {
        let (mut cx, _index) = setup(false, cx).await;

        hover_over(&mut cx, "local", 1);
        let text = section_text(&mut cx);
        assert!(
            text.starts_with(
                "Type of local: struct outer (declared in this function)\nstruct outer"
            ),
            "{text}"
        );

        hover_over(&mut cx, "depth", 0);
        let text = section_text(&mut cx);
        assert_eq!(
            text, "struct outer → .in (struct inner) → .depth (int)",
            "an int has no contents"
        );

        hover_over(&mut cx, "in.", 0);
        let text = section_text(&mut cx);
        assert!(
            text.starts_with(
                "struct outer → .in (struct inner)\nstruct inner [Name match] dir/types.h:3\n  depth | int\n▸ leaf | struct leaf"
            ),
            "{text}"
        );

        hover_over(&mut cx, "MAX_ITEMS", 0);
        assert_eq!(
            section_text(&mut cx),
            "MAX_ITEMS [Name match] dir/types.h:1 (Definition 1 of 1) #define MAX_ITEMS 16"
        );

        hover_over(&mut cx, "shade", 0);
        let text = section_text(&mut cx);
        assert!(
            text.ends_with(
                "enum color [Name match] dir/types.h:13\n  RED ≈ 0\n  GREEN = 5\n  BLUE ≈ 6"
            ),
            "{text}"
        );
    }

    #[gpui::test]
    async fn test_members_expand_level_by_level(cx: &mut TestAppContext) {
        let (mut cx, _index) = setup(false, cx).await;
        hover_over(&mut cx, "alias_t", 0);

        toggle(&mut cx, "in");
        let text = section_text(&mut cx);
        assert!(
            text.contains(
                "▾ in | struct inner // the inner part\n  struct inner dir/types.h:3\n    depth | int\n  ▸ leaf | struct leaf"
            ),
            "{text}"
        );
        toggle(&mut cx, "leaf");
        let text = section_text(&mut cx);
        assert!(
            text.contains(
                "  ▾ leaf | struct leaf\n    struct leaf dir/types.h:2\n      value | int"
            ),
            "{text}"
        );

        toggle(&mut cx, "gone");
        let text = section_text(&mut cx);
        assert!(
            text.contains("▾ gone | struct missing *\n    Definition not found"),
            "{text}"
        );

        // Collapsing keeps what was read while the popover is open.
        toggle(&mut cx, "in");
        let text = section_text(&mut cx);
        assert!(text.contains("▸ in | struct inner"), "{text}");
        assert!(!text.contains("value | int"), "{text}");
        toggle(&mut cx, "in");
        assert!(section_text(&mut cx).contains("      value | int"));
    }

    #[gpui::test]
    async fn test_a_member_naming_its_own_type_is_not_expanded(cx: &mut TestAppContext) {
        let (mut cx, _index) = setup(false, cx).await;
        hover_over(&mut cx, "node", 0);
        let text = section_text(&mut cx);
        assert!(
            text.contains("↺ next | struct node * (recursive)"),
            "{text}"
        );
        toggle(&mut cx, "next");
        assert_eq!(section_text(&mut cx), text, "nothing to expand");
    }

    #[gpui::test]
    async fn test_clicking_a_toggle_expands_the_member(cx: &mut TestAppContext) {
        let (mut cx, _index) = setup(false, cx).await;
        hover_over(&mut cx, "alias_t", 0);
        let cursor_before = cx.editor(|editor, _, cx| {
            editor
                .selections
                .newest_anchor()
                .head()
                .to_point(&editor.buffer().read(cx).snapshot(cx))
        });
        cx.update(|window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
        });
        let toggle = cx
            .debug_bounds("type-contents-toggle-1")
            .expect("the toggle of `in` is drawn");
        cx.simulate_mouse_move(toggle.center(), None, gpui::Modifiers::none());
        cx.simulate_click(toggle.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        let text = section_text(&mut cx);
        assert!(text.contains("▾ in | struct inner"), "{text}");
        assert!(text.contains("depth | int"), "{text}");
        let cursor_after = cx.editor(|editor, _, cx| {
            editor
                .selections
                .newest_anchor()
                .head()
                .to_point(&editor.buffer().read(cx).snapshot(cx))
        });
        assert_eq!(
            cursor_before, cursor_after,
            "the click stays in the popover"
        );
    }

    #[gpui::test]
    async fn test_same_name_definitions_can_be_switched(cx: &mut TestAppContext) {
        let (mut cx, _index) = setup(false, cx).await;
        hover_over(&mut cx, "dup", 0);
        assert_eq!(
            section_text(&mut cx),
            "struct dup [Name match] dir/file.c:2 (Definition 1 of 3)
  in_main | int"
        );
        let section = section(&mut cx).expect("a section");
        let mut shown = Vec::new();
        for _ in 0..3 {
            cx.update(|_, cx| {
                section.update(cx, |section, cx| {
                    let path = section.view_path_for_tests("struct dup");
                    section.select_candidate(path, true, cx);
                })
            });
            cx.run_until_parked();
            shown.push(section_text(&mut cx));
        }
        shown.sort();
        assert_eq!(
            shown,
            vec![
                "struct dup [Name match] dir/file.c:2 (Definition 1 of 3)
  in_main | int",
                "struct dup [Name match] dir/other_dup.h:1 (Definition 2 of 3)
  in_other | int",
                "struct dup [Name match] dir/types.h:14 (Definition 3 of 3)
  in_header | int",
            ],
            "each definition is read when it is shown, and the one shown is the one read"
        );
    }

    #[gpui::test]
    async fn test_definitions_past_the_read_limit_are_read_when_shown(cx: &mut TestAppContext) {
        let mut cx = c_context(false, cx).await;
        let edited = "static struct many *pointer;\n";
        cx.set_state(&format!("ˇ{edited}"));
        let count = project::symbol_index_store::MAX_TYPE_READ_LIMIT + 2;
        let headers = (0..count)
            .map(|index| {
                (
                    format!("dir/many_{index}.h"),
                    format!("struct many {{ int in_{index}; }};\n"),
                )
            })
            .collect::<Vec<_>>();
        let others = headers
            .iter()
            .map(|(path, text)| (path.as_str(), text.as_str()))
            .collect::<Vec<_>>();
        let _index = install_index(&mut cx, ("dir/file.c", edited), &others).await;
        hover_over(&mut cx, "many", 0);
        let section = section(&mut cx).expect("a section");
        let mut shown = vec![section_text(&mut cx)];
        for _ in 1..count {
            cx.update(|_, cx| {
                section.update(cx, |section, cx| {
                    let path = section.view_path_for_tests("struct many");
                    section.select_candidate(path, true, cx);
                })
            });
            cx.run_until_parked();
            shown.push(section_text(&mut cx));
        }
        let mut members = Vec::new();
        for text in &shown {
            assert!(
                !text.contains("Only the location of this definition is known"),
                "every definition is read when it is shown:\n{text}"
            );
            let member = text
                .lines()
                .find_map(|line| line.trim().strip_suffix(" | int"))
                .unwrap_or_else(|| panic!("a member in\n{text}"));
            let header = text.lines().next().unwrap_or_default();
            let index = member.trim_start_matches("in_");
            assert!(
                header.contains(&format!("many_{index}.h")),
                "the definition shown is the one read:\n{text}"
            );
            members.push(member.to_string());
        }
        members.sort();
        members.dedup();
        assert_eq!(members.len(), count, "{shown:#?}");
    }

    #[gpui::test]
    async fn test_typedefs_past_the_read_limit_are_read_when_shown(cx: &mut TestAppContext) {
        let count = project::symbol_index_store::MAX_TYPE_READ_LIMIT + 2;
        let headers = (0..count)
            .map(|index| {
                (
                    format!("dir/many_{index}.h"),
                    format!("typedef struct many_{index} {{ int in_{index}; }} many_t;\n"),
                )
            })
            .collect::<Vec<_>>();
        check_typedefs_are_read_when_shown(headers, count, cx).await;
    }

    #[gpui::test]
    async fn test_typedefs_of_types_elsewhere_are_read_when_shown(cx: &mut TestAppContext) {
        let count = project::symbol_index_store::MAX_TYPE_READ_LIMIT + 2;
        let headers = (0..count)
            .flat_map(|index| {
                [
                    (
                        format!("dir/many_{index}.h"),
                        format!("typedef struct body_{index} many_t;\n"),
                    ),
                    (
                        format!("dir/body_{index}.h"),
                        format!("struct body_{index} {{ int in_{index}; }};\n"),
                    ),
                ]
            })
            .collect::<Vec<_>>();
        check_typedefs_are_read_when_shown(headers, count, cx).await;
    }

    async fn check_typedefs_are_read_when_shown(
        headers: Vec<(String, String)>,
        count: usize,
        cx: &mut TestAppContext,
    ) {
        let mut cx = c_context(false, cx).await;
        let edited = "static many_t *pointer;\n";
        cx.set_state(&format!("ˇ{edited}"));
        let others = headers
            .iter()
            .map(|(path, text)| (path.as_str(), text.as_str()))
            .collect::<Vec<_>>();
        let _index = install_index(&mut cx, ("dir/file.c", edited), &others).await;
        hover_over(&mut cx, "many_t", 0);
        let section = section(&mut cx).expect("a section");
        let mut shown = vec![section_text(&mut cx)];
        for _ in 1..count {
            cx.update(|_, cx| {
                section.update(cx, |section, cx| {
                    // Each definition is headed by the struct its typedef names.
                    let text = section.text_for_tests();
                    let heading = text
                        .lines()
                        .next()
                        .and_then(|line| line.split(" [").next())
                        .unwrap_or_default()
                        .to_string();
                    let path = section.view_path_for_tests(&heading);
                    section.select_candidate(path, true, cx);
                })
            });
            cx.run_until_parked();
            shown.push(section_text(&mut cx));
        }
        let mut members = Vec::new();
        for text in &shown {
            assert!(
                !text.contains("Only the location of this definition is known"),
                "every typedef is read when it is shown:\n{text}"
            );
            let member = text
                .lines()
                .find_map(|line| line.trim().strip_suffix(" | int"))
                .unwrap_or_else(|| panic!("a member in\n{text}"));
            members.push(member.to_string());
        }
        members.sort();
        members.dedup();
        assert_eq!(members.len(), count, "{shown:#?}");
    }

    #[gpui::test]
    async fn test_typedefs_sharing_a_file_are_not_guessed_between(cx: &mut TestAppContext) {
        let mut cx = c_context(false, cx).await;
        let edited = "static many_t *pointer;\n";
        cx.set_state(&format!("ˇ{edited}"));
        let _index = install_index(
            &mut cx,
            ("dir/file.c", edited),
            &[
                (
                    "dir/many_0.h",
                    "typedef struct many_0 { int in_0; } many_t;\n",
                ),
                (
                    "dir/many_1.h",
                    "#ifdef ONE\ntypedef struct one { int in_one; } many_t;\n#else\ntypedef struct two { int in_two; } many_t;\n#endif\n",
                ),
            ],
        )
        .await;
        hover_over(&mut cx, "many_t", 0);
        let section = section(&mut cx).expect("a section");
        let first = section_text(&mut cx);
        assert!(first.contains("in_0 | int"), "{first}");
        let mut shown = Vec::new();
        for _ in 0..2 {
            cx.update(|_, cx| {
                section.update(cx, |section, cx| {
                    let text = section.text_for_tests();
                    let heading = text
                        .lines()
                        .next()
                        .and_then(|line| line.split(" [").next())
                        .unwrap_or_default()
                        .to_string();
                    let path = section.view_path_for_tests(&heading);
                    section.select_candidate(path, true, cx);
                })
            });
            cx.run_until_parked();
            shown.push(section_text(&mut cx));
        }
        // The index keeps the typedef step of a type defined in place, so each of the two is
        // read by its own location rather than by being the only one in its file.
        for (text, (member, other)) in shown
            .iter()
            .zip([("in_one | int", "in_two"), ("in_two | int", "in_one")])
        {
            assert!(text.contains("many_1.h"), "{shown:#?}");
            assert!(text.contains(member), "{shown:#?}");
            assert!(!text.contains(other), "{shown:#?}");
            assert!(
                !text.contains("Only the location of this definition is known"),
                "{shown:#?}"
            );
        }
    }

    #[gpui::test]
    async fn test_long_types_show_their_first_rows(cx: &mut TestAppContext) {
        let (mut cx, _index) = setup(false, cx).await;
        hover_over(&mut cx, "big", 0);
        let text = section_text(&mut cx);
        assert!(text.contains("member_29 | int"), "{text}");
        assert!(!text.contains("member_30"), "{text}");
        assert!(text.ends_with("5 more"), "{text}");
        let section = section(&mut cx).expect("a section");
        cx.update(|_, cx| {
            section.update(cx, |section, cx| {
                let path = section.view_path_for_tests("struct big");
                section.show_more_rows(path, 0, cx);
            })
        });
        let text = section_text(&mut cx);
        assert!(text.ends_with("member_34 | int"), "{text}");
    }

    #[gpui::test]
    async fn test_unsaved_edits_are_described(cx: &mut TestAppContext) {
        let (mut cx, _index) = setup(false, cx).await;
        let edited =
            format!("ˇ{FILE_C}struct fresh {{ int new_field; }};\nstruct fresh *pointer;\n");
        cx.set_state(&edited);
        hover_over(&mut cx, "fresh", 1);
        assert_eq!(
            section_text(&mut cx),
            "struct fresh [Name match] dir/file.c:11\n  new_field | int"
        );
    }

    #[gpui::test]
    async fn test_the_language_server_hover_comes_first(cx: &mut TestAppContext) {
        let (mut cx, _index) = setup(true, cx).await;
        let mut requests =
            cx.set_request_handler::<lsp::request::HoverRequest, _, _>(|_, _, _| async move {
                Ok(Some(lsp::Hover {
                    contents: lsp::HoverContents::Markup(lsp::MarkupContent {
                        kind: lsp::MarkupKind::Markdown,
                        value: "### variable `local`\n\n---\nType: `struct outer`".to_string(),
                    }),
                    range: None,
                }))
            });
        hover_over(&mut cx, "local", 1);
        requests.next().await;
        cx.run_until_parked();
        let (count, first_is_markdown) = cx.editor(|editor, _, _| {
            let popovers = &editor.hover_state.info_popovers;
            (
                popovers.len(),
                popovers
                    .first()
                    .is_some_and(|popover| popover.parsed_content.is_some()),
            )
        });
        assert_eq!(count, 2);
        assert!(
            first_is_markdown,
            "the language server's popover stays first"
        );
        assert!(section_text(&mut cx).contains("in | struct inner"));
    }

    #[gpui::test]
    async fn test_the_language_server_type_helps_when_the_index_cannot_tell(
        cx: &mut TestAppContext,
    ) {
        let (mut cx, _index) = setup(true, cx).await;
        cx.set_request_handler::<lsp::request::HoverRequest, _, _>(|_, _, _| async move {
            Ok(Some(lsp::Hover {
                contents: lsp::HoverContents::Markup(lsp::MarkupContent {
                    kind: lsp::MarkupKind::Markdown,
                    value:
                        "### variable `mystery`\n\n---\nType: `struct inner *` (aka `struct inner *`)"
                            .to_string(),
                }),
                range: None,
            }))
        });
        hover_over(&mut cx, "mystery", 0);
        let text = section_text(&mut cx);
        assert!(
            text.starts_with(
                "Type of mystery: struct inner * (type named by the language server)\nstruct inner"
            ),
            "{text}"
        );
    }

    #[gpui::test]
    async fn test_without_an_index_nothing_is_added(cx: &mut TestAppContext) {
        let mut cx = c_context(true, cx).await;
        cx.set_state(&format!("ˇ{FILE_C}"));
        cx.set_request_handler::<lsp::request::HoverRequest, _, _>(|_, _, _| async move {
            Ok(Some(lsp::Hover {
                contents: lsp::HoverContents::Scalar(lsp::MarkedString::String("docs".to_string())),
                range: None,
            }))
        });
        let before = queries_for_tests();
        hover_over(&mut cx, "local", 1);
        assert_eq!(queries_for_tests(), before + 1, "the index was asked");
        let count = cx.editor(|editor, _, _| editor.hover_state.info_popovers.len());
        assert_eq!(count, 1, "the language server's hover alone");
        assert!(section(&mut cx).is_none());
    }

    /// The text of the popovers that are neither the language server's nor a section.
    fn notes(cx: &mut EditorLspTestContext) -> Vec<String> {
        cx.editor(|editor, _, cx| {
            editor
                .hover_state
                .info_popovers
                .iter()
                .filter(|popover| popover.type_contents.is_none())
                .filter_map(|popover| popover.parsed_content.as_ref())
                .map(|markdown| markdown.read(cx).source().to_string())
                .collect()
        })
    }

    fn scratch_files(cx: &mut EditorLspTestContext) -> Vec<String> {
        let project = cx.update_workspace(|workspace, _, _| workspace.project().clone());
        cx.update(|_, cx| {
            project
                .read(cx)
                .symbol_index_store()
                .read(cx)
                .scratch_files_for_tests()
        })
    }

    async fn ignore_dir(cx: &mut EditorLspTestContext) {
        let fs = cx.update_workspace(|workspace, _, cx| workspace.project().read(cx).fs().clone());
        fs.as_fake()
            .insert_file(
                EditorLspTestContext::root_path().join(".gitignore"),
                b"dir/\n".to_vec(),
            )
            .await;
        cx.executor().run_until_parked();
    }

    async fn insert_files(cx: &mut EditorLspTestContext, files: &[(&str, &str)]) {
        let fs = cx.update_workspace(|workspace, _, cx| workspace.project().read(cx).fs().clone());
        for (path, text) in files {
            let path = EditorLspTestContext::root_path().join(path);
            if let Some(parent) = path.parent() {
                fs.create_dir(parent).await.expect("creating the folder");
            }
            fs.as_fake()
                .insert_file(path, text.as_bytes().to_vec())
                .await;
        }
        cx.executor().run_until_parked();
    }

    #[gpui::test]
    async fn test_an_ignored_file_reads_itself_and_its_headers(cx: &mut TestAppContext) {
        let (mut cx, _index) = setup(false, cx).await;
        ignore_dir(&mut cx).await;

        hover_over(&mut cx, "local", 1);
        let text = section_text(&mut cx);
        assert!(
            text.starts_with("Type of local: struct outer (declared in this function)"),
            "{text}"
        );
        assert!(text.contains("dir/types.h:5"), "{text}");
        assert!(notes(&mut cx).is_empty());

        hover_over(&mut cx, "many", 0);
        let found = notes(&mut cx);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            found[0].contains("symbol_index.include_ignored"),
            "{found:?}"
        );

        for (name, occurrence) in [("use", 0), ("return", 0), ("mystery", 0)] {
            hover_over(&mut cx, name, occurrence);
            assert!(
                cx.editor(|editor, _, _| editor.hover_state.info_popovers.is_empty()),
                "{name}: only a name whose type's contents were to be expected gets a note"
            );
        }
    }

    #[gpui::test]
    async fn test_names_of_no_type_or_a_standard_one_get_no_note(cx: &mut TestAppContext) {
        let mut cx = c_context(false, cx).await;
        ignore_dir(&mut cx).await;
        cx.set_state(
            "ˇ#include <stdio.h>\n\
             size_t count;\nFILE *out;\nuint32_t bits;\nint plain;\n\
             struct absent *gone;\n\
             int run(void) { return count; }\n",
        );
        for (name, occurrence) in [
            ("count", 0),
            ("out", 0),
            ("bits", 0),
            ("plain", 0),
            ("run", 0),
        ] {
            hover_over(&mut cx, name, occurrence);
            assert!(
                cx.editor(|editor, _, _| editor.hover_state.info_popovers.is_empty()),
                "{name} gets no note"
            );
        }
        hover_over(&mut cx, "gone", 0);
        assert_eq!(notes(&mut cx).len(), 1, "a struct of its own does");
    }

    #[test]
    fn test_standard_types_are_told_apart() {
        for name in [
            "size_t",
            "FILE",
            "int32_t",
            "uint_least8_t",
            "intptr_t",
            "std::string",
        ] {
            assert!(!is_name_worth_a_note(name), "{name}");
        }
        for name in ["outer_t", "inner", "ns::node", "int_list_t", "uint"] {
            assert!(is_name_worth_a_note(name), "{name}");
        }
    }

    const NESTED_H: &str = "\
struct inner { int depth; char tag; };
typedef struct { struct inner in; int flags; } Outer;
";

    const NESTED_C: &str = "\
#include \"nested.h\"
int use(void) { Outer value; return value.flags; }
";

    #[gpui::test]
    async fn test_nested_types_of_an_ignored_folder_expand(cx: &mut TestAppContext) {
        let mut cx = c_context(false, cx).await;
        insert_files(&mut cx, &[("dir/nested.h", NESTED_H)]).await;
        ignore_dir(&mut cx).await;
        cx.set_state(&format!("ˇ{NESTED_C}"));

        hover_over(&mut cx, "value", 0);
        let text = section_text(&mut cx);
        assert!(text.starts_with("Type of value: Outer"), "{text}");
        assert!(text.contains("in | struct inner"), "{text}");
        assert!(!text.contains("depth | int"), "{text}");
        toggle(&mut cx, "in");
        let text = section_text(&mut cx);
        assert!(text.contains("depth | int"), "{text}");
        assert!(text.contains("tag | char"), "{text}");
    }

    #[gpui::test]
    async fn test_headers_past_the_include_depth_are_not_read(cx: &mut TestAppContext) {
        let mut cx = c_context(false, cx).await;
        insert_files(
            &mut cx,
            &[
                ("dir/h1.h", "#include \"h2.h\"\n"),
                ("dir/h2.h", "#include \"sub/h3.h\"\n"),
                ("dir/sub/h3.h", "#include \"../h4.h\"\n"),
                (
                    "dir/h4.h",
                    "#include \"h5.h\"\nstruct reached { int four; };\n",
                ),
                ("dir/h5.h", "struct too_far { int five; };\n"),
            ],
        )
        .await;
        ignore_dir(&mut cx).await;
        cx.set_state("ˇ#include \"h1.h\"\nstruct reached a;\nstruct too_far b;\n");

        hover_over(&mut cx, "reached", 0);
        assert!(section_text(&mut cx).contains("four | int"));

        hover_over(&mut cx, "too_far", 0);
        assert_eq!(
            scratch_files(&mut cx),
            [
                "dir/file.c",
                "dir/h1.h",
                "dir/h2.h",
                "dir/h4.h",
                "dir/sub/h3.h"
            ]
        );
        assert!(section(&mut cx).is_none());
        assert_eq!(notes(&mut cx).len(), 1);
    }

    #[gpui::test]
    async fn test_private_headers_are_not_read(cx: &mut TestAppContext) {
        let mut cx = c_context(false, cx).await;
        cx.update(|_, cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.project.worktree.private_files =
                        Some(vec!["**/secret.h".to_string()].into());
                });
            });
        });
        insert_files(
            &mut cx,
            &[
                ("dir/secret.h", "struct hidden { int key; };\n"),
                ("dir/open.h", "struct shown { int value; };\n"),
            ],
        )
        .await;
        ignore_dir(&mut cx).await;
        cx.set_state(
            "ˇ#include \"secret.h\"\n#include \"open.h\"\nstruct hidden a;\nstruct shown b;\n",
        );

        hover_over(&mut cx, "shown", 0);
        assert!(section_text(&mut cx).contains("value | int"));

        hover_over(&mut cx, "hidden", 0);
        assert_eq!(
            scratch_files(&mut cx),
            ["dir/file.c", "dir/open.h"],
            "the private header is not read"
        );
        assert!(section(&mut cx).is_none());
        let found = notes(&mut cx);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(!found[0].contains("key"), "{found:?}");
    }

    fn set_private_files(cx: &mut EditorLspTestContext, globs: &[&str]) {
        let globs = globs
            .iter()
            .map(|glob| glob.to_string())
            .collect::<Vec<_>>();
        cx.update(|_, cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.project.worktree.private_files = Some(globs.into());
                });
            });
        });
        cx.executor().run_until_parked();
    }

    #[gpui::test]
    async fn test_headers_reached_through_a_link_are_not_read(cx: &mut TestAppContext) {
        let mut cx = c_context(false, cx).await;
        set_private_files(&mut cx, &["keys/**"]);
        insert_files(
            &mut cx,
            &[
                ("keys/secret.h", "struct hidden { int key; };\n"),
                ("dir/open.h", "struct shown { int value; };\n"),
            ],
        )
        .await;
        let fs = cx.update_workspace(|workspace, _, cx| workspace.project().read(cx).fs().clone());
        let root = EditorLspTestContext::root_path();
        fs.as_fake()
            .insert_symlink(root.join("dir/link.h"), root.join("keys/secret.h"))
            .await;
        fs.as_fake()
            .insert_symlink(root.join("dir/inc"), root.join("keys"))
            .await;
        cx.executor().run_until_parked();
        ignore_dir(&mut cx).await;
        cx.set_state(
            "ˇ#include \"link.h\"\n#include \"inc/secret.h\"\n#include \"open.h\"\n\
             struct hidden a;\nstruct shown b;\n",
        );

        hover_over(&mut cx, "shown", 0);
        assert!(section_text(&mut cx).contains("value | int"));
        hover_over(&mut cx, "hidden", 0);
        assert_eq!(
            scratch_files(&mut cx),
            ["dir/file.c", "dir/open.h"],
            "a link is not followed to a private file"
        );
        assert!(section(&mut cx).is_none());
    }

    #[gpui::test]
    async fn test_includes_leaving_the_worktree_or_in_angle_brackets_are_not_read(
        cx: &mut TestAppContext,
    ) {
        let mut cx = c_context(false, cx).await;
        insert_files(
            &mut cx,
            &[
                ("top.h", "struct top { int up; };\n"),
                ("dir/system.h", "struct system { int sys; };\n"),
            ],
        )
        .await;
        ignore_dir(&mut cx).await;
        let absolute = EditorLspTestContext::root_path()
            .join("top.h")
            .to_string_lossy()
            .replace('\\', "/");
        cx.set_state(&format!(
            "ˇ#include \"../../top.h\"\n#include \"{absolute}\"\n#include <system.h>\n\
             struct top a;\nstruct system b;\n"
        ));

        hover_over(&mut cx, "top a", 0);
        assert_eq!(scratch_files(&mut cx), ["dir/file.c"]);
        assert!(section(&mut cx).is_none());
        hover_over(&mut cx, "system b", 0);
        assert_eq!(scratch_files(&mut cx), ["dir/file.c"]);
        assert!(section(&mut cx).is_none());
    }

    #[gpui::test]
    async fn test_headers_in_folders_not_scanned_yet_are_read(cx: &mut TestAppContext) {
        let mut cx = c_context(false, cx).await;
        ignore_dir(&mut cx).await;
        insert_files(
            &mut cx,
            &[("dir/sub/deep.h", "struct deep { int level; };\n")],
        )
        .await;
        cx.set_state("ˇ#include \"sub/deep.h\"\nstruct deep a;\n");

        hover_over(&mut cx, "deep a", 0);
        assert_eq!(scratch_files(&mut cx), ["dir/file.c", "dir/sub/deep.h"]);
        assert!(section_text(&mut cx).contains("level | int"));
    }

    #[gpui::test]
    async fn test_a_hover_reads_at_most_its_share_of_headers(cx: &mut TestAppContext) {
        let mut cx = c_context(false, cx).await;
        let headers = (0..SCRATCH_MAX_FILES + 8)
            .map(|index| {
                (
                    format!("dir/h{index:02}.h"),
                    format!("struct s{index:02} {{ int member; }};\n"),
                )
            })
            .collect::<Vec<_>>();
        let limit = usize::try_from(SCRATCH_MAX_FILE_BYTES).expect("a small limit");
        let at_limit =
            format!("struct edge {{ int e; }};\n{}", " ".repeat(limit))[..limit].to_string();
        let over_limit = format!("struct over {{ int o; }};\n{}", " ".repeat(limit + 1));
        let mut files = vec![
            ("dir/edge.h", at_limit.as_str()),
            ("dir/over.h", &over_limit[..limit + 1]),
        ];
        files.extend(
            headers
                .iter()
                .map(|(path, text)| (path.as_str(), text.as_str())),
        );
        insert_files(&mut cx, &files).await;
        ignore_dir(&mut cx).await;

        cx.set_state("ˇ#include \"edge.h\"\n#include \"over.h\"\nstruct edge a;\nstruct over b;\n");
        hover_over(&mut cx, "edge a", 0);
        assert_eq!(
            scratch_files(&mut cx),
            ["dir/edge.h", "dir/file.c"],
            "a header of the limit's size is read, a larger one is not"
        );
        assert!(section_text(&mut cx).contains("e | int"));

        let includes = headers
            .iter()
            .map(|(path, _)| format!("#include \"{}\"\n", path.trim_start_matches("dir/")))
            .collect::<String>();
        cx.set_state(&format!("ˇ{includes}struct s00 a;\n"));
        hover_over(&mut cx, "s00", 0);
        let read = scratch_files(&mut cx);
        assert_eq!(read.len(), SCRATCH_MAX_FILES, "{read:?}");
        assert!(read.contains(&"dir/h30.h".to_string()), "{read:?}");
        assert!(!read.contains(&"dir/h31.h".to_string()), "{read:?}");
    }

    #[gpui::test]
    async fn test_what_turned_private_after_the_hover_is_not_expanded(cx: &mut TestAppContext) {
        let mut cx = c_context(false, cx).await;
        insert_files(&mut cx, &[("dir/nested.h", NESTED_H)]).await;
        ignore_dir(&mut cx).await;
        cx.set_state(&format!("ˇ{NESTED_C}"));

        hover_over(&mut cx, "value", 0);
        assert!(section_text(&mut cx).contains("in | struct inner"));
        set_private_files(&mut cx, &["**/nested.h"]);
        toggle(&mut cx, "in");
        let text = section_text(&mut cx);
        assert!(!text.contains("depth | int"), "{text}");
    }

    #[gpui::test]
    async fn test_headers_the_folder_index_has_are_expanded_from_it(cx: &mut TestAppContext) {
        const OUTER_H: &str = "struct inner;
typedef struct { struct inner in; int flags; } Outer;
";
        const MAIN_C: &str = "#include \"outer.h\"
int use(void) { Outer value; return value.flags; }
";
        let mut cx = c_context(false, cx).await;
        cx.set_state(&format!("ˇ{MAIN_C}"));
        // `inner.h` is in the folder's index but not included: only that index finds `inner`.
        let _index = install_index(
            &mut cx,
            ("dir/file.c", MAIN_C),
            &[
                ("dir/outer.h", OUTER_H),
                (
                    "dir/inner.h",
                    "struct inner { int depth; char tag; };
",
                ),
            ],
        )
        .await;
        let fs = cx.update_workspace(|workspace, _, cx| workspace.project().read(cx).fs().clone());
        fs.as_fake()
            .insert_file(
                EditorLspTestContext::root_path().join(".gitignore"),
                b"dir/file.c
"
                .to_vec(),
            )
            .await;
        cx.executor().run_until_parked();

        hover_over(&mut cx, "value", 0);
        assert_eq!(scratch_files(&mut cx), ["dir/file.c", "dir/outer.h"]);
        let text = section_text(&mut cx);
        assert!(text.contains("in | struct inner"), "{text}");
        toggle(&mut cx, "in");
        let text = section_text(&mut cx);
        assert!(
            text.contains("depth | int"),
            "a header the folder's index has is expanded from it: {text}"
        );
    }

    #[gpui::test]
    async fn test_a_slow_language_server_does_not_hold_the_section_back(cx: &mut TestAppContext) {
        let (mut cx, _index) = setup(true, cx).await;
        let (answer, answered) = futures::channel::oneshot::channel::<()>();
        let answered = std::sync::Arc::new(std::sync::Mutex::new(Some(answered)));
        cx.set_request_handler::<lsp::request::HoverRequest, _, _>(move |_, _, _| {
            let answered = answered.lock().ok().and_then(|mut slot| slot.take());
            async move {
                if let Some(answered) = answered {
                    answered.await.expect("the test lets the answer through");
                }
                Ok(Some(lsp::Hover {
                    contents: lsp::HoverContents::Markup(lsp::MarkupContent {
                        kind: lsp::MarkupKind::Markdown,
                        value: "from the language server".to_string(),
                    }),
                    range: None,
                }))
            }
        });

        hover_over(&mut cx, "local", 1);
        assert!(
            section(&mut cx).is_none(),
            "a language server that answers soon is waited for"
        );
        cx.executor().advance_clock(LANGUAGE_SERVER_WAIT);
        cx.run_until_parked();
        assert!(
            section(&mut cx).is_some(),
            "the section does not wait longer"
        );
        assert!(notes(&mut cx).is_empty());

        answer.send(()).expect("the request is still waiting");
        cx.run_until_parked();
        let found = notes(&mut cx);
        assert_eq!(found, ["from the language server"]);
        let first_is_the_language_server = cx.editor(|editor, _, _| {
            editor
                .hover_state
                .info_popovers
                .first()
                .is_some_and(|popover| popover.type_contents.is_none())
        });
        assert!(first_is_the_language_server, "its answer comes first");
        assert!(section(&mut cx).is_some());
    }

    #[gpui::test]
    async fn test_a_file_opened_on_its_own_shows_its_types(cx: &mut TestAppContext) {
        let mut cx = c_context(false, cx).await;
        let fs = cx.update_workspace(|workspace, _, cx| workspace.project().read(cx).fs().clone());
        let solo = std::path::PathBuf::from(util::path!("/solo/solo.c"));
        fs.create_dir(std::path::Path::new(util::path!("/solo")))
            .await
            .expect("creating the folder");
        fs.as_fake()
            .insert_file(
                &solo,
                "typedef struct { int x; int y; } Point;\nint use(void) { Point p; return p.x; }\n"
                    .as_bytes()
                    .to_vec(),
            )
            .await;
        let open = cx.update_workspace(|workspace, window, cx| {
            workspace.open_abs_path(solo.clone(), workspace::OpenOptions::default(), window, cx)
        });
        cx.executor().run_until_parked();
        let editor = open
            .await
            .expect("opening the file")
            .downcast::<Editor>()
            .expect("an editor");
        cx.executor().run_until_parked();
        let is_single_file = cx.update(|_, cx| {
            let buffer = editor.read(cx).buffer().read(cx).as_singleton();
            let file = buffer.and_then(|buffer| buffer.read(cx).file().cloned());
            file.is_some_and(|file| file.path().is_empty())
        });
        assert!(is_single_file, "the file is a worktree of its own");

        let anchor = cx.update(|_, cx| {
            let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
            let offset = snapshot
                .text()
                .find("Point p")
                .expect("the variable's type");
            snapshot.anchor_before(MultiBufferOffset(offset))
        });
        editor.update_in(&mut cx.cx.cx, |editor, window, cx| {
            hover_at(editor, Some(anchor), None, window, cx)
        });
        let delay = hover_delay(&mut cx);
        cx.executor()
            .advance_clock(Duration::from_millis(delay + 100));
        cx.run_until_parked();
        let text = cx.update(|_, cx| {
            editor
                .read(cx)
                .hover_state
                .info_popovers
                .iter()
                .find_map(|popover| popover.type_contents.clone())
                .map(|section| section.read(cx).text_for_tests())
        });
        let text = text.expect("a type contents section");
        assert!(text.contains("solo.c:1"), "{text}");
        assert!(text.contains("x | int"), "{text}");
        assert!(text.contains("y | int"), "{text}");
    }

    fn set_index_status(cx: &mut EditorLspTestContext, status: SymbolIndexStatus) {
        let project = cx.update_workspace(|workspace, _, _| workspace.project().clone());
        cx.update(|_, cx| {
            let worktree_id = project
                .read(cx)
                .worktrees(cx)
                .next()
                .expect("a worktree")
                .read(cx)
                .id();
            project.update(cx, |project, cx| {
                project.symbol_index_store().update(cx, |store, cx| {
                    store.set_status_for_tests(worktree_id, status, cx)
                })
            });
        });
    }

    #[gpui::test]
    async fn test_an_index_that_cannot_answer_says_why(cx: &mut TestAppContext) {
        let (mut cx, _index) = setup(false, cx).await;
        cx.set_state(&format!("ˇ{FILE_C}static struct absent *lost;\n"));

        hover_over(&mut cx, "lost", 0);
        assert!(
            notes(&mut cx).is_empty(),
            "a ready index has nothing to explain"
        );

        set_index_status(&mut cx, SymbolIndexStatus::Indexing { done: 1, total: 4 });
        cx.update_editor(|editor, _, cx| hide_hover(editor, cx));
        hover_over(&mut cx, "lost", 0);
        let found = notes(&mut cx);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("still being built"), "{found:?}");
        hover_over(&mut cx, "use", 0);
        assert!(
            notes(&mut cx).is_empty(),
            "a function's name gets no note while the index is built"
        );
        hover_over(&mut cx, "local", 1);
        assert!(
            section(&mut cx).is_some() && notes(&mut cx).is_empty(),
            "what the index already knows is shown while it is built"
        );

        set_index_status(
            &mut cx,
            SymbolIndexStatus::Unavailable {
                reason: SymbolIndexUnavailable::InUseByAnotherWindow,
            },
        );
        cx.update_editor(|editor, _, cx| hide_hover(editor, cx));
        hover_over(&mut cx, "lost", 0);
        let found = notes(&mut cx);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("another window"), "{found:?}");

        set_index_status(&mut cx, SymbolIndexStatus::Disabled);
        cx.update_editor(|editor, _, cx| hide_hover(editor, cx));
        hover_over(&mut cx, "lost", 0);
        assert!(
            notes(&mut cx).is_empty(),
            "an index turned off on purpose is not brought up on every hover"
        );
    }

    #[gpui::test]
    async fn test_other_languages_are_not_asked_about(cx: &mut TestAppContext) {
        init_test(cx, |_| {});
        cx.executor().allow_parking();
        let mut cx = EditorLspTestContext::new_rust(lsp::ServerCapabilities::default(), cx).await;
        cx.set_state("ˇfn main() { let local = 1; }\n");
        let _index = install_index(
            &mut cx,
            ("dir/file.rs", "fn main() { let local = 1; }\n"),
            &[("dir/types.h", TYPES_H)],
        )
        .await;
        let before = queries_for_tests();
        hover_over(&mut cx, "local", 0);
        assert_eq!(queries_for_tests(), before);
        assert!(section(&mut cx).is_none());
    }

    #[gpui::test]
    async fn test_the_setting_turns_the_section_off(cx: &mut TestAppContext) {
        let (mut cx, _index) = setup(false, cx).await;
        set_hover_type_contents(&mut cx, false);
        let before = queries_for_tests();
        hover_over(&mut cx, "alias_t", 0);
        assert!(section(&mut cx).is_none());
        assert_eq!(queries_for_tests(), before, "the index is not asked");

        set_hover_type_contents(&mut cx, true);
        hover_over(&mut cx, "local", 1);
        assert!(section(&mut cx).is_some());
    }

    #[gpui::test]
    async fn test_moving_on_drops_the_earlier_question(cx: &mut TestAppContext) {
        let (mut cx, _index) = setup(false, cx).await;
        let before = queries_for_tests();
        point_at(&mut cx, "local", 0);
        let delay = hover_delay(&mut cx);
        cx.executor()
            .advance_clock(Duration::from_millis(delay / 2 + 10));
        assert_eq!(queries_for_tests(), before + 1, "the index was asked");
        point_at(&mut cx, "shade", 0);
        cx.executor()
            .advance_clock(Duration::from_millis(delay + 100));
        cx.run_until_parked();
        assert_eq!(queries_for_tests(), before + 2);
        let text = section_text(&mut cx);
        assert!(text.contains("enum color"), "{text}");
        assert!(!text.contains("struct outer"), "{text}");
        let sections = cx.editor(|editor, _, _| {
            editor
                .hover_state
                .info_popovers
                .iter()
                .filter(|popover| popover.type_contents.is_some())
                .count()
        });
        assert_eq!(sections, 1);
    }

    #[gpui::test]
    async fn test_closing_the_popover_drops_the_section(cx: &mut TestAppContext) {
        let (mut cx, _index) = setup(false, cx).await;
        hover_over(&mut cx, "alias_t", 0);
        let section = section(&mut cx).expect("a section").downgrade();
        cx.update(|_, cx| {
            section
                .update(cx, |section, cx| {
                    let path = section.member_path_for_tests("in");
                    section.toggle_member(path, cx);
                })
                .ok();
        });
        cx.update_editor(|editor, _, cx| {
            hide_hover(editor, cx);
        });
        cx.run_until_parked();
        assert!(
            section.upgrade().is_none(),
            "nothing keeps it or its questions"
        );
    }

    #[gpui::test]
    async fn test_the_keyboard_hover_shows_the_section(cx: &mut TestAppContext) {
        let (mut cx, _index) = setup(false, cx).await;
        let offset = FILE_C.find("local.").expect("local.");
        cx.set_state(&format!("{}ˇ{}", &FILE_C[..offset], &FILE_C[offset..]));
        cx.update_editor(|editor, window, cx| hover(editor, &Hover, window, cx));
        cx.run_until_parked();
        assert!(section_text(&mut cx).starts_with("Type of local: struct outer"));
    }

    #[gpui::test]
    async fn test_rows_jump_to_their_definitions(cx: &mut TestAppContext) {
        let (mut cx, _index) = setup(false, cx).await;
        hover_over(&mut cx, "alias_t", 0);
        let section = section(&mut cx).expect("a section");
        let editor = cx.editor(|_, _, cx| cx.entity().downgrade());
        let project = cx.update_workspace(|workspace, _, _| workspace.project().downgrade());
        for (row, expected_row, expected_column) in [("struct outer", 4, 15), ("flags", 8, 13)] {
            let jump = cx.update(|_, cx| section.read(cx).jump_for_tests(row));
            cx.update(|window, cx| open_location(&editor, &project, jump, window, cx));
            cx.run_until_parked();
            let (path, position) = cx.update_workspace(|workspace, _, cx| {
                let active = workspace
                    .active_item_as::<Editor>(cx)
                    .expect("an active editor");
                let active = active.read(cx);
                let buffer = active
                    .buffer()
                    .read(cx)
                    .as_singleton()
                    .expect("a singleton buffer");
                let path = buffer
                    .read(cx)
                    .file()
                    .expect("a file")
                    .path()
                    .as_unix_str()
                    .to_string();
                let snapshot = active.buffer().read(cx).snapshot(cx);
                let head = active.selections.newest_anchor().head().to_point(&snapshot);
                (path, head)
            });
            assert_eq!(path, "dir/types.h");
            assert_eq!(
                (position.row, position.column),
                (expected_row, expected_column),
                "{row}"
            );
        }
    }

    #[gpui::test]
    async fn test_the_section_is_translated(cx: &mut TestAppContext) {
        let (mut cx, _index) = setup(false, cx).await;
        hover_over(&mut cx, "dup", 0);
        let _locale = i18n::test_support::LocaleOverride::new(Some(
            i18n_catalog::test_support::bundled_catalog("ja") as &'static dyn i18n::Catalog,
        ));
        let text = section_text(&mut cx);
        assert_eq!(
            text,
            "struct dup [名前一致] dir/file.c:2 (3 件の定義のうち 1 件目)\n  in_main | int"
        );
        hover_over(&mut cx, "local", 1);
        let text = section_text(&mut cx);
        assert!(
            text.starts_with("local の型: struct outer (この関数の中の宣言)"),
            "{text}"
        );
    }

    #[test]
    fn test_the_type_is_read_from_a_language_server_hover() {
        let hover = |kind: HoverBlockKind, text: &str| project::Hover {
            contents: vec![project::HoverBlock {
                text: text.to_string(),
                kind,
            }],
            range: None,
            language: None,
        };
        assert_eq!(
            language_server_type_text(&[hover(
                HoverBlockKind::Markdown,
                "### field `ops`\n\n---\nType: `const struct file_operations *` (aka `x`)\n\n```cpp\n// In struct inode\nconst struct file_operations *ops\n```",
            )]),
            Some("const struct file_operations *".to_string())
        );
        assert_eq!(
            language_server_type_text(&[hover(
                HoverBlockKind::PlainText,
                "variable ctx\nType: struct my_ctx * (aka struct my_ctx *)",
            )]),
            Some("struct my_ctx *".to_string())
        );
        assert_eq!(
            language_server_type_text(&[hover(
                HoverBlockKind::Markdown,
                "### variable `ctx`\n\n```cpp\n// In main\nstruct my_ctx *ctx = make();\n```",
            )]),
            Some("struct my_ctx *ctx = make()".to_string())
        );
        assert_eq!(
            language_server_type_text(&[hover(HoverBlockKind::Markdown, "just docs")]),
            None
        );
    }

    const RECURSION_C: &str = "\
struct tree { struct branch *branch; };
struct ring { struct link *link; };
static struct tree *tree_root;
static struct ring *ring_head;
";

    async fn recursion_setup(cx: &mut TestAppContext) -> (EditorLspTestContext, tempfile::TempDir) {
        let mut cx = c_context(false, cx).await;
        cx.set_state(&format!("ˇ{RECURSION_C}"));
        // Expanding a member reads its type's file from the disk, and these types are in the
        // edited file.
        let fs = cx.update_workspace(|workspace, _, cx| workspace.project().read(cx).fs().clone());
        fs.as_fake()
            .insert_file(
                EditorLspTestContext::root_path().join("dir/file.c"),
                RECURSION_C.as_bytes().to_vec(),
            )
            .await;
        let index = install_index(
            &mut cx,
            ("dir/file.c", RECURSION_C),
            &[
                ("dir/branch.h", "struct branch { struct tree *up; };\n"),
                (
                    "dir/link.h",
                    "struct ring { int other_ring; };\nstruct link { struct ring *back; };\n",
                ),
            ],
        )
        .await;
        (cx, index)
    }

    #[gpui::test]
    async fn test_recursion_is_told_by_definition_not_by_name(cx: &mut TestAppContext) {
        let (mut cx, _index) = recursion_setup(cx).await;

        // `struct tree` in another file is the same definition: found once expanded.
        hover_over(&mut cx, "tree *tree_root", 0);
        toggle(&mut cx, "branch");
        let text = section_text(&mut cx);
        assert!(text.contains("  ▸ up | struct tree *"), "{text}");
        toggle(&mut cx, "up");
        let text = section_text(&mut cx);
        assert!(
            text.contains("  ↺ up | struct tree * (recursive)"),
            "{text}"
        );
        toggle(&mut cx, "up");
        assert_eq!(section_text(&mut cx), text, "nothing to expand");

        // `struct ring` in `link.h` is another type of the same name.
        hover_over(&mut cx, "ring *ring_head", 0);
        toggle(&mut cx, "link");
        toggle(&mut cx, "back");
        let text = section_text(&mut cx);
        assert!(
            text.contains("  ▾ back | struct ring *\n    struct ring dir/link.h:1"),
            "{text}"
        );
        assert!(text.contains("      other_ring | int"), "{text}");
    }

    #[gpui::test]
    async fn test_switching_definitions_closes_what_was_opened(cx: &mut TestAppContext) {
        let (mut cx, _index) = recursion_setup(cx).await;
        hover_over(&mut cx, "ring *ring_head", 0);
        toggle(&mut cx, "link");
        let opened = section_text(&mut cx);
        assert!(opened.contains("▾ link | struct link *"), "{opened}");
        let section = section(&mut cx).expect("a section");
        for _ in 0..2 {
            cx.update(|_, cx| {
                section.update(cx, |section, cx| {
                    let path = section.view_path_for_tests("struct ring");
                    section.select_candidate(path, true, cx);
                })
            });
            cx.run_until_parked();
        }
        let text = section_text(&mut cx);
        assert!(text.contains("dir/file.c:2"), "back to the first: {text}");
        assert!(text.contains("▸ link | struct link *"), "{text}");
        assert!(!text.contains("back | struct ring"), "{text}");
    }

    #[gpui::test]
    async fn test_a_member_whose_type_went_away_is_not_found(cx: &mut TestAppContext) {
        let (mut cx, _index) = setup(false, cx).await;
        hover_over(&mut cx, "alias_t", 0);
        let fs = cx.update_workspace(|workspace, _, cx| workspace.project().read(cx).fs().clone());
        fs.as_fake()
            .insert_file(
                EditorLspTestContext::root_path().join("dir/types.h"),
                b"struct unrelated { int value; };\n".to_vec(),
            )
            .await;
        cx.run_until_parked();
        toggle(&mut cx, "in");
        let text = section_text(&mut cx);
        assert!(
            text.contains("▾ in | struct inner // the inner part\n    Definition not found"),
            "{text}"
        );
    }

    #[gpui::test]
    async fn test_more_rows_come_a_page_at_a_time(cx: &mut TestAppContext) {
        let (mut cx, _index) = setup(false, cx).await;
        let members = (0..250)
            .map(|index| format!("    int field_{index};\n"))
            .collect::<String>();
        let huge = format!("struct huge {{\n{members}}};\n");
        let edited = format!("ˇ{FILE_C}{huge}struct huge *huge_value;\n");
        cx.set_state(&edited);
        hover_over(&mut cx, "huge *huge_value", 0);
        let section = section(&mut cx).expect("a section");
        let show_more = |cx: &mut EditorLspTestContext| {
            cx.update(|_, cx| {
                section.update(cx, |section, cx| {
                    let path = section.view_path_for_tests("struct huge");
                    section.show_more_rows(path, 0, cx);
                })
            });
        };
        let text = section_text(&mut cx);
        assert!(text.ends_with("220 more"), "{text}");
        show_more(&mut cx);
        let text = section_text(&mut cx);
        assert!(text.contains("field_129 | int"), "{text}");
        assert!(!text.contains("field_130"), "{text}");
        assert!(text.ends_with("120 more"), "{text}");
        show_more(&mut cx);
        show_more(&mut cx);
        let text = section_text(&mut cx);
        assert!(text.ends_with("field_249 | int"), "{text}");
    }

    #[gpui::test]
    async fn test_copying_puts_the_shown_definition_on_the_clipboard(cx: &mut TestAppContext) {
        let (mut cx, _index) = setup(false, cx).await;
        hover_over(&mut cx, "alias_t", 0);
        let outer = section(&mut cx).expect("a section");
        cx.update(|_, cx| outer.update(cx, |section, cx| section.copy_type_contents(cx)));
        cx.run_until_parked();
        let copied = cx
            .update(|_, cx| cx.read_from_clipboard())
            .and_then(|item| item.text())
            .expect("copied text");
        for expected in ["struct outer", "in", "depth", "callback", "flags"] {
            assert!(copied.contains(expected), "{expected:?} not in\n{copied}");
        }

        hover_over(&mut cx, "dup", 0);
        let dup = section(&mut cx).expect("a section");
        cx.update(|_, cx| {
            dup.update(cx, |section, cx| {
                let path = section.view_path_for_tests("struct dup");
                section.select_candidate(path, true, cx);
            })
        });
        cx.run_until_parked();
        let shown = section_text(&mut cx);
        cx.update(|_, cx| dup.update(cx, |section, cx| section.copy_type_contents(cx)));
        cx.run_until_parked();
        let copied = cx
            .update(|_, cx| cx.read_from_clipboard())
            .and_then(|item| item.text())
            .expect("copied text");
        let shown_member = ["in_other", "in_header"]
            .into_iter()
            .find(|member| shown.contains(member))
            .unwrap_or_else(|| panic!("a second definition in\n{shown}"));
        assert!(copied.contains(shown_member), "{shown}\n---\n{copied}");
        assert!(!copied.contains("in_main"), "{copied}");
    }

    #[test]
    fn test_locations_name_the_worktree_when_there_are_several() {
        let path = ProjectPath {
            worktree_id: WorktreeId::from_usize(1),
            path: util::rel_path::rel_path("include/linux/fs.h").into_arc(),
        };
        let single = PathLabels {
            path_style: PathStyle::Unix,
            root_names: None,
            single_files: HashMap::default(),
        };
        assert_eq!(
            location_label(&path, Point::new(9, 4), &single),
            ("include/linux/".to_string(), "fs.h:10".to_string())
        );
        let several = PathLabels {
            path_style: PathStyle::Windows,
            root_names: Some(HashMap::from_iter([
                (WorktreeId::from_usize(1), "kernel".to_string()),
                (WorktreeId::from_usize(2), "tools".to_string()),
            ])),
            single_files: HashMap::default(),
        };
        assert_eq!(
            location_label(&path, Point::new(0, 0), &several),
            ("kernel\\include\\linux\\".to_string(), "fs.h:1".to_string())
        );
    }

    /// The language server's popover and the section, as drawn: nearest the line first.
    fn drawn_popovers(cx: &mut EditorLspTestContext) -> Vec<gpui::Bounds<Pixels>> {
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        cx.editor(|editor, _, _| {
            editor
                .hover_state
                .info_popovers
                .iter()
                .map(|popover| popover.last_bounds.get().expect("the popover is drawn"))
                .collect()
        })
    }

    fn answer_hovers_briefly(cx: &mut EditorLspTestContext) {
        cx.set_request_handler::<lsp::request::HoverRequest, _, _>(|_, _, _| async move {
            Ok(Some(lsp::Hover {
                contents: lsp::HoverContents::Markup(lsp::MarkupContent {
                    kind: lsp::MarkupKind::Markdown,
                    value: "### variable `local`".to_string(),
                }),
                range: None,
            }))
        });
    }

    #[gpui::test]
    async fn test_the_gap_between_stacked_popovers_keeps_them_open(cx: &mut TestAppContext) {
        let (mut cx, _index) = setup(true, cx).await;
        answer_hovers_briefly(&mut cx);
        hover_over(&mut cx, "local", 1);
        let mut popovers = drawn_popovers(&mut cx);
        assert_eq!(popovers.len(), 2, "the language server's and the section");
        popovers.sort_by(|left, right| {
            left.top()
                .partial_cmp(&right.top())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let (upper, lower) = (popovers[0], popovers[1]);
        let gap = lower.top() - upper.bottom();
        assert!(
            gap >= HOVER_POPOVER_GAP && gap <= HOVER_POPOVER_GAP + px(4.),
            "stacked: {popovers:?}"
        );
        // Beside the narrower popover, where nothing covers the gap.
        let in_gap = gpui::point(
            upper.right().max(lower.right()) - px(2.),
            upper.bottom() + HOVER_POPOVER_GAP / 2.,
        );
        let hiding_delay =
            cx.update(|_, cx| EditorSettings::get_global(cx).hover_popover_hiding_delay.0);
        let elsewhere = anchor_of(&mut cx, "shade", 0);

        let assert_still_shown = |cx: &mut EditorLspTestContext, why: &str| {
            let shown = cx.editor(|editor, _, _| editor.hover_state.info_popovers.len());
            assert_eq!(shown, 2, "{why}");
            let text = section_text(cx);
            assert!(text.starts_with("Type of local:"), "{why}: {text}");
        };

        // Past the end of a line: the pointer is no longer over text.
        cx.update_editor(|editor, window, cx| hover_at(editor, None, Some(in_gap), window, cx));
        cx.executor()
            .advance_clock(Duration::from_millis(hiding_delay + 100));
        cx.run_until_parked();
        assert_still_shown(&mut cx, "the gap does not start hiding the popovers");

        // Over other text.
        cx.update_editor(|editor, window, cx| {
            hover_at(editor, Some(elsewhere), Some(in_gap), window, cx)
        });
        assert_still_shown(&mut cx, "the gap does not hover the text underneath");
        let delay = hover_delay(&mut cx);
        cx.executor()
            .advance_clock(Duration::from_millis(delay.max(hiding_delay) + 100));
        cx.run_until_parked();
        assert_still_shown(&mut cx, "nor later");

        let below = gpui::point(in_gap.x, lower.bottom() + HOVER_POPOVER_GAP * 3.);
        cx.update_editor(|editor, window, cx| hover_at(editor, None, Some(below), window, cx));
        cx.executor()
            .advance_clock(Duration::from_millis(hiding_delay + 100));
        cx.run_until_parked();
        let shown = cx.editor(|editor, _, _| editor.hover_state.visible());
        assert!(!shown, "leaving the stack still hides it");
    }

    /// Hovers the `occurrence`-th `needle`, draws the language server's popover alone, and then
    /// with the section arriving: the popover alone, then the language server's and the section.
    fn draw_section_arriving_later(
        cx: &mut EditorLspTestContext,
        needle: &str,
        occurrence: usize,
    ) -> (
        gpui::Bounds<Pixels>,
        gpui::Bounds<Pixels>,
        gpui::Bounds<Pixels>,
    ) {
        answer_hovers_briefly(cx);
        hover_over(cx, needle, occurrence);
        let section = cx.update_editor(|editor, _, _| {
            let section = editor.hover_state.info_popovers.pop();
            editor.hover_state.kept_stack_layout = None;
            section.expect("a section")
        });
        let alone = drawn_popovers(cx);
        let [alone] = alone.as_slice() else {
            panic!("one popover: {alone:?}");
        };
        let alone = *alone;
        cx.update_editor(|editor, _, cx| {
            editor.hover_state.info_popovers.push(section);
            cx.notify();
        });
        let both = drawn_popovers(cx);
        let [first, section] = both.as_slice() else {
            panic!("two popovers: {both:?}");
        };
        (alone, *first, *section)
    }

    #[gpui::test]
    async fn test_a_stack_with_a_section_goes_below_the_line_where_it_fits(
        cx: &mut TestAppContext,
    ) {
        // Room above the line for the whole stack, too.
        let edited = format!("{}{FILE_C}", "\n".repeat(12));
        let mut cx = c_context(true, cx).await;
        cx.set_state(&format!("ˇ{edited}"));
        let _index = install_index(
            &mut cx,
            ("dir/file.c", edited.as_str()),
            &[("dir/types.h", TYPES_H)],
        )
        .await;

        // Without a section, the popover goes above as upstream places it.
        let (alone, first, section) = draw_section_arriving_later(&mut cx, "shade", 0);
        assert!(
            first.top() > alone.bottom() && section.top() > first.bottom(),
            "with the section, the stack goes below: {first:?} and {section:?} after {alone:?}"
        );
    }

    #[gpui::test]
    async fn test_expanding_a_member_keeps_the_stack_where_it_is(cx: &mut TestAppContext) {
        let (mut cx, _index) = setup(true, cx).await;
        answer_hovers_briefly(&mut cx);
        hover_over(&mut cx, "local", 1);
        let before = drawn_popovers(&mut cx);
        let [first, section] = before.as_slice() else {
            panic!("two popovers: {before:?}");
        };
        let (first, section) = (*first, *section);
        assert!(section.top() > first.bottom(), "below the line: {before:?}");

        toggle(&mut cx, "in");
        let after = drawn_popovers(&mut cx);
        let [first_after, section_after] = after.as_slice() else {
            panic!("two popovers: {after:?}");
        };
        assert!(
            section_after.size.height > section.size.height,
            "expanded: {section:?} to {section_after:?}"
        );
        assert_eq!(
            (first_after.origin, section_after.origin),
            (first.origin, section.origin),
            "the popovers stay where they were"
        );

        // A stack shown above the line keeps its top as it grows, rather than its bottom.
        toggle(&mut cx, "in");
        drawn_popovers(&mut cx);
        cx.update_editor(|editor, _, cx| {
            let kept = editor
                .hover_state
                .kept_stack_layout
                .as_mut()
                .expect("the stack's layout is kept");
            kept.above = true;
            cx.notify();
        });
        let above = drawn_popovers(&mut cx);
        let [first_above, section_above] = above.as_slice() else {
            panic!("two popovers: {above:?}");
        };
        assert!(
            section_above.bottom() < first_above.top(),
            "above the line, the popover nearest the line is the lowest: {above:?}"
        );
        let top = section_above.top();
        toggle(&mut cx, "in");
        let grown = drawn_popovers(&mut cx);
        let [first_grown, section_grown] = grown.as_slice() else {
            panic!("two popovers: {grown:?}");
        };
        assert_eq!(section_grown.top(), top, "{grown:?} after {above:?}");
        assert!(
            first_grown.top() > first_above.top(),
            "the stack grows downward: {grown:?} after {above:?}"
        );

        // With no room left below, the stack is drawn shorter rather than moved up.
        toggle(&mut cx, "in");
        drawn_popovers(&mut cx);
        let bottom = cx.editor(|editor, _, _| {
            editor
                .last_bounds()
                .expect("the editor is drawn")
                .bottom()
        });
        let stack = drawn_popovers(&mut cx);
        let stack_top = stack.iter().map(|bounds| bounds.top()).fold(bottom, Pixels::min);
        let stack_bottom = stack
            .iter()
            .map(|bounds| bounds.bottom())
            .fold(Pixels::ZERO, Pixels::max);
        let room = px(4.);
        let lowered = bottom - room - stack_bottom;
        cx.update_editor(|editor, _, cx| {
            let kept = editor
                .hover_state
                .kept_stack_layout
                .as_mut()
                .expect("the stack's layout is kept");
            kept.offset.y += lowered;
            cx.notify();
        });
        let lowered_stack = drawn_popovers(&mut cx);
        let lowered_top = lowered_stack
            .iter()
            .map(|bounds| bounds.top())
            .fold(bottom, Pixels::min);
        assert_eq!(lowered_top, stack_top + lowered, "{lowered_stack:?}");
        toggle(&mut cx, "in");
        let squeezed = drawn_popovers(&mut cx);
        let squeezed_top = squeezed
            .iter()
            .map(|bounds| bounds.top())
            .fold(bottom, Pixels::min);
        assert_eq!(squeezed_top, lowered_top, "the top stays: {squeezed:?}");
        assert!(
            squeezed.iter().all(|bounds| bounds.bottom() <= bottom),
            "shorter, within the editor: {squeezed:?} in {bottom:?}"
        );
    }

    #[gpui::test]
    async fn test_a_section_arriving_later_moves_the_whole_stack_when_it_does_not_fit(
        cx: &mut TestAppContext,
    ) {
        let (mut cx, _index) = setup(true, cx).await;
        let line_height = cx.update(|window, _| window.line_height());

        // A tall section does not fit above with the popover shown there: the whole stack moves
        // below, rather than being split by the line.
        let (alone, first, section) = draw_section_arriving_later(&mut cx, "local", 1);
        assert!(
            first.top() >= alone.bottom() + line_height && section.top() > first.bottom(),
            "the whole stack goes to the other side of the line: {first:?} and {section:?} \
             after {alone:?}"
        );
    }

    #[gpui::test]
    async fn test_a_stack_shown_again_is_placed_afresh(cx: &mut TestAppContext) {
        let (mut cx, _index) = setup(true, cx).await;
        answer_hovers_briefly(&mut cx);
        hover_over(&mut cx, "local", 1);
        cx.update_editor(|editor, _, _| {
            editor.hover_state.info_popovers.pop();
            editor.hover_state.kept_stack_layout = None;
        });
        let alone = drawn_popovers(&mut cx);
        let [alone] = alone.as_slice() else {
            panic!("one popover: {alone:?}");
        };

        cx.update_editor(|editor, _, cx| hide_hover(editor, cx));
        hover_over(&mut cx, "local", 1);
        let both = drawn_popovers(&mut cx);
        let [first, section] = both.as_slice() else {
            panic!("two popovers: {both:?}");
        };
        assert!(
            first.top() > alone.bottom() && section.top() > first.bottom(),
            "the side the hidden stack had is forgotten, and the whole new one goes below: \
             {first:?} and {section:?} after {alone:?}"
        );
    }

    #[gpui::test]
    async fn test_a_stack_without_a_section_is_placed_as_upstream_places_it(
        cx: &mut TestAppContext,
    ) {
        let (mut cx, _index) = setup(true, cx).await;
        answer_hovers_briefly(&mut cx);
        hover_over(&mut cx, "local", 1);
        let mut second = cx.update_editor(|editor, _, _| {
            let section = editor.hover_state.info_popovers.pop();
            editor.hover_state.kept_stack_layout = None;
            section.expect("a section")
        });
        let alone = drawn_popovers(&mut cx);
        let [alone] = alone.as_slice() else {
            panic!("one popover: {alone:?}");
        };
        // A language server's popover, as tall as the section, grown after the first was shown.
        let tall = (0..60)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n\n");
        let markdown = cx.update(|_, cx| cx.new(|cx| Markdown::new(tall.into(), None, None, cx)));
        second.type_contents = None;
        second.parsed_content = Some(markdown);
        second.last_bounds.set(None);
        cx.update_editor(|editor, _, cx| {
            editor.hover_state.info_popovers.push(second);
            cx.notify();
        });
        drawn_popovers(&mut cx);
        let both = drawn_popovers(&mut cx);
        let [first, second] = both.as_slice() else {
            panic!("two popovers: {both:?}");
        };
        assert!(
            first.top() > alone.bottom() && second.top() > first.bottom(),
            "the whole stack goes below the line, as it always has: {first:?} and {second:?} \
             after {alone:?}"
        );
    }

    #[gpui::test]
    async fn test_the_grace_period_starts_when_the_language_server_answers(
        cx: &mut TestAppContext,
    ) {
        let (mut cx, _index) = setup(false, cx).await;
        let anchor = anchor_of(&mut cx, "local", 1);
        let query = cx
            .update_editor(|editor, _, cx| {
                let (position, _) = editor
                    .buffer()
                    .read(cx)
                    .snapshot(cx)
                    .anchor_to_buffer_anchor(anchor)?;
                let buffer = editor.buffer().read(cx).as_singleton()?;
                query(editor, &buffer, position, cx)
            })
            .expect("a question for the index");
        // The language server answers long after the question was asked.
        cx.executor().advance_clock(GRACE_PERIOD * 4);
        cx.run_until_parked();
        let async_cx = cx.update(|_, cx| cx.to_async());
        let PendingTypeContents {
            grace_period,
            future: _,
        } = query.finish(&[], false, &async_cx);
        let waiting = cx.foreground_executor().spawn(grace_period);
        cx.run_until_parked();
        assert!(!waiting.is_ready(), "the period starts with the answer");
        cx.executor()
            .advance_clock(GRACE_PERIOD - Duration::from_millis(10));
        cx.run_until_parked();
        assert!(!waiting.is_ready());
        cx.executor().advance_clock(Duration::from_millis(20));
        cx.run_until_parked();
        assert!(waiting.is_ready());
    }

    #[test]
    fn test_columns_count_wide_characters_twice() {
        let member = |name: &str, type_text: &str| Line::Member {
            depth: 0,
            path: Vec::new(),
            name: name.to_string(),
            type_text: type_text.to_string(),
            bit_width: None,
            comment: None,
            condition: None,
            toggle: Toggle::None,
            unparsed: false,
            jump: (
                ProjectPath {
                    worktree_id: WorktreeId::from_usize(1),
                    path: util::rel_path::rel_path("a.h").into_arc(),
                },
                Point::zero(),
            ),
        };
        assert_eq!(column_widths(&[member("counter", "unsigned int")]), (7, 12));
        assert_eq!(
            column_widths(&[member("名前の長さ", "構造体 名前 *")]),
            (10, 13),
            "a wide character takes two columns"
        );
        assert_eq!(
            column_widths(&[member(&"幅".repeat(30), &"型".repeat(30))]),
            (NAME_COLUMN_CHARS, TYPE_COLUMN_CHARS)
        );
    }

    #[gpui::test]
    async fn test_a_slow_section_does_not_hold_the_popover_up(cx: &mut TestAppContext) {
        let pending = |future: LocalBoxFuture<'static, Option<Section>>,
                       cx: &mut TestAppContext| PendingTypeContents {
            future,
            grace_period: cx.executor().timer(GRACE_PERIOD),
        };

        let ready = pending(async { None }.boxed_local(), cx);
        let (now, later) = within_grace_period(Some(ready)).await;
        assert!(now.is_none() && later.is_none(), "answered in time");

        let (answer, answered) = futures::channel::oneshot::channel::<()>();
        let slow = pending(
            async move {
                answered.await.ok();
                None
            }
            .boxed_local(),
            cx,
        );
        let waiting = cx
            .foreground_executor()
            .spawn(within_grace_period(Some(slow)));
        cx.executor()
            .advance_clock(GRACE_PERIOD - Duration::from_millis(10));
        cx.run_until_parked();
        assert!(!waiting.is_ready(), "still within the grace period");
        cx.executor().advance_clock(Duration::from_millis(20));
        cx.run_until_parked();
        let (now, later) = waiting.await;
        assert!(now.is_none());
        let later = later.expect("added below later");
        answer.send(()).ok();
        assert!(later.await.is_none());
    }

    /// The heading of a definition with others of the same name stays one row: the popover is
    /// as narrow as its widest row allows, and a heading that may wrap between its parts put
    /// the name, the location, the switcher and the copy button on rows of their own.
    #[gpui::test]
    async fn test_the_heading_of_same_name_definitions_stays_on_one_row(cx: &mut TestAppContext) {
        let (mut cx, _index) = setup(false, cx).await;
        hover_over(&mut cx, "dup", 0);
        assert!(section_text(&mut cx).contains("(Definition 1 of 3)"));
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        let heading = cx
            .debug_bounds("type-contents-heading")
            .expect("the heading is drawn");
        let switcher = cx
            .debug_bounds("type-contents-switcher")
            .expect("the switcher is drawn");
        let line_height = cx.update(|window, _| window.line_height());
        assert!(
            heading.size.height < line_height * 2.,
            "the heading wrapped: {heading:?}"
        );
        assert!(
            switcher.top() < heading.top() + line_height,
            "the switcher is on the heading's row: {switcher:?} in {heading:?}"
        );
        assert_eq!(short_position_label(1, 3), "2 / 3");
        assert_eq!(definition_position_label(1, 3), "Definition 2 of 3");
    }
}
