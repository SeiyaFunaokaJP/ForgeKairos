pub use crate::commit_context_menu::{CopyCommitSha, CopyCommitTag, OpenCommitView};
use crate::{
    commit_context_menu::{CommitContextMenuData, CommitContextMenuSource, commit_context_menu},
    commit_tooltip::CommitAvatar,
    commit_view::CommitView,
    git_status_icon,
    git_terms::{self, help_aside},
    merge_request_box::{self, MergeRequestBox, MergeRequestCandidate, RowMark, RowMarks},
    rewrite_range_modal::{self, RewriteCommitRow, RewriteRangeModal},
};
use collections::{BTreeMap, BTreeSet, HashMap, HashSet, IndexSet};
use editor::Editor;
use file_icons::FileIcons;
use git::{
    BuildCommitPermalinkParams, GitHostingProviderRegistry, GitRemote, Oid, ParsedGitRemote,
    parse_git_remote_url,
    repository::{
        Branch, InitialGraphCommitData, LogOrder, LogSource, RepoPath, ResetMode, TagDistance,
        TagInfo, Worktree as GitWorktree,
    },
    status::{FileStatus, StatusCode, TrackedStatus},
};
use gpui::{
    Action, Anchor, AnyElement, App, Bounds, ClickEvent, ClipboardItem, DefiniteLength,
    DismissEvent, DragMoveEvent, ElementId, Empty, Entity, EventEmitter, FocusHandle, Focusable,
    Hsla, Modifiers, MouseButton, MouseDownEvent, PathBuilder, Pixels, Point, ScrollHandle,
    ScrollStrategy, ScrollWheelEvent, SharedString, Subscription, Task, UniformListScrollHandle,
    WeakEntity, Window, actions, anchored, deferred, point, prelude::*, px, uniform_list,
};
use language::line_diff;
use markdown::{Markdown, MarkdownElement};
use menu::{Cancel, SelectFirst, SelectLast, SelectNext, SelectPrevious};
use picker::{Picker, PickerDelegate};
use project::{
    ProjectPath,
    git_store::{
        CommitDataState, CommitDiff, CommitFile, GitGraphEvent, GitStore, GitStoreEvent,
        GraphDataResponse, Repository, RepositoryEvent, RepositoryId,
    },
};
use smallvec::{SmallVec, smallvec};
use std::{
    cell::Cell,
    ops::Range,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};
use zed_actions::{
    buffer_search,
    search::{SelectNextMatch, SelectPreviousMatch, ToggleCaseSensitive},
};

use notifications::status_toast::StatusToast;
use theme::AccentColors;
use time::{OffsetDateTime, UtcOffset};
use ui::{
    Chip, ColumnWidthConfig, CommonAnimationExt as _, ContextMenu, ContextMenuEntry, DiffStat,
    Disclosure, Divider, DocumentationSide, HeaderResizeInfo, HighlightedLabel, IndentGuideColors,
    ListItem, ListItemSpacing, PopoverMenu, RedistributableColumnsState, ScrollableHandle, Table,
    TableInteractionState, TableRenderContext, TableResizeBehavior, Tooltip, WithScrollbar,
    bind_redistributable_columns, prelude::*, redistribute_hidden_fractions,
    redistribute_hidden_widths, render_redistributable_columns_resize_handles, render_table_header,
    table_row::TableRow,
};
use util::{ResultExt, debug_panic};
use workspace::{
    ItemNavHistory, ModalView, Workspace,
    item::{Item, ItemEvent, TabTooltipContent},
};

mod action_bar;
mod graph_search;

const COMMIT_CIRCLE_RADIUS: Pixels = px(4.5);
const COMMIT_CIRCLE_STROKE_WIDTH: Pixels = px(1.5);
const COMMIT_CIRCLE_HALO_WIDTH: Pixels = px(2.0);
const LANE_WIDTH: Pixels = px(18.0);
const LEFT_PADDING: Pixels = px(12.0);
const LINE_WIDTH: Pixels = px(2.0);
const RESIZE_HANDLE_WIDTH: f32 = 8.0;
const COPIED_STATE_DURATION: Duration = Duration::from_secs(2);
const COMMIT_TAG_LIST_WIDTH_IN_REMS: Rems = rems(10.);
const TREE_INDENT: f32 = 20.0;
const TABLE_COLUMN_COUNT: usize = 4;
const ROW_VERTICAL_PADDING: Pixels = px(4.0);

struct CopiedState {
    copied_at: Option<Instant>,
}

impl CopiedState {
    fn new(_window: &mut Window, _cx: &mut Context<Self>) -> Self {
        Self { copied_at: None }
    }

    fn is_copied(&self) -> bool {
        self.copied_at
            .map(|t| t.elapsed() < COPIED_STATE_DURATION)
            .unwrap_or(false)
    }

    fn mark_copied(&mut self) {
        self.copied_at = Some(Instant::now());
    }
}

struct DraggedSplitHandle;

struct CommitTagPicker {
    picker: Entity<Picker<CommitTagPickerDelegate>>,
}

impl CommitTagPicker {
    fn new(tag_names: Vec<SharedString>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let delegate = CommitTagPickerDelegate {
            picker: cx.entity().downgrade(),
            tag_names,
            selected_index: 0,
        };
        let picker = cx.new(|cx| {
            Picker::nonsearchable_uniform_list(delegate, window, cx)
                .initial_width(COMMIT_TAG_LIST_WIDTH_IN_REMS)
        });
        Self { picker }
    }
}

impl EventEmitter<DismissEvent> for CommitTagPicker {}
impl ModalView for CommitTagPicker {}

impl Focusable for CommitTagPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for CommitTagPicker {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex().child(self.picker.clone())
    }
}

struct CommitTagPickerDelegate {
    picker: WeakEntity<CommitTagPicker>,
    tag_names: Vec<SharedString>,
    selected_index: usize,
}

impl PickerDelegate for CommitTagPickerDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "commit-tag"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        i18n::t!("Copy Tag").into()
    }

    fn match_count(&self) -> usize {
        self.tag_names.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix;
    }

    fn update_matches(
        &mut self,
        _query: String,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        Task::ready(())
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        if let Some(tag_name) = self.tag_names.get(self.selected_index) {
            cx.write_to_clipboard(ClipboardItem::new_string(tag_name.to_string()));
        }
        self.dismissed(window, cx);
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.picker
            .update(cx, |_this, cx| cx.emit(DismissEvent))
            .ok();
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(Label::new(self.tag_names.get(ix)?.clone())),
        )
    }
}

#[derive(Clone)]
struct ChangedFileEntry {
    status: FileStatus,
    file_name: SharedString,
    dir_path: SharedString,
    repo_path: RepoPath,
}

impl ChangedFileEntry {
    fn from_commit_file(file: &CommitFile, _cx: &App) -> Self {
        let file_name: SharedString = file
            .path
            .file_name()
            .map(|n| n.to_string())
            .unwrap_or_default()
            .into();
        let dir_path: SharedString = file
            .path
            .parent()
            .map(|p| p.as_unix_str().to_string())
            .unwrap_or_default()
            .into();

        let status_code = match (&file.old_text, &file.new_text) {
            (None, Some(_)) => StatusCode::Added,
            (Some(_), None) => StatusCode::Deleted,
            _ => StatusCode::Modified,
        };

        let status = FileStatus::Tracked(TrackedStatus {
            index_status: status_code,
            worktree_status: StatusCode::Unmodified,
        });

        Self {
            status,
            file_name,
            dir_path,
            repo_path: file.path.clone(),
        }
    }

    fn open_in_commit_view(
        &self,
        commit_sha: &SharedString,
        repository: &WeakEntity<Repository>,
        workspace: &WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut App,
    ) {
        CommitView::open(
            commit_sha.to_string(),
            repository.clone(),
            workspace.clone(),
            None,
            Some(self.repo_path.clone()),
            window,
            cx,
        );
    }

    fn render(
        &self,
        ix: usize,
        depth: usize,
        directory_label: Option<SharedString>,
        commit_sha: SharedString,
        repository: WeakEntity<Repository>,
        workspace: WeakEntity<Workspace>,
        _cx: &App,
    ) -> AnyElement {
        let file_name = self.file_name.clone();
        let dir_path = self.dir_path.clone();

        ListItem::new(("changed-file", ix))
            .spacing(ListItemSpacing::Sparse)
            .indent_level(depth)
            .indent_step_size(px(TREE_INDENT))
            .start_slot(git_status_icon(self.status))
            .child(
                Label::new(file_name.clone())
                    .size(LabelSize::Small)
                    .truncate(),
            )
            .when_some(directory_label, |this, directory_label| {
                this.child(
                    Label::new(directory_label)
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .truncate_start(),
                )
            })
            .tooltip({
                let meta = if dir_path.is_empty() {
                    file_name
                } else {
                    format!("{}/{}", dir_path, file_name).into()
                };
                move |_, cx| Tooltip::with_meta(i18n::t!("View Changes"), None, meta.clone(), cx)
            })
            .on_click({
                let entry = self.clone();
                move |_, window, cx| {
                    entry.open_in_commit_view(&commit_sha, &repository, &workspace, window, cx);
                }
            })
            .into_any_element()
    }
}

enum ChangedFileTreeEntry {
    Directory(ChangedFileDirectoryEntry),
    File(ChangedFileTreeStatusEntry),
}

struct ChangedFileTreeStatusEntry {
    entry: ChangedFileEntry,
    depth: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ChangedFilesViewMode {
    Flat,
    #[default]
    Tree,
}

impl ChangedFilesViewMode {
    fn toggled(self) -> Self {
        match self {
            Self::Flat => Self::Tree,
            Self::Tree => Self::Flat,
        }
    }

    fn is_tree(self) -> bool {
        matches!(self, Self::Tree)
    }
}

struct ChangedFileDirectoryEntry {
    path: RepoPath,
    name: SharedString,
    depth: usize,
    expanded: bool,
}

impl ChangedFileDirectoryEntry {
    fn render(&self, ix: usize, git_graph: WeakEntity<GitGraph>, cx: &App) -> AnyElement {
        let path = self.path.clone();
        let expanded = self.expanded;
        let folder_icon = FileIcons::get_folder_icon(expanded, path.as_std_path(), cx)
            .map(|icon| {
                Icon::from_path(icon)
                    .size(IconSize::Small)
                    .color(Color::Muted)
            })
            .unwrap_or_else(|| {
                let icon = if expanded {
                    IconName::FolderOpen
                } else {
                    IconName::Folder
                };
                Icon::new(icon).size(IconSize::Small).color(Color::Muted)
            });

        ListItem::new(("changed-file-dir", ix))
            .spacing(ListItemSpacing::Sparse)
            .indent_level(self.depth)
            .indent_step_size(px(TREE_INDENT))
            .start_slot(folder_icon)
            .child(
                Label::new(self.name.clone())
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .truncate(),
            )
            .tooltip({
                let name = self.name.clone();
                move |_, cx| Tooltip::with_meta(i18n::t!("Toggle Folder"), None, name.clone(), cx)
            })
            .on_click(move |_, _, cx| {
                git_graph
                    .update(cx, |git_graph, cx| {
                        git_graph
                            .changed_files_expanded_dirs
                            .insert(path.clone(), !expanded);
                        cx.notify();
                    })
                    .ok();
            })
            .into_any_element()
    }
}

#[derive(Default)]
struct ChangedFileTreeNode {
    name: SharedString,
    path: Option<RepoPath>,
    children: BTreeMap<SharedString, ChangedFileTreeNode>,
    files: Vec<ChangedFileEntry>,
}

fn build_changed_file_tree_entries(
    mut files: Vec<ChangedFileEntry>,
    expanded_dirs: &HashMap<RepoPath, bool>,
) -> Vec<ChangedFileTreeEntry> {
    files.sort_by(|a, b| a.repo_path.cmp(&b.repo_path));

    let mut root = ChangedFileTreeNode::default();
    for file in files {
        let components: Vec<&str> = file.repo_path.components().collect();
        if components.is_empty() {
            root.files.push(file);
            continue;
        }

        let mut current = &mut root;
        let mut current_path = String::new();

        for (ix, component) in components.iter().enumerate() {
            if ix == components.len() - 1 {
                current.files.push(file.clone());
            } else {
                if !current_path.is_empty() {
                    current_path.push('/');
                }
                current_path.push_str(component);

                let Ok(dir_path) = RepoPath::new(&current_path) else {
                    continue;
                };
                let component = SharedString::from(component.to_string());

                current = current
                    .children
                    .entry(component.clone())
                    .or_insert_with(|| ChangedFileTreeNode {
                        name: component,
                        path: Some(dir_path),
                        ..Default::default()
                    });
            }
        }
    }

    flatten_changed_file_tree(&root, 0, expanded_dirs)
}

fn flatten_changed_file_tree(
    node: &ChangedFileTreeNode,
    depth: usize,
    expanded_dirs: &HashMap<RepoPath, bool>,
) -> Vec<ChangedFileTreeEntry> {
    let mut entries = Vec::new();

    for child in node.children.values() {
        let (terminal, name) = compact_changed_file_directory_chain(child);
        let Some(path) = terminal.path.clone().or_else(|| child.path.clone()) else {
            continue;
        };
        let expanded = *expanded_dirs.get(&path).unwrap_or(&true);
        let child_entries = flatten_changed_file_tree(terminal, depth + 1, expanded_dirs);

        entries.push(ChangedFileTreeEntry::Directory(ChangedFileDirectoryEntry {
            path,
            name,
            depth,
            expanded,
        }));

        if expanded {
            entries.extend(child_entries);
        }
    }

    entries.extend(
        node.files
            .iter()
            .cloned()
            .map(|entry| ChangedFileTreeEntry::File(ChangedFileTreeStatusEntry { entry, depth })),
    );
    entries
}

fn compact_changed_file_directory_chain(
    mut node: &ChangedFileTreeNode,
) -> (&ChangedFileTreeNode, SharedString) {
    let mut parts = vec![node.name.clone()];
    while node.files.is_empty() && node.children.len() == 1 {
        let Some(child) = node.children.values().next() else {
            continue;
        };
        if child.path.is_none() {
            break;
        }
        parts.push(child.name.clone());
        node = child;
    }
    (node, SharedString::from(parts.join("/")))
}

enum QueryState {
    Pending(SharedString),
    Confirmed((SharedString, Task<()>)),
    Empty,
}

impl QueryState {
    fn next_state(&mut self) {
        match self {
            Self::Confirmed((query, _)) => *self = Self::Pending(std::mem::take(query)),
            _ => {}
        };
    }
}

struct SearchState {
    case_sensitive: bool,
    editor: Entity<Editor>,
    state: QueryState,
    matches: IndexSet<Oid>,
    selected_index: Option<usize>,
    outcome: graph_search::SearchOutcome,
}

struct SplitState {
    left_ratio: f32,
    visible_left_ratio: f32,
}

impl SplitState {
    fn new() -> Self {
        Self {
            left_ratio: 1.0,
            visible_left_ratio: 1.0,
        }
    }

    fn right_ratio(&self) -> f32 {
        1.0 - self.visible_left_ratio
    }

    fn on_drag_move(
        &mut self,
        drag_event: &DragMoveEvent<DraggedSplitHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        let drag_position = drag_event.event.position;
        let bounds = drag_event.bounds;
        let bounds_width = bounds.right() - bounds.left();

        let min_ratio = 0.1;
        let max_ratio = 0.9;

        let new_ratio = (drag_position.x - bounds.left()) / bounds_width;
        self.visible_left_ratio = new_ratio.clamp(min_ratio, max_ratio);
    }

    fn commit_ratio(&mut self) {
        self.left_ratio = self.visible_left_ratio;
    }

    fn on_double_click(&mut self) {
        self.left_ratio = 1.0;
        self.visible_left_ratio = 1.0;
    }
}

actions!(
    git_graph,
    [
        /// Opens the Git Graph Tab.
        Open,
        /// Focuses the search field.
        FocusSearch,
        /// Focuses the next git graph tab stop.
        FocusNextTabStop,
        /// Focuses the previous git graph tab stop.
        FocusPreviousTabStop,
        /// Selects a commit half a page above the current selection.
        ScrollUp,
        /// Selects a commit half a page below the current selection.
        ScrollDown,
        /// Toggles the selected commit's changed files between flat and tree views.
        ToggleChangedFilesView,
        /// Reorders, squashes, rewords or drops the selected commits in one edit.
        EditSelectedCommits,
    ]
);

/// Opens the Git Graph Tab at a specific commit.
#[derive(Clone, PartialEq, serde::Deserialize, schemars::JsonSchema, gpui::Action)]
#[action(namespace = git_graph)]
pub struct OpenAtCommit {
    pub sha: String,
}

/// How many ref chips a graph row shows before folding the rest into a "+N"
/// chip. A commit can carry thousands of refs (every release branch cut from
/// it, say), and drawing each one on every frame stalls scrolling.
const ROW_REF_CHIP_LIMIT: usize = 3;
/// How many of the folded refs the "+N" chip's tooltip names.
const HIDDEN_REF_TOOLTIP_LIMIT: usize = 20;
/// How many ref chips the detail panel shows until asked for all of them.
const DETAIL_REF_CHIP_LIMIT: usize = 30;
/// How long the selection has to rest on a commit before `git describe` is
/// asked about it; walking the rows with the arrow keys would otherwise queue
/// one per row passed over.
const TAG_DISTANCE_DEBOUNCE: Duration = Duration::from_millis(150);

/// What one of git's `%D` decorations points at. The graph shows a different
/// icon, colour and tooltip per kind so that a tag is not mistaken for a branch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RefKind {
    /// The branch that is checked out right now.
    Head,
    LocalBranch,
    RemoteBranch,
    Tag,
}

impl RefKind {
    fn icon(self) -> IconName {
        match self {
            RefKind::Head => IconName::Check,
            RefKind::LocalBranch => IconName::GitBranch,
            RefKind::RemoteBranch => IconName::Server,
            RefKind::Tag => IconName::Bookmark,
        }
    }

    fn description(self) -> &'static str {
        match self {
            RefKind::Head => i18n::t!("Current branch (the branch checked out now)"),
            RefKind::LocalBranch => i18n::t!("Local branch (a branch on this computer)"),
            RefKind::RemoteBranch => {
                i18n::t!("Remote branch (a copy of a branch on the server)")
            }
            RefKind::Tag => i18n::t!("Tag (a marker given to a particular commit)"),
        }
    }

    /// Whether the graph may move this ref, which rules out tags (they name a
    /// fixed commit) and remote branches (they mirror the server).
    fn is_movable_branch(self) -> bool {
        matches!(self, RefKind::Head | RefKind::LocalBranch)
    }
}

/// Splits `name` at the first `/` and reports whether the part before it names
/// one of this repository's remotes. When the branch list has not loaded yet
/// `remote_names` is empty, and only `origin/` is assumed to be a remote,
/// because that is the name `git clone` creates.
fn decoration_is_remote_ref(name: &str, remote_names: &[SharedString]) -> bool {
    let Some((prefix, rest)) = name.split_once('/') else {
        return false;
    };
    if prefix.is_empty() || rest.is_empty() {
        return false;
    }
    if remote_names.is_empty() {
        return prefix == "origin";
    }
    remote_names.iter().any(|remote| remote.as_ref() == prefix)
}

/// Classifies one decoration from git's `%D` format, returning the kind and the
/// bare ref name. Detached `HEAD` has no ref name and yields `None`.
fn classify_ref_decoration(
    decoration: &str,
    head_branch_name: Option<&str>,
    remote_names: &[SharedString],
) -> Option<(RefKind, SharedString)> {
    let decoration = decoration.trim();
    if let Some(tag_name) = decoration.strip_prefix("tag: ") {
        let tag_name = tag_name.trim();
        if tag_name.is_empty() {
            return None;
        }
        return Some((RefKind::Tag, SharedString::from(tag_name.to_string())));
    }
    if let Some(branch_name) = decoration.strip_prefix("HEAD -> ") {
        let branch_name = branch_name.trim();
        if branch_name.is_empty() {
            return None;
        }
        return Some((RefKind::Head, SharedString::from(branch_name.to_string())));
    }
    if decoration.is_empty() || decoration == "HEAD" {
        return None;
    }

    let kind = if head_branch_name == Some(decoration) {
        RefKind::Head
    } else if decoration_is_remote_ref(decoration, remote_names) {
        RefKind::RemoteBranch
    } else {
        RefKind::LocalBranch
    };
    Some((kind, SharedString::from(decoration.to_string())))
}

/// Which remotes this repository has, taken from the cached branch list so that
/// classifying a chip never has to wait on git.
fn remote_names_from_branches(branches: &[Branch]) -> Vec<SharedString> {
    let mut names: Vec<SharedString> = Vec::new();
    for branch in branches {
        let Some(remote_name) = branch.remote_name() else {
            continue;
        };
        if names.iter().any(|name| name.as_ref() == remote_name) {
            continue;
        }
        names.push(SharedString::from(remote_name.to_string()));
    }
    names
}

/// What telling a commit's `%D` decorations apart depends on: the checked-out
/// branch and the remotes. Both change only with the branch list.
#[derive(Clone, Debug, Default, PartialEq)]
struct RefChipContext {
    head_branch_name: Option<SharedString>,
    remote_names: Vec<SharedString>,
}

/// One `%D` decoration of a commit, classified once when the commit arrives
/// rather than on every frame.
#[derive(Clone, Debug, PartialEq)]
struct RefChip {
    decoration: SharedString,
    classified: Option<(RefKind, SharedString)>,
}

fn classify_ref_chips(ref_names: &[SharedString], context: &RefChipContext) -> Arc<[RefChip]> {
    ref_names
        .iter()
        .map(|decoration| RefChip {
            decoration: decoration.clone(),
            classified: classify_ref_decoration(
                decoration,
                context.head_branch_name.as_deref(),
                &context.remote_names,
            ),
        })
        .collect()
}

/// Splits a commit's chips into the ones shown and how many are folded away.
fn visible_ref_chips(chips: &[RefChip], limit: usize) -> (&[RefChip], usize) {
    let shown = chips.len().min(limit);
    (&chips[..shown], chips.len() - shown)
}

/// The tooltip of the "+N" chip: the folded refs' names, up to a limit, and
/// how many more there are past it.
fn hidden_ref_chips_tooltip(hidden: &[RefChip]) -> String {
    let mut lines = hidden
        .iter()
        .take(HIDDEN_REF_TOOLTIP_LIMIT)
        .map(|chip| chip.decoration.to_string())
        .collect::<Vec<_>>();
    if let Some(more) = hidden.len().checked_sub(HIDDEN_REF_TOOLTIP_LIMIT)
        && more > 0
    {
        lines.push(i18n::t!("and {more} more"));
    }
    lines.join("\n")
}

/// How a click changes the set of picked rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SelectionGesture {
    /// A plain click: only the clicked row stays picked.
    Replace,
    /// Ctrl (Cmd on macOS) click: add or remove the clicked row.
    Toggle,
    /// Shift click: pick everything between the anchor and the clicked row.
    Range,
}

impl SelectionGesture {
    fn from_modifiers(modifiers: &Modifiers) -> Self {
        if modifiers.shift {
            SelectionGesture::Range
        } else if modifiers.secondary() {
            SelectionGesture::Toggle
        } else {
            SelectionGesture::Replace
        }
    }
}

/// Applies `gesture` to the picked rows, returning the new set and the new
/// anchor for a following shift-click. The set is never left empty: the detail
/// panel always shows the row that was clicked last.
fn apply_selection_gesture(
    selection: &BTreeSet<usize>,
    anchor: Option<usize>,
    idx: usize,
    gesture: SelectionGesture,
) -> (BTreeSet<usize>, usize) {
    match gesture {
        SelectionGesture::Replace => (BTreeSet::from_iter([idx]), idx),
        SelectionGesture::Toggle => {
            let mut selection = selection.clone();
            if !selection.remove(&idx) {
                selection.insert(idx);
            } else if selection.is_empty() {
                selection.insert(idx);
            }
            (selection, idx)
        }
        SelectionGesture::Range => {
            let Some(anchor) = anchor else {
                return (BTreeSet::from_iter([idx]), idx);
            };
            let range = if anchor <= idx {
                anchor..=idx
            } else {
                idx..=anchor
            };
            (range.collect(), anchor)
        }
    }
}

/// A toast from somewhere that only holds a weak handle to the workspace.
fn report_to_workspace(
    workspace: &WeakEntity<Workspace>,
    message: String,
    icon: IconName,
    color: Color,
    cx: &mut App,
) {
    workspace
        .update(cx, |workspace, cx| {
            let toast = StatusToast::new(message, cx, move |this, _cx| {
                this.icon(Icon::new(icon).size(IconSize::Small).color(color))
                    .dismiss_button(true)
            });
            workspace.toggle_status_toast(toast, cx);
        })
        .ok();
}

/// Puts the branch back where it stood before a rewrite.
///
/// `reset --hard` cannot itself be taken back from the UI, so this refuses unless the working tree
/// is clean and HEAD is still exactly the commit the rewrite produced: anything else means there
/// is work here that the reset would throw away without the user having asked.
fn undo_rewrite(
    repository: Entity<Repository>,
    workspace: WeakEntity<Workspace>,
    old_head: String,
    new_head: String,
    window: &mut Window,
    cx: &mut App,
) {
    let (head, dirty) = {
        let repository = repository.read(cx);
        (
            repository
                .head_commit
                .as_ref()
                .map(|commit| commit.sha.to_string()),
            repository
                .cached_status()
                .any(|entry| entry.status.has_changes()),
        )
    };
    if head.as_deref() != Some(new_head.as_str()) {
        report_to_workspace(
            &workspace,
            i18n::t!(
                "HEAD has moved since the rewrite, so it cannot be undone from here. Undo it by hand with git reflog."
            )
            .to_string(),
            IconName::Warning,
            Color::Warning,
            cx,
        );
        return;
    }
    if dirty {
        report_to_workspace(
            &workspace,
            i18n::t!(
                "There are uncommitted changes, so it cannot be undone. Commit or stash them, then try again."
            )
            .to_string(),
            IconName::Warning,
            Color::Warning,
            cx,
        );
        return;
    }
    let short_old: String = old_head.chars().take(git::SHORT_SHA_LENGTH).collect();
    let short_new: String = new_head.chars().take(git::SHORT_SHA_LENGTH).collect();
    let confirmation = window.prompt(
        gpui::PromptLevel::Warning,
        i18n::t!("Go back to the state before the reorder?"),
        Some(&i18n::t!(
            "This throws away the current tip {short_new} and goes back to {short_old} (git reset --hard).\nThe commits the reorder made can then only be found through the reflog."
        )),
        &[i18n::tc!("git prompt", "Go Back"), i18n::t!("Cancel")],
        cx,
    );
    cx.spawn(async move |cx| {
        if confirmation.await.unwrap_or(1) != 0 {
            return;
        }
        let receiver = repository.update(cx, |repository, cx| {
            repository.reset_to(old_head, ResetMode::Hard, cx)
        });
        let (message, failed) = match receiver.await {
            Ok(Ok(output)) if output.succeeded() => (i18n::t!("Went back to {short_old}."), false),
            Ok(Ok(output)) => {
                let failure = output.failure_message();
                (i18n::t!("Could not undo\n{failure}"), true)
            }
            Ok(Err(error)) => {
                let failure = format!("{error:#}");
                (i18n::t!("Could not undo\n{failure}"), true)
            }
            Err(_) => return,
        };
        cx.update(|cx| {
            report_to_workspace(
                &workspace,
                message,
                if failed {
                    IconName::XCircle
                } else {
                    IconName::GitBranch
                },
                if failed { Color::Error } else { Color::Muted },
                cx,
            );
        });
    })
    .detach();
}

/// Whether the picked rows are one unbroken chain of commits, which is what
/// `squash_range` needs: reading from the newest picked row down, every commit
/// but the oldest must have exactly one parent, and that parent must be the
/// next picked row. A merge commit in the middle therefore breaks the chain.
fn is_contiguous_commit_range(commits: &[Rc<CommitEntry>], rows: &BTreeSet<usize>) -> bool {
    if rows.is_empty() {
        return false;
    }
    if rows.iter().any(|row| *row >= commits.len()) {
        return false;
    }
    rows.iter().zip(rows.iter().skip(1)).all(|(child, parent)| {
        let Some(child) = commits.get(*child) else {
            return false;
        };
        let Some(parent) = commits.get(*parent) else {
            return false;
        };
        child.data.parents.as_slice() == [parent.data.sha]
    })
}

/// Whether `target` can be reached from `from` by following parents through the
/// commits the graph has loaded.
///
/// `squash_range` rewrites whichever branch is checked out, so a range that is
/// not on it would replay unrelated commits onto a new root. Commits that have
/// not been loaded yet end the walk, which makes this answer "no" rather than
/// guess.
fn commit_reaches(commits: &[Rc<CommitEntry>], from: Oid, target: Oid) -> bool {
    let mut index_by_sha = HashMap::default();
    for (index, commit) in commits.iter().enumerate() {
        index_by_sha.insert(commit.data.sha, index);
    }
    let mut visited = HashSet::default();
    let mut pending = vec![from];
    while let Some(current) = pending.pop() {
        if current == target {
            return true;
        }
        if !visited.insert(current) {
            continue;
        }
        let Some(commit) = index_by_sha
            .get(&current)
            .and_then(|index| commits.get(*index))
        else {
            continue;
        };
        pending.extend(commit.data.parents.iter().copied());
    }
    false
}

/// The other worktree that has `branch` checked out, if there is one.
///
/// `git branch -f` refuses to move a branch in that state, and `update_ref`
/// does not: it would leave that worktree's index and files describing a commit
/// its branch no longer names. `worktrees` is the repository's linked
/// worktrees, which already excludes the one the graph is looking at.
fn worktree_holding_branch<'a>(
    worktrees: &'a [GitWorktree],
    branch: &str,
) -> Option<&'a GitWorktree> {
    worktrees.iter().find(|worktree| {
        worktree.ref_name.as_ref().is_some_and(|ref_name| {
            let ref_name = ref_name.as_ref();
            ref_name == branch || ref_name.strip_prefix("refs/heads/") == Some(branch)
        })
    })
}

/// A commit row being dragged: onto a ref chip, onto another commit row, or
/// into the merge-request box.
#[derive(Clone)]
pub(crate) struct DraggedCommit {
    pub(crate) sha: Oid,
    pub(crate) subject: SharedString,
    /// The whole multi-selection, oldest first, when the dragged row is part
    /// of it; otherwise just `sha`.
    pub(crate) selection: Arc<[Oid]>,
}

/// A ref chip being dragged onto a commit row.
#[derive(Clone)]
struct DraggedRef {
    name: SharedString,
    kind: RefKind,
}

/// What follows the pointer while a commit or a ref is dragged.
struct DraggedGraphLabel {
    icon: IconName,
    text: SharedString,
}

impl Render for DraggedGraphLabel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .gap_1()
            .px_1p5()
            .py_0p5()
            .rounded_sm()
            .elevation_2(cx)
            .child(
                Icon::new(self.icon)
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
            )
            .child(Label::new(self.text.clone()).size(LabelSize::Small))
    }
}

/// One side of a drag inside the graph.
#[derive(Clone, Debug, PartialEq, Eq)]
enum GraphDragEnd {
    Commit(Oid),
    /// Several picked commits dragged together, oldest first.
    Commits(Arc<[Oid]>),
    Ref {
        name: SharedString,
        kind: RefKind,
    },
    /// Files dragged out of the git panel.
    Files(Vec<RepoPath>),
}

impl GraphDragEnd {
    /// The dragged row on its own, or the whole multi-selection when the row is part of one.
    fn dragged_commit(dragged: &DraggedCommit) -> Self {
        if dragged.selection.len() > 1 {
            GraphDragEnd::Commits(dragged.selection.clone())
        } else {
            GraphDragEnd::Commit(dragged.sha)
        }
    }
}

/// What dropping one thing on another is going to do. The graph asks for
/// confirmation before running any of these.
#[derive(Clone, Debug, PartialEq, Eq)]
enum GraphDropAction {
    /// The drop target is the branch that is already checked out. `shas` is
    /// oldest first, the order `cherry-pick` replays them in.
    CherryPickOntoHead {
        shas: Arc<[Oid]>,
        branch: SharedString,
    },
    /// The drop target is another local branch, so it has to be checked out first.
    CherryPickAfterSwitch {
        shas: Arc<[Oid]>,
        branch: SharedString,
    },
    /// Moving the checked-out branch also moves the index and working tree.
    ResetHeadBranch { branch: SharedString, sha: Oid },
    /// Another branch can be re-pointed without touching the working tree.
    MoveBranchRef { branch: SharedString, sha: Oid },
    /// A commit dropped on another takes its place, the way a list reorders.
    /// Nothing runs from the drop itself: the rewrite modal opens with the
    /// commits between the two already rearranged, and confirming it there is
    /// what rewrites the branch.
    MoveCommit { sha: Oid, target: Oid },
    /// Files from the git panel go into the commit they were dropped on.
    Fixup { sha: Oid, repo_paths: Vec<RepoPath> },
    /// The combination has a meaning the graph does not carry out; the reason
    /// is shown instead.
    Refused(&'static str),
}

/// Resolves a drag and a drop target into the git operation they stand for, or
/// `None` when the combination means nothing (a tag or a remote branch cannot
/// be moved, and nothing can be dropped onto itself).
fn graph_drop_action(dragged: &GraphDragEnd, target: &GraphDragEnd) -> Option<GraphDropAction> {
    match (dragged, target) {
        (GraphDragEnd::Commit(_) | GraphDragEnd::Commits(_), GraphDragEnd::Ref { name, kind }) => {
            let shas: Arc<[Oid]> = match dragged {
                GraphDragEnd::Commits(shas) => shas.clone(),
                GraphDragEnd::Commit(sha) => Arc::from([*sha]),
                GraphDragEnd::Ref { .. } | GraphDragEnd::Files(_) => return None,
            };
            match kind {
                RefKind::Head => Some(GraphDropAction::CherryPickOntoHead {
                    shas,
                    branch: name.clone(),
                }),
                RefKind::LocalBranch => Some(GraphDropAction::CherryPickAfterSwitch {
                    shas,
                    branch: name.clone(),
                }),
                RefKind::RemoteBranch | RefKind::Tag => None,
            }
        }
        (GraphDragEnd::Ref { name, kind }, GraphDragEnd::Commit(sha)) => match kind {
            RefKind::Head => Some(GraphDropAction::ResetHeadBranch {
                branch: name.clone(),
                sha: *sha,
            }),
            RefKind::LocalBranch => Some(GraphDropAction::MoveBranchRef {
                branch: name.clone(),
                sha: *sha,
            }),
            RefKind::RemoteBranch | RefKind::Tag => None,
        },
        (GraphDragEnd::Commit(sha), GraphDragEnd::Commit(target)) => {
            (sha != target).then_some(GraphDropAction::MoveCommit {
                sha: *sha,
                target: *target,
            })
        }
        // Let go over the selection itself, a drag that went nowhere is not worth a refusal.
        (GraphDragEnd::Commits(shas), GraphDragEnd::Commit(target)) => (!shas.contains(target))
            .then(|| {
                GraphDropAction::Refused(i18n::t!(
                    "Commits are reordered one at a time. Drag a single commit."
                ))
            }),
        (GraphDragEnd::Files(repo_paths), GraphDragEnd::Commit(sha)) => (!repo_paths.is_empty())
            .then(|| GraphDropAction::Fixup {
                sha: *sha,
                repo_paths: repo_paths.clone(),
            }),
        (GraphDragEnd::Ref { .. } | GraphDragEnd::Files(_), GraphDragEnd::Ref { .. })
        | (_, GraphDragEnd::Commits(_) | GraphDragEnd::Files(_)) => None,
    }
}

/// Why files dragged out of the git panel cannot go into this graph's
/// commits, if they cannot: they have to belong to the repository the graph
/// shows, and that repository has to still be there.
fn dropped_files_refusal<Id: PartialEq>(
    dragged_repository: Option<Id>,
    graph_repository: Option<Id>,
) -> Option<&'static str> {
    match (dragged_repository, graph_repository) {
        (Some(dragged), Some(graph)) if dragged == graph => None,
        (Some(_), Some(_)) => Some(i18n::t!(
            "Files from another repository cannot go into this graph's commits."
        )),
        _ => Some(i18n::t!(
            "The repository could not be found, so the files cannot go into the commit."
        )),
    }
}

/// The stretch of the current branch a history rewrite can reach: from HEAD
/// down through commits with exactly one parent, stopping before the first
/// merge commit or the root commit.
///
/// A rewrite replays everything above the rewritten range onto the result, and
/// that replay would flatten a merge commit, so a commit below one is out of
/// reach even though HEAD can get to it.
#[derive(Debug, Default)]
struct RewritableHistory {
    /// Newest first.
    commits: Vec<Oid>,
    /// The rows those commits are on.
    rows: HashMap<usize, Oid>,
    /// The root commit's row, when the stretch runs all the way down to it:
    /// nothing can be moved past it, but changes can still go into it.
    root_row: Option<usize>,
}

fn rewritable_history(commits: &[Rc<CommitEntry>], head: Oid) -> RewritableHistory {
    let mut row_by_sha = HashMap::default();
    for (row, commit) in commits.iter().enumerate() {
        row_by_sha.insert(commit.data.sha, row);
    }
    let mut history = RewritableHistory::default();
    let mut current = head;
    // A well-formed history cannot loop, but the walk must not hang on one that does.
    while history.commits.len() < commits.len() {
        let Some((row, commit)) = row_by_sha
            .get(&current)
            .and_then(|row| Some((*row, commits.get(*row)?)))
        else {
            break;
        };
        match commit.data.parents.as_slice() {
            [] => {
                history.root_row = Some(row);
                break;
            }
            [parent] => {
                history.commits.push(current);
                history.rows.insert(row, current);
                current = *parent;
            }
            _ => break,
        }
    }
    history
}

/// A commit move worked out: the commits between the dragged commit and the
/// one it was dropped on, and the order the rewrite modal opens with.
#[derive(Debug, PartialEq, Eq)]
struct CommitMove {
    /// The range as it stands, oldest first.
    commits_oldest_first: Vec<Oid>,
    /// Row `i` of the modal starts out as `commits_oldest_first[initial_order[i]]`.
    initial_order: Vec<usize>,
}

/// Works out moving `dragged` to where `target` is, as a list reorder of the
/// commits between them (both included) in the graph's newest-first order:
/// `dragged` ends up where `target` was, and the commits in between shift one
/// place towards where `dragged` came from.
///
/// Both have to be on `history` (see [`RewritableHistory`], newest first);
/// `on_current_branch` tells a commit that is not on the branch at all from one
/// that is only below a merge commit, so that the refusal says which it is.
fn plan_commit_move(
    history: &[Oid],
    dragged: Oid,
    target: Oid,
    on_current_branch: impl Fn(Oid) -> bool,
) -> Result<CommitMove, SharedString> {
    let position = |sha: Oid| history.iter().position(|commit| *commit == sha);
    let (Some(from), Some(to)) = (position(dragged), position(target)) else {
        let outside = if position(dragged).is_none() {
            dragged
        } else {
            target
        };
        return Err(if on_current_branch(outside) {
            i18n::t!(
                "A merge commit and the commits before it cannot be reordered (rewriting them would break the merge)."
            )
            .into()
        } else {
            i18n::t!(
                "Commits that are not in the current branch's history cannot be reordered. Switch to that branch, then try again."
            )
            .into()
        });
    };
    let (newest, oldest) = (from.min(to), from.max(to));
    let Some(range) = history.get(newest..=oldest) else {
        return Err(i18n::t!("Could not read the range to reorder.").into());
    };
    let mut reordered = range.to_vec();
    if !rewrite_range_modal::move_list_item(&mut reordered, from - newest, to - newest) {
        return Err(i18n::t!("Dropped on the same commit, so the order does not change.").into());
    }
    let commits_oldest_first: Vec<Oid> = range.iter().rev().copied().collect();
    let index_of: HashMap<Oid, usize> = commits_oldest_first
        .iter()
        .enumerate()
        .map(|(index, sha)| (*sha, index))
        .collect();
    let initial_order = reordered
        .iter()
        .rev()
        .filter_map(|sha| index_of.get(sha).copied())
        .collect::<Vec<_>>();
    if initial_order.len() != commits_oldest_first.len() {
        return Err(i18n::t!("Could not read the range to reorder.").into());
    }
    Ok(CommitMove {
        commits_oldest_first,
        initial_order,
    })
}

/// The group every commit row belongs to, so that the note a row shows while
/// something hangs over it can follow the row's hover.
const GRAPH_ROW_GROUP: &str = "git-graph-row";

/// The note a drop target shows while a `T` it would take hangs over `group`,
/// saying what letting go there does. It is always in the tree and only
/// painted while that drag is over the group.
pub(crate) fn drop_hint<T: 'static>(
    group: impl Into<SharedString>,
    text: SharedString,
    cx: &App,
) -> Div {
    h_flex()
        .invisible()
        .group_drag_over::<T>(group, |style| style.visible())
        // `group_drag_over` is only consulted for an element with a hitbox of its
        // own; an empty `drag_over` gives it one without catching any events.
        .drag_over::<T>(|style, _, _, _| style)
        .flex_none()
        .px_1()
        .rounded_sm()
        .border_1()
        .border_color(cx.theme().colors().drop_target_border)
        .bg(cx.theme().colors().elevated_surface_background)
        .child(
            Label::new(text)
                .size(LabelSize::XSmall)
                .color(Color::Accent),
        )
}

/// What the drag in flight would do on each commit row. It is worked out once
/// per frame, because the rows are drawn again on every mouse move while
/// something is dragged, and only the rows that would take the drop are
/// highlighted and labelled.
#[derive(Clone, Default)]
enum RowDropContext {
    #[default]
    Nothing,
    /// A branch chip, which every row takes.
    Ref { name: SharedString, kind: RefKind },
    /// A single commit of the rewritable stretch, which can take the place of
    /// any other commit of it.
    MoveCommit {
        dragged: Oid,
        history: Rc<RewritableHistory>,
    },
    /// Files from the git panel, which can go into a commit of the rewritable
    /// stretch or into the root commit at its end.
    Files { history: Rc<RewritableHistory> },
}

impl RowDropContext {
    /// What letting go over `row` does, or `None` when it would do nothing and
    /// the row should not be highlighted.
    fn hint(&self, row: usize) -> Option<SharedString> {
        match self {
            RowDropContext::Nothing => None,
            RowDropContext::Ref { name, kind } => match kind {
                RefKind::Head => Some(i18n::t!("Drop here: reset {name} to here (reset)").into()),
                RefKind::LocalBranch => Some(i18n::t!("Drop here: move {name} here").into()),
                RefKind::RemoteBranch | RefKind::Tag => None,
            },
            RowDropContext::MoveCommit { dragged, history } => history
                .rows
                .get(&row)
                .is_some_and(|sha| sha != dragged)
                .then(|| i18n::t!("Drop here: move to this position (reorder)").into()),
            RowDropContext::Files { history } => (history.rows.contains_key(&row)
                || history.root_row == Some(row))
            .then(|| i18n::t!("Drop here: add to this commit (fixup)").into()),
        }
    }

    fn accepts_commit(&self, row: usize) -> bool {
        matches!(self, RowDropContext::MoveCommit { .. }) && self.hint(row).is_some()
    }

    fn accepts_files(&self, row: usize) -> bool {
        matches!(self, RowDropContext::Files { .. }) && self.hint(row).is_some()
    }

    /// The note for `row`, pinned to the row's right edge.
    fn render_hint(&self, row: usize, cx: &App) -> Option<AnyElement> {
        let hint = self.hint(row)?;
        let hint = match self {
            RowDropContext::Nothing => return None,
            RowDropContext::Ref { .. } => drop_hint::<DraggedRef>(GRAPH_ROW_GROUP, hint, cx),
            RowDropContext::MoveCommit { .. } => {
                drop_hint::<DraggedCommit>(GRAPH_ROW_GROUP, hint, cx)
            }
            RowDropContext::Files { .. } => {
                drop_hint::<crate::git_panel::DraggedGitPanelEntries>(GRAPH_ROW_GROUP, hint, cx)
            }
        };
        Some(
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .right_2()
                .flex()
                .items_center()
                .child(hint)
                .into_any_element(),
        )
    }
}

/// [`RewritableHistory`] for the commits the graph has loaded, kept until HEAD
/// or the loaded list changes: working it out walks every loaded commit, and
/// the rows ask for it on every frame of a drag.
struct CachedRewritableHistory {
    head: Oid,
    commit_count: usize,
    first_commit: Option<Oid>,
    history: Rc<RewritableHistory>,
}

/// What letting `count` dragged commits go over a branch chip does.
fn chip_drop_hint(count: usize, branch: &str, kind: RefKind) -> Option<SharedString> {
    match kind {
        RefKind::Head if count > 1 => Some(
            i18n::tn!(
                count,
                "Drop here: cherry-pick {count} commit",
                "Drop here: cherry-pick {count} commits"
            )
            .into(),
        ),
        RefKind::Head => Some(i18n::t!("Drop here: cherry-pick").into()),
        RefKind::LocalBranch if count > 1 => Some(
            i18n::tn!(
                count,
                "Drop here: switch to {branch} and cherry-pick {count} commit",
                "Drop here: switch to {branch} and cherry-pick {count} commits"
            )
            .into(),
        ),
        RefKind::LocalBranch => {
            Some(i18n::t!("Drop here: switch to {branch} and cherry-pick").into())
        }
        RefKind::RemoteBranch | RefKind::Tag => None,
    }
}

/// The title and detail of the confirmation for commits dropped on a branch chip.
fn cherry_pick_drop_prompt(shas: &[Oid], branch: &str, after_switch: bool) -> (String, String) {
    match (shas, after_switch) {
        ([sha], false) => {
            let sha = sha.display_short();
            (
                i18n::t!("Cherry-pick this commit onto the current branch?").to_string(),
                i18n::t!(
                    "Adds the changes of {sha} to the tip of the current branch {branch} as a new commit (cherry-pick)."
                ),
            )
        }
        ([sha], true) => {
            let sha = sha.display_short();
            (
                i18n::t!("Switch to {branch} and then cherry-pick?"),
                i18n::t!(
                    "Switches to {branch}, then adds the changes of {sha} as a new commit (checkout + cherry-pick)."
                ),
            )
        }
        (shas, false) => (
            i18n::tn!(
                shas.len(),
                "Cherry-pick {count} commit onto the current branch?",
                "Cherry-pick {count} commits onto the current branch?"
            ),
            i18n::tn!(
                shas.len(),
                "Cherry-picks {count} commit: adds it, oldest first, to the tip of the current branch {branch} as a new commit (cherry-pick).",
                "Cherry-picks {count} commits: adds them, oldest first, to the tip of the current branch {branch} as new commits (cherry-pick)."
            ),
        ),
        (shas, true) => (
            i18n::tn!(
                shas.len(),
                "Switch to {branch} and then cherry-pick {count} commit?",
                "Switch to {branch} and then cherry-pick {count} commits?"
            ),
            i18n::tn!(
                shas.len(),
                "Switches to {branch}, then cherry-picks {count} commit: adds it, oldest first, as a new commit (checkout + cherry-pick).",
                "Switches to {branch}, then cherry-picks {count} commits: adds them, oldest first, as new commits (checkout + cherry-pick)."
            ),
        ),
    }
}

/// Makes a commit row a drop target for a dragged commit (reorder) and for
/// files from the git panel (fixup), and gives it the note that says what
/// letting go over it does. Rows that would not take the drag in flight are
/// left without highlight or note.
fn decorate_row_for_drops(
    row: gpui::Stateful<Div>,
    index: usize,
    row_drop: &RowDropContext,
    chip_note: Option<&(usize, SharedString)>,
    graph: WeakEntity<GitGraph>,
    cx: &App,
) -> gpui::Stateful<Div> {
    let drop_background = cx.theme().colors().drop_target_background;
    row.group(GRAPH_ROW_GROUP)
        .relative()
        .when(row_drop.accepts_commit(index), |row| {
            row.drag_over::<DraggedCommit>(move |style, _, _, _| style.bg(drop_background))
        })
        .on_drop({
            let graph = graph.clone();
            move |dragged: &DraggedCommit, window, cx| {
                graph
                    .update(cx, |graph, cx| {
                        graph.handle_commit_drop_on_row(dragged, index, window, cx);
                    })
                    .ok();
            }
        })
        .when(row_drop.accepts_files(index), |row| {
            row.drag_over::<crate::git_panel::DraggedGitPanelEntries>(move |style, _, _, _| {
                style.bg(drop_background)
            })
        })
        .on_drop(
            move |dragged: &crate::git_panel::DraggedGitPanelEntries, window, cx| {
                graph
                    .update(cx, |graph, cx| {
                        graph.handle_files_drop_on_row(dragged, index, window, cx);
                    })
                    .ok();
            },
        )
        .children(row_drop.render_hint(index, cx))
        .children(chip_note.filter(|(row, _)| *row == index).map(|(_, note)| {
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .right_2()
                .flex()
                .items_center()
                .child(
                    // Shown straight away rather than on hover: the graph only knows
                    // about this chip because the pointer is over it.
                    drop_hint::<DraggedCommit>(GRAPH_ROW_GROUP, note.clone(), cx).visible(),
                )
        }))
}

// The date follows the display language's date pattern when it has one, and the OS's date and
// time format otherwise, rather than one fixed English layout.
pub(crate) fn format_timestamp(timestamp: i64) -> String {
    let Ok(datetime) = OffsetDateTime::from_unix_timestamp(timestamp) else {
        return i18n::t!("Unknown").to_string();
    };
    let local_offset = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
    format_local_datetime(datetime, local_offset)
}

fn format_local_datetime(datetime: OffsetDateTime, local_offset: UtcOffset) -> String {
    time_format::format_localized_timestamp(
        datetime,
        OffsetDateTime::now_utc(),
        local_offset,
        time_format::TimestampFormat::Absolute,
    )
}

/// The width the date column needs to show a date without cutting it off. Two-digit days and
/// hours late in the day stand in for the widest dates; September has the longest English month
/// name and December the longest month number.
fn date_column_min_width(window: &Window, cx: &App) -> Option<Pixels> {
    let local_offset = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
    let year = OffsetDateTime::now_utc().to_offset(local_offset).year();
    let time_of_day = time::Time::from_hms(22, 58, 0).ok()?;
    let font = <theme_settings::ThemeSettings as settings::Settings>::get_global(cx)
        .ui_font
        .clone();
    let font_size = TextSize::Default.rems(cx).to_pixels(window.rem_size());
    let text_width = [time::Month::September, time::Month::December]
        .into_iter()
        .filter_map(|month| {
            let date = time::Date::from_calendar_date(year, month, 28).ok()?;
            let widest =
                time::PrimitiveDateTime::new(date, time_of_day).assume_offset(local_offset);
            let text = SharedString::from(format_local_datetime(widest, local_offset));
            let run = gpui::TextRun {
                len: text.len(),
                font: font.clone(),
                color: gpui::Hsla::default(),
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            Some(
                window
                    .text_system()
                    .shape_line(text, font_size, &[run], None)
                    .width,
            )
        })
        .reduce(|widest, width| if width > widest { width } else { widest })?;
    // The cell's padding on both sides and the column divider.
    Some(text_width + rems(1.).to_pixels(window.rem_size()))
}

fn accent_colors_count(accents: &AccentColors) -> usize {
    accents.0.len()
}

#[derive(Copy, Clone, Debug)]
struct BranchColor(u8);

#[derive(Debug)]
enum LaneState {
    Empty,
    Active {
        child: Oid,
        parent: Oid,
        color: Option<BranchColor>,
        starting_row: usize,
        starting_col: usize,
        destination_column: Option<usize>,
        segments: SmallVec<[CommitLineSegment; 1]>,
    },
}

impl LaneState {
    fn to_commit_lines(
        &mut self,
        ending_row: usize,
        lane_column: usize,
        parent_column: usize,
        parent_color: BranchColor,
    ) -> Option<CommitLine> {
        let state = std::mem::replace(self, LaneState::Empty);

        match state {
            LaneState::Active {
                #[cfg_attr(not(test), allow(unused_variables))]
                parent,
                #[cfg_attr(not(test), allow(unused_variables))]
                child,
                color,
                starting_row,
                starting_col,
                destination_column,
                mut segments,
            } => {
                let final_destination = destination_column.unwrap_or(parent_column);
                let final_color = color.unwrap_or(parent_color);

                Some(CommitLine {
                    #[cfg(test)]
                    child,
                    #[cfg(test)]
                    parent,
                    child_column: starting_col,
                    full_interval: starting_row..ending_row,
                    color_idx: final_color.0 as usize,
                    segments: {
                        match segments.last_mut() {
                            Some(CommitLineSegment::Straight { to_row })
                                if *to_row == usize::MAX =>
                            {
                                if final_destination != lane_column {
                                    *to_row = ending_row - 1;

                                    let curved_line = CommitLineSegment::Curve {
                                        to_column: final_destination,
                                        on_row: ending_row,
                                        curve_kind: CurveKind::Checkout,
                                    };

                                    if *to_row == starting_row {
                                        let last_index = segments.len() - 1;
                                        segments[last_index] = curved_line;
                                    } else {
                                        segments.push(curved_line);
                                    }
                                } else {
                                    *to_row = ending_row;
                                }
                            }
                            Some(CommitLineSegment::Curve {
                                on_row,
                                to_column,
                                curve_kind,
                            }) if *on_row == usize::MAX => {
                                if *to_column == usize::MAX {
                                    *to_column = final_destination;
                                }
                                if matches!(curve_kind, CurveKind::Merge) {
                                    *on_row = starting_row + 1;
                                    if *on_row < ending_row {
                                        if *to_column != final_destination {
                                            segments.push(CommitLineSegment::Straight {
                                                to_row: ending_row - 1,
                                            });
                                            segments.push(CommitLineSegment::Curve {
                                                to_column: final_destination,
                                                on_row: ending_row,
                                                curve_kind: CurveKind::Checkout,
                                            });
                                        } else {
                                            segments.push(CommitLineSegment::Straight {
                                                to_row: ending_row,
                                            });
                                        }
                                    } else if *to_column != final_destination {
                                        segments.push(CommitLineSegment::Curve {
                                            to_column: final_destination,
                                            on_row: ending_row,
                                            curve_kind: CurveKind::Checkout,
                                        });
                                    }
                                } else {
                                    *on_row = ending_row;
                                    if *to_column != final_destination {
                                        segments.push(CommitLineSegment::Straight {
                                            to_row: ending_row,
                                        });
                                        segments.push(CommitLineSegment::Curve {
                                            to_column: final_destination,
                                            on_row: ending_row,
                                            curve_kind: CurveKind::Checkout,
                                        });
                                    }
                                }
                            }
                            Some(CommitLineSegment::Curve {
                                on_row, to_column, ..
                            }) => {
                                if *on_row < ending_row {
                                    if *to_column != final_destination {
                                        segments.push(CommitLineSegment::Straight {
                                            to_row: ending_row - 1,
                                        });
                                        segments.push(CommitLineSegment::Curve {
                                            to_column: final_destination,
                                            on_row: ending_row,
                                            curve_kind: CurveKind::Checkout,
                                        });
                                    } else {
                                        segments.push(CommitLineSegment::Straight {
                                            to_row: ending_row,
                                        });
                                    }
                                } else if *to_column != final_destination {
                                    segments.push(CommitLineSegment::Curve {
                                        to_column: final_destination,
                                        on_row: ending_row,
                                        curve_kind: CurveKind::Checkout,
                                    });
                                }
                            }
                            _ => {}
                        }

                        segments
                    },
                })
            }
            LaneState::Empty => None,
        }
    }

    fn is_empty(&self) -> bool {
        match self {
            LaneState::Empty => true,
            LaneState::Active { .. } => false,
        }
    }
}

struct CommitEntry {
    data: Arc<InitialGraphCommitData>,
    lane: usize,
    color_idx: usize,
}

type ActiveLaneIdx = usize;

enum AllCommitCount {
    NotLoaded,
    Loading(usize),
    FullyLoaded(usize),
}

#[derive(Debug)]
enum CurveKind {
    Merge,
    Checkout,
}

#[derive(Debug)]
enum CommitLineSegment {
    Straight {
        to_row: usize,
    },
    Curve {
        to_column: usize,
        on_row: usize,
        curve_kind: CurveKind,
    },
}

#[derive(Debug)]
struct CommitLine {
    #[cfg(test)]
    child: Oid,
    #[cfg(test)]
    parent: Oid,
    child_column: usize,
    full_interval: Range<usize>,
    color_idx: usize,
    segments: SmallVec<[CommitLineSegment; 1]>,
}

impl CommitLine {
    fn get_first_visible_segment_idx(&self, first_visible_row: usize) -> Option<(usize, usize)> {
        if first_visible_row > self.full_interval.end {
            return None;
        } else if first_visible_row <= self.full_interval.start {
            return Some((0, self.child_column));
        }

        let mut current_column = self.child_column;

        for (idx, segment) in self.segments.iter().enumerate() {
            match segment {
                CommitLineSegment::Straight { to_row } => {
                    if *to_row >= first_visible_row {
                        return Some((idx, current_column));
                    }
                }
                CommitLineSegment::Curve {
                    to_column, on_row, ..
                } => {
                    if *on_row >= first_visible_row {
                        return Some((idx, current_column));
                    }
                    current_column = *to_column;
                }
            }
        }

        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct CommitLineKey {
    child: Oid,
    parent: Oid,
}

struct GraphData {
    lane_states: SmallVec<[LaneState; 8]>,
    lane_colors: HashMap<ActiveLaneIdx, BranchColor>,
    parent_to_lanes: HashMap<Oid, SmallVec<[usize; 1]>>,
    next_color: BranchColor,
    accent_colors_count: usize,
    commits: Vec<Rc<CommitEntry>>,
    max_commit_count: AllCommitCount,
    max_lanes: usize,
    lines: Vec<Rc<CommitLine>>,
    active_commit_lines: HashMap<CommitLineKey, usize>,
    active_commit_lines_by_parent: HashMap<Oid, SmallVec<[usize; 1]>>,
    ref_chip_context: RefChipContext,
    /// The classified chips of every loaded commit that has any.
    ref_chips: HashMap<Oid, Arc<[RefChip]>>,
}

impl GraphData {
    fn new(accent_colors_count: usize) -> Self {
        GraphData {
            lane_states: SmallVec::default(),
            lane_colors: HashMap::default(),
            parent_to_lanes: HashMap::default(),
            next_color: BranchColor(0),
            accent_colors_count,
            commits: Vec::default(),
            max_commit_count: AllCommitCount::NotLoaded,
            max_lanes: 0,
            lines: Vec::default(),
            active_commit_lines: HashMap::default(),
            active_commit_lines_by_parent: HashMap::default(),
            ref_chip_context: RefChipContext::default(),
            ref_chips: HashMap::default(),
        }
    }

    fn clear(&mut self) {
        self.lane_states.clear();
        self.lane_colors.clear();
        self.parent_to_lanes.clear();
        self.commits.clear();
        self.lines.clear();
        self.active_commit_lines.clear();
        self.active_commit_lines_by_parent.clear();
        self.ref_chips.clear();
        self.next_color = BranchColor(0);
        self.max_commit_count = AllCommitCount::NotLoaded;
        self.max_lanes = 0;
    }

    /// Classifies the loaded commits' chips again when the branch list says
    /// something new about which decorations are remote or checked out.
    fn set_ref_chip_context(&mut self, context: RefChipContext) {
        if self.ref_chip_context == context {
            return;
        }
        self.ref_chip_context = context;
        for commit in &self.commits {
            if !commit.data.ref_names.is_empty() {
                self.ref_chips.insert(
                    commit.data.sha,
                    classify_ref_chips(&commit.data.ref_names, &self.ref_chip_context),
                );
            }
        }
    }

    fn first_empty_lane_idx(&mut self) -> ActiveLaneIdx {
        self.lane_states
            .iter()
            .position(LaneState::is_empty)
            .unwrap_or_else(|| {
                self.lane_states.push(LaneState::Empty);
                self.lane_states.len() - 1
            })
    }

    fn get_lane_color(&mut self, lane_idx: ActiveLaneIdx) -> BranchColor {
        let accent_colors_count = self.accent_colors_count;
        *self.lane_colors.entry(lane_idx).or_insert_with(|| {
            let color_idx = self.next_color;
            self.next_color = BranchColor((self.next_color.0 + 1) % accent_colors_count as u8);
            color_idx
        })
    }

    fn add_commits(&mut self, commits: &[Arc<InitialGraphCommitData>]) {
        self.commits.reserve(commits.len());
        self.lines.reserve(commits.len() / 2);

        for commit in commits.iter() {
            let commit_row = self.commits.len();

            let commit_lane = self
                .parent_to_lanes
                .get(&commit.sha)
                .and_then(|lanes| lanes.iter().min().copied());

            let commit_lane = commit_lane.unwrap_or_else(|| self.first_empty_lane_idx());

            let commit_color = self.get_lane_color(commit_lane);

            if let Some(lanes) = self.parent_to_lanes.remove(&commit.sha) {
                for lane_column in lanes {
                    let state = &mut self.lane_states[lane_column];

                    if let LaneState::Active {
                        starting_row,
                        segments,
                        ..
                    } = state
                    {
                        if let Some(CommitLineSegment::Curve {
                            to_column,
                            curve_kind: CurveKind::Merge,
                            ..
                        }) = segments.first_mut()
                        {
                            let curve_row = *starting_row + 1;
                            let would_overlap =
                                if lane_column != commit_lane && curve_row < commit_row {
                                    self.commits[curve_row..commit_row]
                                        .iter()
                                        .any(|c| c.lane == commit_lane)
                                } else {
                                    false
                                };

                            if would_overlap {
                                *to_column = lane_column;
                            }
                        }
                    }

                    if let Some(commit_line) =
                        state.to_commit_lines(commit_row, lane_column, commit_lane, commit_color)
                    {
                        self.lines.push(Rc::new(commit_line));
                    }
                }
            }

            commit
                .parents
                .iter()
                .enumerate()
                .for_each(|(parent_idx, parent)| {
                    if parent_idx == 0 {
                        self.lane_states[commit_lane] = LaneState::Active {
                            parent: *parent,
                            child: commit.sha,
                            color: Some(commit_color),
                            starting_col: commit_lane,
                            starting_row: commit_row,
                            destination_column: None,
                            segments: smallvec![CommitLineSegment::Straight { to_row: usize::MAX }],
                        };

                        self.parent_to_lanes
                            .entry(*parent)
                            .or_default()
                            .push(commit_lane);
                    } else {
                        let new_lane = self.first_empty_lane_idx();

                        self.lane_states[new_lane] = LaneState::Active {
                            parent: *parent,
                            child: commit.sha,
                            color: None,
                            starting_col: commit_lane,
                            starting_row: commit_row,
                            destination_column: None,
                            segments: smallvec![CommitLineSegment::Curve {
                                to_column: usize::MAX,
                                on_row: usize::MAX,
                                curve_kind: CurveKind::Merge,
                            },],
                        };

                        self.parent_to_lanes
                            .entry(*parent)
                            .or_default()
                            .push(new_lane);
                    }
                });

            self.max_lanes = self.max_lanes.max(self.lane_states.len());

            if !commit.ref_names.is_empty() {
                self.ref_chips.insert(
                    commit.sha,
                    classify_ref_chips(&commit.ref_names, &self.ref_chip_context),
                );
            }

            self.commits.push(Rc::new(CommitEntry {
                data: commit.clone(),
                lane: commit_lane,
                color_idx: commit_color.0 as usize,
            }));
        }

        self.max_commit_count = AllCommitCount::Loading(self.commits.len());
    }
}

pub fn init(cx: &mut App) {
    workspace::register_serializable_item::<GitGraph>(cx);

    cx.observe_new(|workspace: &mut workspace::Workspace, _, _| {
        workspace.register_action_renderer(|div, workspace, window, cx| {
            div.when_some(
                resolve_file_history_target(workspace, window, cx),
                |div, (repo_id, log_source)| {
                    let git_store = workspace.project().read(cx).git_store().clone();
                    let workspace = workspace.weak_handle();

                    div.on_action(move |_: &git::FileHistory, window, cx| {
                        let git_store = git_store.clone();
                        workspace
                            .update(cx, |workspace, cx| {
                                open_or_reuse_graph(
                                    workspace,
                                    repo_id,
                                    git_store,
                                    log_source.clone(),
                                    None,
                                    window,
                                    cx,
                                );
                            })
                            .ok();
                    })
                },
            )
            .when(
                workspace.project().read(cx).active_repository(cx).is_some(),
                |div| {
                    let workspace = workspace.weak_handle();

                    div.on_action({
                        let workspace = workspace.clone();
                        move |_: &Open, window, cx| {
                            workspace
                                .update(cx, |workspace, cx| {
                                    let Some(repo) =
                                        workspace.project().read(cx).active_repository(cx)
                                    else {
                                        return;
                                    };
                                    let selected_repo_id = repo.read(cx).id;

                                    let git_store =
                                        workspace.project().read(cx).git_store().clone();
                                    open_or_reuse_graph(
                                        workspace,
                                        selected_repo_id,
                                        git_store,
                                        LogSource::All,
                                        None,
                                        window,
                                        cx,
                                    );
                                })
                                .ok();
                        }
                    })
                    .on_action(move |action: &OpenAtCommit, window, cx| {
                        let sha = action.sha.clone();
                        workspace
                            .update(cx, |workspace, cx| {
                                let Some(repo) = workspace.project().read(cx).active_repository(cx)
                                else {
                                    return;
                                };
                                let selected_repo_id = repo.read(cx).id;

                                let git_store = workspace.project().read(cx).git_store().clone();
                                open_or_reuse_graph(
                                    workspace,
                                    selected_repo_id,
                                    git_store,
                                    LogSource::All,
                                    Some(sha),
                                    window,
                                    cx,
                                );
                            })
                            .ok();
                    })
                },
            )
        });
    })
    .detach();
}

/// Resolves a `git::FileHistory` target from a known project path (used by
/// callers like `project_panel` that own a focused selection but cannot be
/// referenced from this module due to dependency direction).
pub fn resolve_file_history_target_from_project_path(
    workspace: &Workspace,
    project_path: &ProjectPath,
    cx: &App,
) -> Option<(RepositoryId, LogSource)> {
    let git_store = workspace.project().read(cx).git_store();
    let (repo, repo_path) = git_store
        .read(cx)
        .repository_and_path_for_project_path(project_path, cx)?;
    let log_source = if repo_path.is_empty() {
        LogSource::All
    } else {
        LogSource::Path(repo_path)
    };
    Some((repo.read(cx).id, log_source))
}

fn resolve_file_history_target(
    workspace: &Workspace,
    window: &Window,
    cx: &App,
) -> Option<(RepositoryId, LogSource)> {
    if let Some(panel) = workspace.panel::<crate::git_panel::GitPanel>(cx)
        && panel.read(cx).focus_handle(cx).contains_focused(window, cx)
        && let Some((repository, repo_path)) = panel.read(cx).selected_file_history_target()
    {
        return Some((repository.read(cx).id, LogSource::Path(repo_path)));
    }

    let editor = workspace.active_item_as::<Editor>(cx)?;

    let file = editor
        .read(cx)
        .file_at(editor.read(cx).selections.newest_anchor().head(), cx)?;
    let project_path = ProjectPath {
        worktree_id: file.worktree_id(cx),
        path: file.path().clone(),
    };

    let git_store = workspace.project().read(cx).git_store();
    let (repo, repo_path) = git_store
        .read(cx)
        .repository_and_path_for_project_path(&project_path, cx)?;
    Some((repo.read(cx).id, LogSource::Path(repo_path)))
}

pub fn open_or_reuse_graph(
    workspace: &mut Workspace,
    repo_id: RepositoryId,
    git_store: Entity<GitStore>,
    log_source: LogSource,
    sha: Option<String>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let existing = workspace.items_of_type::<GitGraph>(cx).find(|graph| {
        let graph = graph.read(cx);
        graph.repo_id == repo_id && graph.log_source == log_source
    });

    let git_graph = if let Some(existing) = existing {
        workspace.activate_item(&existing, true, true, window, cx);
        existing
    } else {
        let workspace_handle = workspace.weak_handle();
        let git_graph = cx.new(|cx| {
            GitGraph::new(
                repo_id,
                git_store,
                workspace_handle,
                Some(log_source),
                window,
                cx,
            )
        });
        workspace.add_item_to_active_pane(Box::new(git_graph.clone()), None, true, window, cx);
        git_graph
    };

    if let Some(sha) = sha {
        cx.defer(move |cx| {
            git_graph.update(cx, |graph, cx| {
                graph.select_commit_by_sha(sha.as_str(), cx);
            });
        });
    }
}

fn lane_center_x(bounds: Bounds<Pixels>, lane: f32) -> Pixels {
    bounds.origin.x + LEFT_PADDING + lane * LANE_WIDTH + LANE_WIDTH / 2.0
}

fn to_row_center(
    to_row: usize,
    row_height: Pixels,
    scroll_offset: Pixels,
    bounds: Bounds<Pixels>,
) -> Pixels {
    bounds.origin.y + to_row as f32 * row_height + row_height / 2.0 - scroll_offset
}

fn fill_circle(
    center_x: Pixels,
    center_y: Pixels,
    radius: Pixels,
    color: Hsla,
    window: &mut Window,
) {
    let mut builder = PathBuilder::fill();

    // Start at the rightmost point of the circle
    builder.move_to(point(center_x + radius, center_y));

    // Draw the circle using two arc_to calls (top half, then bottom half)
    builder.arc_to(
        point(radius, radius),
        px(0.),
        false,
        true,
        point(center_x - radius, center_y),
    );
    builder.arc_to(
        point(radius, radius),
        px(0.),
        false,
        true,
        point(center_x + radius, center_y),
    );
    builder.close();

    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }
}

/// Draws a commit node: a background-colored "halo" ring that masks any
/// connector lines passing through the lane, an outer ring in the branch
/// color, and a lighter center so the node reads as a clear, distinct point.
fn draw_commit_circle(
    center_x: Pixels,
    center_y: Pixels,
    color: Hsla,
    background: Hsla,
    window: &mut Window,
) {
    // Halo: clears space around the node so crossing lines don't touch it.
    fill_circle(
        center_x,
        center_y,
        COMMIT_CIRCLE_RADIUS + COMMIT_CIRCLE_HALO_WIDTH,
        background,
        window,
    );

    // Outer ring in the branch color.
    fill_circle(center_x, center_y, COMMIT_CIRCLE_RADIUS, color, window);

    // Inner fill makes the node read as a ring rather than a solid dot.
    fill_circle(
        center_x,
        center_y,
        (COMMIT_CIRCLE_RADIUS - COMMIT_CIRCLE_STROKE_WIDTH).max(px(0.)),
        background,
        window,
    );
}

fn compute_diff_stats(diff: &CommitDiff) -> (usize, usize) {
    diff.files.iter().fold((0, 0), |(added, removed), file| {
        let old_text = file.old_text.as_deref().unwrap_or("");
        let new_text = file.new_text.as_deref().unwrap_or("");
        let hunks = line_diff(old_text, new_text);
        hunks
            .iter()
            .fold((added, removed), |(a, r), (old_range, new_range)| {
                (
                    a + (new_range.end - new_range.start) as usize,
                    r + (old_range.end - old_range.start) as usize,
                )
            })
    })
}

struct GitGraphContextMenu {
    menu: Entity<ContextMenu>,
    position: Point<Pixels>,
    target_entry_index: Option<usize>,
    _subscription: Subscription,
}

struct DetailPanelCommitMessage {
    sha: Oid,
    message: Entity<Markdown>,
    scroll_handle: ScrollHandle,
}

/// What [`GitGraph::reveal_commit`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommitReveal {
    Selected,
    /// Selected once the rows still loading get to it.
    Pending,
    /// The graph showed one branch only and now shows all of them; selected once they load.
    ShowingAllBranches,
    NotInGraph,
}

pub struct GitGraph {
    focus_handle: FocusHandle,
    search_state: SearchState,
    graph_data: GraphData,
    git_store: Entity<GitStore>,
    workspace: WeakEntity<Workspace>,
    context_menu: Option<GitGraphContextMenu>,
    table_interaction_state: Entity<TableInteractionState>,
    column_widths: Entity<RedistributableColumnsState>,
    /// Per-column visibility mask owned by the view (not the resize state) so columns can be
    /// hidden regardless of whether the table is resizable. `true` means the column is hidden.
    column_visibility: TableRow<bool>,
    selected_entry_idx: Option<usize>,
    hovered_entry_idx: Option<usize>,
    rewritable_history_cache: Option<CachedRewritableHistory>,
    /// The branch chip a dragged commit hangs over: its row, branch name and
    /// kind. A chip keeps the row behind it from being hovered during such a
    /// drag, so this is how the row learns to show the chip's note instead of
    /// its own.
    hovered_drop_chip: Option<(usize, SharedString, RefKind)>,
    graph_canvas_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    log_source: LogSource,
    log_order: LogOrder,
    selected_commit_diff: Option<CommitDiff>,
    selected_commit_diff_stats: Option<(usize, usize)>,
    _commit_diff_task: Option<Task<()>>,
    selected_commit_message: Option<DetailPanelCommitMessage>,
    _selected_commit_message_task: Option<Task<()>>,
    commit_details_split_state: Entity<SplitState>,
    repo_id: RepositoryId,
    changed_files_scroll_handle: UniformListScrollHandle,
    changed_files_view_mode: ChangedFilesViewMode,
    commit_details_metadata_collapsed: bool,
    commit_details_message_collapsed: bool,
    changed_files_expanded_dirs: HashMap<RepoPath, bool>,
    pending_select_sha: Option<Oid>,
    /// Rows picked with Ctrl / Shift click. Always contains
    /// `selected_entry_idx` while anything is picked; empty means the graph is
    /// back to single selection.
    multi_selection: BTreeSet<usize>,
    /// Where the next Shift click measures its range from.
    selection_anchor_idx: Option<usize>,
    /// `git describe` for the selected commit, so the detail panel can name the
    /// tag it came after. The `Oid` is what the answer was asked for.
    selected_commit_tag_distance: Option<(Oid, Option<TagDistance>)>,
    _selected_commit_tag_distance_task: Option<Task<()>>,
    /// The repository's tags, newest first, loaded with the toolbar. Shared
    /// rather than copied, since there can be tens of thousands; a reload keeps
    /// showing this list until the new one is in.
    tags: Option<Arc<[TagInfo]>>,
    _tags_task: Option<Task<()>>,
    /// The commit whose detail panel shows every ref chip rather than the
    /// first [`DETAIL_REF_CHIP_LIMIT`].
    detail_ref_chips_expanded_for: Option<Oid>,
    /// Kept while closed, so that closing the box never throws away what is in it.
    merge_request_box: Option<Entity<MergeRequestBox>>,
    merge_request_box_open: bool,
    /// The box's changes redraw the graph too: its rows carry the box's marks.
    _merge_request_box_observation: Option<Subscription>,
    guide: action_bar::GraphGuideState,
    nav_history: Option<ItemNavHistory>,
}

impl GitGraph {
    fn invalidate_state(&mut self, cx: &mut Context<Self>) {
        self.graph_data.clear();
        self.multi_selection.clear();
        self.selection_anchor_idx = None;
        // The tags are kept: moving HEAD or a branch does not change them, and
        // `RepositoryEvent::TagsChanged` says when something did.
        self.selected_commit_tag_distance = None;
        if let Some(repository) = self.get_repository(cx) {
            self.refresh_ref_chip_context(&repository, cx);
        }
        self.search_state.matches.clear();
        self.search_state.selected_index = None;
        self.search_again_after_change();
        self.context_menu = None;
        cx.emit(ItemEvent::Edit);
        cx.notify();
    }

    /// Computes the height of a single commit row in the git graph.
    ///
    /// The returned value is snapped to the nearest physical pixel. This is
    /// required so that the canvas's float math and the `uniform_list` layout
    /// (which snaps to device pixels) agree on row positions; otherwise rows
    /// drift apart as the user scrolls when `ui_font_size` is fractional.
    fn row_height(window: &Window, _cx: &App) -> Pixels {
        let rem_size = window.rem_size();
        let line_height = window.text_style().line_height_in_pixels(rem_size);
        let raw = line_height + ROW_VERTICAL_PADDING;
        let scale = window.scale_factor();

        (raw * scale).round() / scale
    }

    fn visible_row_count(&self, window: &Window, cx: &App) -> usize {
        let row_height = Self::row_height(window, cx);
        let viewport_height = self
            .table_interaction_state
            .read(cx)
            .scroll_handle
            .0
            .borrow()
            .last_item_size
            .map_or(window.viewport_size().height, |size| size.item.height);

        ((viewport_height / row_height).ceil() as usize).min(self.graph_data.commits.len())
    }

    fn graph_canvas_content_width(&self) -> Pixels {
        (LANE_WIDTH * self.graph_data.max_lanes.max(6) as f32) + LEFT_PADDING * 2.0
    }

    /// Returns the column fractions in display order:
    /// `[graph, description, date, author, commit]`.
    /// For path history there is no graph column, so its fraction is 0.
    fn preview_column_fractions(&self, window: &Window, cx: &App) -> [f32; 5] {
        let raw = self
            .column_widths
            .read(cx)
            .preview_fractions(window.rem_size());
        let fractions = redistribute_hidden_fractions(&raw, Some(&self.column_visibility));

        // Hidden columns occupy no space in the layout, so report them as zero here even though
        // the shared redistribution helper preserves their stored width for when they return.
        let value = |idx: usize| {
            if self.column_visibility.get(idx).copied().unwrap_or(false) {
                0.0
            } else {
                fractions[idx]
            }
        };

        let is_path_history = matches!(self.log_source, LogSource::Path(_));
        let graph_fraction = if is_path_history { 0.0 } else { value(0) };
        let offset = if is_path_history { 0 } else { 1 };

        [
            graph_fraction,
            value(offset),
            value(offset + 1),
            value(offset + 2),
            value(offset + 3),
        ]
    }

    // How long a date is depends on the display language and the OS's date format, so the date
    // column is widened to fit one, taking the width from the description column.
    fn fit_date_column(&mut self, window: &Window, cx: &mut Context<Self>) {
        let description_index = if matches!(self.log_source, LogSource::Path(_)) {
            0
        } else {
            1
        };
        let date_index = description_index + 1;
        let is_hidden = |index: usize| self.column_visibility.get(index).copied().unwrap_or(false);
        if is_hidden(date_index) || is_hidden(description_index) {
            return;
        }
        let Some(min_width) = date_column_min_width(window, cx) else {
            return;
        };
        let rem_size = window.rem_size();
        self.column_widths.update(cx, |columns, cx| {
            if columns.widen_column_to(date_index, min_width, description_index, rem_size) {
                cx.notify();
            }
        });
    }

    fn table_column_width_config(&self, window: &Window, cx: &App) -> ColumnWidthConfig {
        let [_, description, date, author, commit] = self.preview_column_fractions(window, cx);
        let table_total = description + date + author + commit;

        let widths = if table_total > 0.0 {
            vec![
                DefiniteLength::Fraction(description / table_total),
                DefiniteLength::Fraction(date / table_total),
                DefiniteLength::Fraction(author / table_total),
                DefiniteLength::Fraction(commit / table_total),
            ]
        } else {
            vec![DefiniteLength::Fraction(0.25); 4]
        };

        ColumnWidthConfig::explicit(widths)
    }

    fn graph_viewport_width(&self, window: &Window, cx: &App) -> Pixels {
        let container = self.column_widths.read(cx).cached_container_width();
        let graph_fraction = self.preview_column_fractions(window, cx)[0];
        if container > px(0.) && graph_fraction > 0.0 {
            container * graph_fraction
        } else {
            self.graph_canvas_content_width()
        }
    }

    pub fn new(
        repo_id: RepositoryId,
        git_store: Entity<GitStore>,
        workspace: WeakEntity<Workspace>,
        log_source: Option<LogSource>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        cx.on_focus(&focus_handle, window, |_, _, cx| cx.notify())
            .detach();

        let accent_colors = cx.theme().accents();
        let graph = GraphData::new(accent_colors_count(accent_colors));
        let log_source = log_source.unwrap_or_default();
        let log_order = LogOrder::default();

        cx.subscribe(&git_store, |this, _, event, cx| match event {
            GitStoreEvent::RepositoryUpdated(updated_repo_id, repo_event, _) => {
                if this.repo_id == *updated_repo_id {
                    if let Some(repository) = this.get_repository(cx) {
                        this.on_repository_event(repository, repo_event, cx);
                    }
                }
            }
            _ => {}
        })
        .detach();

        let search_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text(i18n::tr(graph_search::SEARCH_PLACEHOLDER), window, cx);
            editor
        });

        let table_interaction_state = cx.new(|cx| {
            let mut state = TableInteractionState::new(cx);
            state.focus_handle = state.focus_handle.tab_index(1).tab_stop(true);
            state
        });

        let column_widths = if matches!(log_source, LogSource::Path(_)) {
            cx.new(|_cx| {
                RedistributableColumnsState::new(
                    4,
                    vec![
                        DefiniteLength::Fraction(0.64),
                        DefiniteLength::Fraction(0.14),
                        DefiniteLength::Fraction(0.12),
                        DefiniteLength::Fraction(0.10),
                    ],
                    vec![TableResizeBehavior::Resizable; 4],
                )
            })
        } else {
            cx.new(|_cx| {
                RedistributableColumnsState::new(
                    5,
                    vec![
                        DefiniteLength::Fraction(0.14),
                        DefiniteLength::Fraction(0.5504),
                        DefiniteLength::Fraction(0.1204),
                        DefiniteLength::Fraction(0.1032),
                        DefiniteLength::Fraction(0.086),
                    ],
                    vec![TableResizeBehavior::Resizable; 5],
                )
            })
        };
        let column_visibility = TableRow::from_element(
            false,
            if matches!(log_source, LogSource::Path(_)) {
                TABLE_COLUMN_COUNT
            } else {
                TABLE_COLUMN_COUNT + 1
            },
        );
        let mut row_height = Self::row_height(window, cx);

        cx.observe_global_in::<settings::SettingsStore>(window, move |this, window, cx| {
            let new_row_height = Self::row_height(window, cx);
            if new_row_height != row_height {
                // The `uniform_list` powering the table caches the item size
                // from its last layout; invalidate it so it re-measures with
                // the new row height on the next frame.
                this.table_interaction_state.update(cx, |state, _cx| {
                    state.scroll_handle.0.borrow_mut().last_item_size = None;
                });
                row_height = new_row_height;
                cx.notify();
            }
        })
        .detach();
        // The loose-refs banner's button follows packing started from any graph or the palette.
        cx.observe_global::<crate::many_refs::PackRefsInProgress>(|_, cx| cx.notify())
            .detach();

        let guide = action_bar::GraphGuideState::new(&log_source, cx);
        let mut this = GitGraph {
            focus_handle,
            git_store,
            search_state: SearchState {
                case_sensitive: false,
                editor: search_editor,
                matches: IndexSet::default(),
                selected_index: None,
                state: QueryState::Empty,
                outcome: graph_search::SearchOutcome::default(),
            },
            workspace,
            graph_data: graph,
            _commit_diff_task: None,
            context_menu: None,
            table_interaction_state,
            column_widths,
            column_visibility,
            selected_entry_idx: None,
            hovered_entry_idx: None,
            rewritable_history_cache: None,
            hovered_drop_chip: None,
            graph_canvas_bounds: Rc::new(Cell::new(None)),
            selected_commit_diff: None,
            selected_commit_diff_stats: None,
            selected_commit_message: None,
            _selected_commit_message_task: None,
            log_source,
            log_order,
            commit_details_split_state: cx.new(|_cx| SplitState::new()),
            repo_id,
            changed_files_scroll_handle: UniformListScrollHandle::new(),
            changed_files_view_mode: ChangedFilesViewMode::default(),
            commit_details_metadata_collapsed: false,
            commit_details_message_collapsed: false,
            changed_files_expanded_dirs: HashMap::default(),
            pending_select_sha: None,
            multi_selection: BTreeSet::default(),
            selection_anchor_idx: None,
            selected_commit_tag_distance: None,
            _selected_commit_tag_distance_task: None,
            tags: None,
            _tags_task: None,
            detail_ref_chips_expanded_for: None,
            merge_request_box: None,
            merge_request_box_open: false,
            _merge_request_box_observation: None,
            guide,
            nav_history: None,
        };

        if let Some(repository) = this.get_repository(cx) {
            this.refresh_ref_chip_context(&repository, cx);
        }
        this.fetch_initial_graph_data(cx);
        this
    }

    /// Takes the checked-out branch and the remotes from the branch list, which
    /// is the only time they can change, so that no frame has to walk it.
    fn refresh_ref_chip_context(&mut self, repository: &Entity<Repository>, cx: &App) {
        let repository = repository.read(cx);
        self.graph_data.set_ref_chip_context(RefChipContext {
            head_branch_name: repository
                .branch
                .as_ref()
                .map(|branch| SharedString::from(branch.name().to_string())),
            remote_names: remote_names_from_branches(&repository.branch_list),
        });
    }

    fn on_repository_event(
        &mut self,
        repository: Entity<Repository>,
        event: &RepositoryEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            RepositoryEvent::GraphEvent((source, order), event)
                if source == &self.log_source && order == &self.log_order =>
            {
                match event {
                    GitGraphEvent::FullyLoaded => {
                        if let Some(pending_sha_index) =
                            self.pending_select_sha.take().and_then(|oid| {
                                repository
                                    .read(cx)
                                    .get_graph_data(source.clone(), *order)
                                    .and_then(|data| data.commit_oid_to_index.get(&oid).copied())
                            })
                        {
                            self.select_entry(pending_sha_index, ScrollStrategy::Nearest, cx);
                        }
                        let count = match self.graph_data.max_commit_count {
                            AllCommitCount::FullyLoaded(count) | AllCommitCount::Loading(count) => {
                                count
                            }
                            AllCommitCount::NotLoaded => 0,
                        };
                        self.graph_data.max_commit_count = AllCommitCount::FullyLoaded(count);
                        cx.notify();
                    }
                    GitGraphEvent::LoadingError => {
                        cx.notify();
                    }
                    GitGraphEvent::CountUpdated(commit_count) => {
                        let old_count = self.graph_data.commits.len();

                        if let Some(pending_selection_index) =
                            repository.update(cx, |repository, cx| {
                                let GraphDataResponse {
                                    commits,
                                    is_loading,
                                    error: _,
                                } = repository.graph_data(
                                    source.clone(),
                                    *order,
                                    old_count..*commit_count,
                                    cx,
                                );
                                self.graph_data.add_commits(commits);

                                let pending_sha_index = self.pending_select_sha.and_then(|oid| {
                                    repository.get_graph_data(source.clone(), *order).and_then(
                                        |data| data.commit_oid_to_index.get(&oid).copied(),
                                    )
                                });

                                if !is_loading && pending_sha_index.is_none() {
                                    self.pending_select_sha.take();
                                }

                                pending_sha_index
                            })
                        {
                            self.select_entry(pending_selection_index, ScrollStrategy::Nearest, cx);
                            self.pending_select_sha.take();
                        }

                        cx.notify();
                    }
                }
            }
            RepositoryEvent::HeadChanged | RepositoryEvent::BranchListChanged => {
                // A host that cannot tell when its tags change (one too old to
                // say) gets its tags reloaded along with the branches instead.
                if repository.read(cx).tags_fingerprint.is_none() && self.tags.is_some() {
                    self.load_tags(cx);
                }
                // Only invalidate if we scanned atleast once,
                // meaning we are not inside the initial repo loading state
                // NOTE: this fixes an loading performance regression
                if repository.read(cx).scan_id > 1 {
                    self.pending_select_sha = None;
                    self.invalidate_state(cx);
                } else {
                    self.refresh_ref_chip_context(&repository, cx);
                }
            }
            RepositoryEvent::TagsChanged => {
                if self.tags.is_some() || self._tags_task.is_some() {
                    self.load_tags(cx);
                }
                // The rows' tag chips come with the log, which has to be read
                // again for a tag added or deleted outside Zed to show up.
                if repository.read(cx).scan_id > 1 {
                    self.pending_select_sha = None;
                    self.invalidate_state(cx);
                }
            }
            RepositoryEvent::StashEntriesChanged if self.log_source == LogSource::All => {
                // Stash entries initial's scan id is 2, so we don't want to invalidate the graph before that
                if repository.read(cx).scan_id > 2 {
                    self.pending_select_sha = None;
                    self.invalidate_state(cx);
                }
            }
            RepositoryEvent::GraphEvent(_, _) => {}
            _ => {}
        }
    }

    fn fetch_initial_graph_data(&mut self, cx: &mut App) {
        if let Some(repository) = self.get_repository(cx) {
            repository.update(cx, |repository, cx| {
                let commits = repository
                    .graph_data(self.log_source.clone(), self.log_order, 0..usize::MAX, cx)
                    .commits;
                self.graph_data.add_commits(commits);
            });
        }
    }

    fn get_repository(&self, cx: &App) -> Option<Entity<Repository>> {
        let git_store = self.git_store.read(cx);
        git_store.repositories().get(&self.repo_id).cloned()
    }

    /// Renders one ref chip. The icon, colour and tooltip come from `kind` so
    /// that a tag, a remote branch and the checked-out branch cannot be
    /// confused with one another.
    fn render_chip(
        &self,
        name: &SharedString,
        accent_color: gpui::Hsla,
        kind: Option<RefKind>,
        cx: &App,
    ) -> impl IntoElement {
        let tooltip_text: SharedString = match kind {
            Some(kind) => format!("{name} — {}", kind.description()).into(),
            None => name.clone(),
        };
        let tag_color = Color::Created.color(cx);
        Chip::new(name.clone())
            .label_size(LabelSize::Small)
            .truncate()
            .tooltip(move |_, cx| Tooltip::simple(tooltip_text.clone(), cx))
            .map(|chip| {
                let Some(kind) = kind else {
                    return chip
                        .bg_color(accent_color.opacity(0.08))
                        .border_color(accent_color.opacity(0.25));
                };
                let chip = chip.icon(kind.icon());
                match kind {
                    RefKind::Head => chip
                        .bg_color(accent_color.opacity(0.25))
                        .border_color(accent_color.opacity(0.5)),
                    RefKind::LocalBranch => chip
                        .bg_color(accent_color.opacity(0.08))
                        .border_color(accent_color.opacity(0.25)),
                    RefKind::RemoteBranch => chip
                        .icon_color(Color::Muted)
                        .label_color(Color::Muted)
                        .bg_color(accent_color.opacity(0.04))
                        .border_color(accent_color.opacity(0.14)),
                    RefKind::Tag => chip
                        .icon_color(Color::Created)
                        .bg_color(tag_color.opacity(0.1))
                        .border_color(tag_color.opacity(0.3)),
                }
            })
    }

    /// Renders a ref chip for the commit at `commit_idx`. Chips that name a ref
    /// (branch, remote ref, or tag) get a right-click handler that opens a
    /// ref-specific context menu, so that custom commands can be resolved
    /// against the clicked ref. A branch chip is also the handle for dragging
    /// the branch onto another commit, and a drop target for a dragged commit.
    fn render_ref_chip(
        &self,
        decoration: &SharedString,
        accent_color: gpui::Hsla,
        classified: Option<(RefKind, SharedString)>,
        commit_idx: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let chip = self.render_chip(
            decoration,
            accent_color,
            classified.as_ref().map(|(kind, _)| *kind),
            cx,
        );
        let Some((kind, ref_name)) = classified else {
            return chip.into_any_element();
        };
        let movable = kind.is_movable_branch();
        let commit_is_dragged = cx.active_drag_is::<DraggedCommit>();
        div()
            .id(ElementId::NamedInteger(
                SharedString::from(format!("ref-chip-{ref_name}")),
                commit_idx as u64,
            ))
            .min_w_0()
            .child(chip)
            .when(movable, |this| {
                let dragged = DraggedRef {
                    name: ref_name.clone(),
                    kind,
                };
                this.on_drag(dragged, |dragged: &DraggedRef, _, _, cx| {
                    let icon = dragged.kind.icon();
                    let text = dragged.name.clone();
                    cx.new(|_| DraggedGraphLabel { icon, text })
                })
                // While a commit is dragged the chip and the row behind it are
                // different drop targets (cherry-pick and reorder), so the chip
                // keeps the row from lighting up and labelling itself as well.
                .when(commit_is_dragged, |this| this.block_mouse_except_scroll())
                .drag_over::<DraggedCommit>(|style, _, _, cx| {
                    style.bg(cx.theme().colors().drop_target_background)
                })
                .on_drag_move::<DraggedCommit>(cx.listener({
                    let ref_name = ref_name.clone();
                    move |this, event: &DragMoveEvent<DraggedCommit>, _window, cx| {
                        let chip = (commit_idx, ref_name.clone(), kind);
                        let is_over = event.bounds.contains(&event.event.position);
                        if is_over && this.hovered_drop_chip.as_ref() != Some(&chip) {
                            this.hovered_drop_chip = Some(chip);
                            cx.notify();
                        } else if !is_over && this.hovered_drop_chip.as_ref() == Some(&chip) {
                            this.hovered_drop_chip = None;
                            cx.notify();
                        }
                    }
                }))
                .on_drop(cx.listener({
                    let ref_name = ref_name.clone();
                    move |this, dragged: &DraggedCommit, window, cx| {
                        this.hovered_drop_chip = None;
                        this.handle_graph_drop(
                            GraphDragEnd::dragged_commit(dragged),
                            GraphDragEnd::Ref {
                                name: ref_name.clone(),
                                kind,
                            },
                            window,
                            cx,
                        );
                    }
                }))
            })
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.deploy_entry_context_menu(
                        event.position,
                        commit_idx,
                        Some(ref_name.clone()),
                        window,
                        cx,
                    );
                    cx.stop_propagation();
                }),
            )
            .into_any_element()
    }

    /// A row's chips: the first few, then one "+N" chip whose tooltip names
    /// the rest.
    fn render_row_ref_chips(
        &self,
        chips: &[RefChip],
        accent_color: gpui::Hsla,
        commit_idx: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (shown, hidden_count) = visible_ref_chips(chips, ROW_REF_CHIP_LIMIT);
        h_flex()
            .gap_1()
            .children(shown.iter().map(|chip| {
                self.render_ref_chip(
                    &chip.decoration,
                    accent_color,
                    chip.classified.clone(),
                    commit_idx,
                    cx,
                )
            }))
            .when(hidden_count > 0, |this| {
                let tooltip = SharedString::from(hidden_ref_chips_tooltip(
                    chips.get(ROW_REF_CHIP_LIMIT..).unwrap_or_default(),
                ));
                this.child(
                    div()
                        .id(ElementId::NamedInteger(
                            "hidden-ref-chips".into(),
                            commit_idx as u64,
                        ))
                        .debug_selector(|| format!("hidden-ref-chips-{commit_idx}"))
                        .flex_none()
                        .child(
                            Chip::new(format!("+{hidden_count}"))
                                .label_size(LabelSize::Small)
                                .bg_color(accent_color.opacity(0.04))
                                .border_color(accent_color.opacity(0.14)),
                        )
                        .tooltip(move |_, cx| Tooltip::simple(tooltip.clone(), cx)),
                )
            })
            .into_any_element()
    }

    /// The detail panel's chips: the first [`DETAIL_REF_CHIP_LIMIT`] with a
    /// button for the rest, or all of them once that has been clicked.
    fn render_detail_ref_chips(
        &self,
        sha: Oid,
        chips: &[RefChip],
        accent_color: gpui::Hsla,
        commit_idx: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let expanded = self.detail_ref_chips_expanded_for == Some(sha);
        let limit = if expanded {
            chips.len()
        } else {
            DETAIL_REF_CHIP_LIMIT
        };
        let (shown, hidden_count) = visible_ref_chips(chips, limit);
        h_flex()
            .gap_1()
            .flex_wrap()
            .justify_center()
            .children(shown.iter().map(|chip| {
                self.render_ref_chip(
                    &chip.decoration,
                    accent_color,
                    chip.classified.clone(),
                    commit_idx,
                    cx,
                )
            }))
            .when(hidden_count > 0, |this| {
                this.child(
                    div()
                        .debug_selector(|| "git-graph-show-all-ref-chips".into())
                        .child(
                            Button::new("show-all-ref-chips", i18n::t!("Show {hidden_count} More"))
                                .label_size(LabelSize::Small)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.detail_ref_chips_expanded_for = Some(sha);
                                    cx.notify();
                                })),
                        ),
                )
            })
            .when(expanded && chips.len() > DETAIL_REF_CHIP_LIMIT, |this| {
                this.child(
                    Button::new("collapse-ref-chips", i18n::t!("Collapse"))
                        .label_size(LabelSize::Small)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.detail_ref_chips_expanded_for = None;
                            cx.notify();
                        })),
                )
            })
            .into_any_element()
    }

    fn render_table_rows(
        &mut self,
        range: Range<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<Vec<AnyElement>> {
        let repository = self.get_repository(cx);
        let multi_selection_oldest_first: Arc<[Oid]> = self
            .multi_selection
            .iter()
            .rev()
            .filter_map(|row| self.graph_data.commits.get(*row))
            .map(|commit| commit.data.sha)
            .collect();
        let merge_request_marks = self.merge_request_row_marks(cx);

        let row_height = Self::row_height(window, cx);

        let remote = repository.as_ref().and_then(|repository| {
            let remote_url = repository.read(cx).default_remote_url()?;
            let provider_registry = GitHostingProviderRegistry::default_global(cx);
            let (provider, parsed) = parse_git_remote_url(provider_registry, &remote_url)?;
            Some(GitRemote {
                host: provider,
                owner: parsed.owner.into(),
                repo: parsed.repo.into(),
            })
        });

        // We fetch data outside the visible viewport to avoid loading entries when
        // users scroll through the git graph
        if let Some(repository) = repository.as_ref() {
            const FETCH_RANGE: usize = 100;
            repository.update(cx, |repository, cx| {
                self.graph_data.commits[range.start.saturating_sub(FETCH_RANGE)
                    ..(range.end + FETCH_RANGE)
                        .min(self.graph_data.commits.len().saturating_sub(1))]
                    .iter()
                    .for_each(|commit| {
                        repository.fetch_commit_data(commit.data.sha, false, cx);
                    });
            });
        }

        range
            .map(|idx| {
                let Some((commit, repository)) =
                    self.graph_data.commits.get(idx).zip(repository.as_ref())
                else {
                    return (0..4)
                        .map(|_| div().h(row_height).into_any_element())
                        .collect();
                };

                let data = repository.update(cx, |repository, cx| {
                    repository
                        .fetch_commit_data(commit.data.sha, false, cx)
                        .clone()
                });

                let short_sha = commit.data.sha.display_short();
                let mut formatted_time = String::new();
                let subject: SharedString;
                let author_name: Option<SharedString>;
                let author_email: Option<SharedString>;

                if let CommitDataState::Loaded(ref data) = data {
                    subject = data.subject.clone();
                    author_name = (!data.author_name.is_empty()).then(|| data.author_name.clone());
                    author_email =
                        (!data.author_email.is_empty()).then(|| data.author_email.clone());
                    formatted_time = format_timestamp(data.commit_timestamp);
                } else {
                    subject = i18n::t!("Loading…").into();
                    author_name = None;
                    author_email = None;
                }

                let accent_colors = cx.theme().accents();
                let accent_color = accent_colors
                    .0
                    .get(commit.color_idx)
                    .copied()
                    .unwrap_or_else(|| accent_colors.0.first().copied().unwrap_or_default());

                let is_selected = self.selected_entry_idx == Some(idx);
                let is_matched = self.search_state.matches.contains(&commit.data.sha);
                let column_label = |label: SharedString| {
                    Label::new(label)
                        .when(!is_selected, |c| c.color(Color::Muted))
                        .truncate()
                        .into_any_element()
                };

                let dragged_commit = DraggedCommit {
                    sha: commit.data.sha,
                    subject: subject.clone(),
                    selection: if self.multi_selection.contains(&idx) {
                        multi_selection_oldest_first.clone()
                    } else {
                        Arc::from([commit.data.sha])
                    },
                };
                let subject_label = if is_matched {
                    // Only the words that are not conditions were looked for in the messages.
                    let query = match &self.search_state.state {
                        QueryState::Confirmed(_) => {
                            Some(self.search_state.outcome.message_text.clone())
                                .filter(|text| !text.is_empty())
                        }
                        _ => None,
                    };
                    let highlight_ranges = query
                        .and_then(|q| {
                            let ranges = if self.search_state.case_sensitive {
                                subject
                                    .match_indices(q.as_str())
                                    .map(|(start, matched)| start..start + matched.len())
                                    .collect::<Vec<_>>()
                            } else {
                                let q = q.to_lowercase();
                                let subject_lower = subject.to_lowercase();

                                subject_lower
                                    .match_indices(&q)
                                    .filter_map(|(start, matched)| {
                                        let end = start + matched.len();
                                        subject.is_char_boundary(start).then_some(()).and_then(
                                            |_| subject.is_char_boundary(end).then_some(start..end),
                                        )
                                    })
                                    .collect::<Vec<_>>()
                            };

                            (!ranges.is_empty()).then_some(ranges)
                        })
                        .unwrap_or_default();
                    HighlightedLabel::from_ranges(subject, highlight_ranges)
                        .when(!is_selected, |c| c.color(Color::Muted))
                        .truncate()
                        .into_any_element()
                } else {
                    column_label(subject)
                };
                let merge_request_mark = merge_request_marks
                    .as_ref()
                    .and_then(|marks| marks.mark(&commit.data.sha));
                let show_drag_grip = self.hovered_entry_idx == Some(idx);

                vec![
                    div()
                        .id(ElementId::NamedInteger("commit-subject".into(), idx as u64))
                        .overflow_hidden()
                        .child(
                            h_flex()
                                .gap_2()
                                .overflow_hidden()
                                .children(
                                    self.graph_data
                                        .ref_chips
                                        .get(&commit.data.sha)
                                        .cloned()
                                        .map(|chips| {
                                            self.render_row_ref_chips(&chips, accent_color, idx, cx)
                                        }),
                                )
                                .children(
                                    merge_request_mark.and_then(|mark| {
                                        merge_request_box::render_row_mark(mark, cx)
                                    }),
                                )
                                // The subject is the drag handle for the commit
                                // itself, so that dragging a ref chip inside the
                                // same cell stays unambiguous.
                                .child(
                                    div()
                                        .id(ElementId::NamedInteger(
                                            "commit-drag-handle".into(),
                                            idx as u64,
                                        ))
                                        .flex_1()
                                        .min_w_0()
                                        .overflow_hidden()
                                        .flex()
                                        .items_center()
                                        .gap_1()
                                        .when(
                                            merge_request_mark == Some(RowMark::Excluded),
                                            |this| this.opacity(0.45),
                                        )
                                        .on_drag(
                                            dragged_commit,
                                            |dragged: &DraggedCommit, _, _, cx| {
                                                let text = SharedString::from(format!(
                                                    "{} {}",
                                                    dragged.sha.display_short(),
                                                    dragged.subject
                                                ));
                                                cx.new(|_| DraggedGraphLabel {
                                                    icon: IconName::GitCommit,
                                                    text,
                                                })
                                            },
                                        )
                                        .child(action_bar::commit_drag_grip(idx, show_drag_grip))
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .overflow_hidden()
                                                .child(subject_label),
                                        ),
                                ),
                        )
                        .into_any_element(),
                    column_label(formatted_time.into()),
                    {
                        let avatar_sha: SharedString = commit.data.sha.to_string().into();
                        let avatar = CommitAvatar::new(&avatar_sha, author_email, remote.as_ref())
                            .size(px(16.))
                            .render(window, cx);
                        h_flex()
                            .id(ElementId::NamedInteger("commit-author".into(), idx as u64))
                            .h(row_height)
                            .items_center()
                            .gap_1p5()
                            .overflow_hidden()
                            .child(avatar)
                            .when_some(author_name, |this, author_name| {
                                this.child(column_label(author_name))
                            })
                            .into_any_element()
                    },
                    column_label(short_sha.into()),
                ]
            })
            .collect()
    }

    fn cancel(&mut self, _: &Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        self.selected_entry_idx = None;
        self.multi_selection.clear();
        self.selection_anchor_idx = None;
        self.selected_commit_tag_distance = None;
        self.selected_commit_diff = None;
        self.selected_commit_diff_stats = None;
        self.changed_files_expanded_dirs.clear();
        cx.emit(ItemEvent::Edit);
        cx.notify();
    }

    fn select_first(&mut self, _: &SelectFirst, _window: &mut Window, cx: &mut Context<Self>) {
        self.select_entry(0, ScrollStrategy::Nearest, cx);
    }

    fn select_prev(&mut self, _: &SelectPrevious, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(selected_entry_idx) = &self.selected_entry_idx {
            self.select_entry(
                selected_entry_idx.saturating_sub(1),
                ScrollStrategy::Nearest,
                cx,
            );
        } else {
            self.select_first(&SelectFirst, window, cx);
        }
    }

    fn select_next(&mut self, _: &SelectNext, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(selected_entry_idx) = &self.selected_entry_idx {
            self.select_entry(
                selected_entry_idx
                    .saturating_add(1)
                    .min(self.graph_data.commits.len().saturating_sub(1)),
                ScrollStrategy::Nearest,
                cx,
            );
        } else {
            self.select_prev(&SelectPrevious, window, cx);
        }
    }

    fn select_last(&mut self, _: &SelectLast, _window: &mut Window, cx: &mut Context<Self>) {
        self.select_entry(
            self.graph_data.commits.len().saturating_sub(1),
            ScrollStrategy::Nearest,
            cx,
        );
    }

    fn scroll_up(&mut self, _: &ScrollUp, window: &mut Window, cx: &mut Context<Self>) {
        let step = (self.visible_row_count(window, cx) / 2).max(1);
        let target_idx = self.selected_entry_idx.unwrap_or(0).saturating_sub(step);

        self.select_entry(target_idx, ScrollStrategy::Nearest, cx);
    }

    fn scroll_down(&mut self, _: &ScrollDown, window: &mut Window, cx: &mut Context<Self>) {
        let Some(last_entry_idx) = self.graph_data.commits.len().checked_sub(1) else {
            return;
        };

        let step = (self.visible_row_count(window, cx) / 2).max(1);
        let target_idx = self
            .selected_entry_idx
            .unwrap_or(0)
            .saturating_add(step)
            .min(last_entry_idx);

        self.select_entry(target_idx, ScrollStrategy::Nearest, cx);
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        self.open_selected_commit_view(window, cx);
    }

    fn toggle_changed_files_view(
        &mut self,
        _: &ToggleChangedFilesView,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.changed_files_view_mode = self.changed_files_view_mode.toggled();
        self.changed_files_scroll_handle
            .scroll_to_item(0, ScrollStrategy::Top);
        cx.notify();
    }

    fn confirm_search(&mut self, _: &menu::Confirm, _window: &mut Window, cx: &mut Context<Self>) {
        let query = self.search_state.editor.read(cx).text(cx).into();
        self.search(query, cx);
    }

    fn activate_search_editor_if_focused(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.search_state.editor.update(cx, |editor, cx| {
            if editor.is_focused(window) {
                editor.select_all(&Default::default(), window, cx);
                editor.show_cursor(cx);
            }
        });
    }

    fn focus_next_tab_stop(
        &mut self,
        _: &FocusNextTabStop,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus_next(cx);
        self.activate_search_editor_if_focused(window, cx);
        cx.stop_propagation();
        cx.notify();
    }

    fn focus_previous_tab_stop(
        &mut self,
        _: &FocusPreviousTabStop,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus_prev(cx);
        self.activate_search_editor_if_focused(window, cx);
        cx.stop_propagation();
        cx.notify();
    }

    fn select_entry(
        &mut self,
        idx: usize,
        scroll_strategy: ScrollStrategy,
        cx: &mut Context<Self>,
    ) {
        if self.selected_entry_idx == Some(idx) || idx >= self.graph_data.commits.len() {
            debug_assert!(
                idx < self.graph_data.commits.len(),
                "attempted to select out of bounds index: {idx}, commits.len: {}",
                self.graph_data.commits.len()
            );
            return;
        }

        self.selected_entry_idx = Some(idx);
        if !self.multi_selection.contains(&idx) {
            self.multi_selection.clear();
            self.selection_anchor_idx = Some(idx);
        }
        self.selected_commit_diff = None;
        self.selected_commit_diff_stats = None;
        self.changed_files_expanded_dirs.clear();
        self.changed_files_scroll_handle
            .scroll_to_item(0, ScrollStrategy::Top);
        self.table_interaction_state.update(cx, |state, cx| {
            state.scroll_handle.scroll_to_item(idx, scroll_strategy);
            cx.notify();
        });

        let Some(commit) = self.graph_data.commits.get(idx) else {
            return;
        };

        let Some(repository) = self.get_repository(cx) else {
            return;
        };

        let commit_message_handle = commit.data.sha;
        let diff_handle = commit.data.sha.to_string();

        self.load_selected_commit_message(cx, &commit_message_handle, &repository);

        let diff_task =
            repository.update(cx, |repo, cx| repo.load_commit_diff(diff_handle, false, cx));

        self._commit_diff_task = Some(cx.spawn(async move |this, cx| {
            if let Ok(diff) = diff_task.await {
                this.update(cx, |this, cx| {
                    let stats = compute_diff_stats(&diff);
                    this.selected_commit_diff = Some(diff);
                    this.selected_commit_diff_stats = Some(stats);
                    cx.notify();
                })
                .ok();
            }
        }));

        self.refresh_selected_commit_tag_distance(cx);

        cx.emit(ItemEvent::Edit);
        cx.notify();
    }

    fn load_selected_commit_message(
        &mut self,
        cx: &mut Context<'_, Self>,
        sha: &Oid,
        repository: &Entity<Repository>,
    ) {
        if self
            .selected_commit_message
            .as_ref()
            .is_some_and(|old| old.sha == *sha)
        {
            return;
        }

        self._selected_commit_message_task = None;
        match repository.update(cx, |repo, cx| {
            repo.fetch_commit_data(*sha, true, cx).clone()
        }) {
            CommitDataState::Loaded(commit_data) => {
                self.set_selected_commit_message(cx, commit_data.sha, commit_data.message.clone());
            }
            CommitDataState::Loading(Some(receiver)) => {
                self._selected_commit_message_task = Some(cx.spawn(async move |this, cx| {
                    if let Ok(commit_data) = receiver.await {
                        this.update(cx, |this, cx| {
                            this.set_selected_commit_message(
                                cx,
                                commit_data.sha,
                                commit_data.message.clone(),
                            );
                        })
                        .log_err();
                    }
                }))
            }
            _ => {
                debug_panic!(
                    "Fetched commit data asynchronously, but was not given a listener or cached commit data."
                );
            }
        };
    }

    fn set_selected_commit_message(
        &mut self,
        cx: &mut Context<'_, GitGraph>,
        sha: Oid,
        message: SharedString,
    ) {
        let languages = self
            .workspace
            .read_with(cx, |workspace, cx| {
                workspace.project().read(cx).languages().clone()
            })
            .log_err();
        self.selected_commit_message = Some(DetailPanelCommitMessage {
            sha,
            message: cx.new(|cx| Markdown::new(message, languages, None, cx)),
            scroll_handle: ScrollHandle::new(),
        });
        self._selected_commit_message_task = None;
        cx.notify();
    }

    fn select_previous_match(&mut self, cx: &mut Context<Self>) {
        if self.search_state.matches.is_empty() {
            return;
        }

        let mut prev_selection = self.search_state.selected_index.unwrap_or_default();

        if prev_selection == 0 {
            prev_selection = self.search_state.matches.len() - 1;
        } else {
            prev_selection -= 1;
        }

        let Some(&oid) = self.search_state.matches.get_index(prev_selection) else {
            return;
        };

        self.search_state.selected_index = Some(prev_selection);
        self.select_commit_by_sha(oid, cx);
    }

    fn select_next_match(&mut self, cx: &mut Context<Self>) {
        if self.search_state.matches.is_empty() {
            return;
        }

        let mut next_selection = self
            .search_state
            .selected_index
            .map(|index| index + 1)
            .unwrap_or_default();

        if next_selection >= self.search_state.matches.len() {
            next_selection = 0;
        }

        let Some(&oid) = self.search_state.matches.get_index(next_selection) else {
            return;
        };

        self.search_state.selected_index = Some(next_selection);
        self.select_commit_by_sha(oid, cx);
    }

    pub fn set_repo_id(&mut self, repo_id: RepositoryId, cx: &mut Context<Self>) {
        if repo_id != self.repo_id
            && self
                .git_store
                .read(cx)
                .repositories()
                .contains_key(&repo_id)
        {
            self.repo_id = repo_id;
            // Unlike a HEAD change, a different repository has tags of its own.
            self.tags = None;
            self._tags_task = None;
            self.invalidate_state(cx);
        }
    }

    pub fn select_commit_by_sha(&mut self, sha: impl TryInto<Oid>, cx: &mut Context<Self>) {
        fn inner(this: &mut GitGraph, oid: Oid, cx: &mut Context<GitGraph>) {
            let Some(selected_repository) = this.get_repository(cx) else {
                return;
            };

            let Some(index) = selected_repository
                .read(cx)
                .get_graph_data(this.log_source.clone(), this.log_order)
                .and_then(|data| data.commit_oid_to_index.get(&oid))
                .copied()
            else {
                this.pending_select_sha = Some(oid);
                return;
            };

            this.pending_select_sha = None;
            this.select_entry(index, ScrollStrategy::Center, cx);
        }

        if let Ok(oid) = sha.try_into() {
            inner(self, oid, cx);
        }
    }

    /// The commit of the selected row, when one row is selected.
    pub(crate) fn selected_commit_sha(&self) -> Option<Oid> {
        let index = self.selected_entry_idx?;
        Some(self.graph_data.commits.get(index)?.data.sha)
    }

    /// [`Self::select_commit_by_sha`] for a commit the graph is asked to show wherever it is: a
    /// graph showing one branch only cannot hold a commit of another, so it switches to all
    /// branches rather than leaving the request pending forever.
    pub(crate) fn reveal_commit(&mut self, sha: Oid, cx: &mut Context<Self>) -> CommitReveal {
        let Some(repository) = self.get_repository(cx) else {
            return CommitReveal::NotInGraph;
        };
        let loaded = repository
            .read(cx)
            .get_graph_data(self.log_source.clone(), self.log_order)
            .is_some_and(|data| data.commit_oid_to_index.contains_key(&sha));
        if loaded {
            self.select_commit_by_sha(sha, cx);
            return CommitReveal::Selected;
        }
        match self.log_source {
            LogSource::All => {
                if matches!(
                    self.graph_data.max_commit_count,
                    AllCommitCount::FullyLoaded(_)
                ) {
                    CommitReveal::NotInGraph
                } else {
                    self.select_commit_by_sha(sha, cx);
                    CommitReveal::Pending
                }
            }
            LogSource::Branch(_) | LogSource::Sha(_) => {
                self.set_log_source(LogSource::All, cx);
                self.select_commit_by_sha(sha, cx);
                CommitReveal::ShowingAllBranches
            }
            LogSource::Path(_) => CommitReveal::NotInGraph,
        }
    }

    fn open_selected_commit_view(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(selected_entry_index) = self.selected_entry_idx else {
            return;
        };

        self.open_commit_view(selected_entry_index, window, cx);
    }

    fn open_commit_view(
        &mut self,
        entry_index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(commit_entry) = self.graph_data.commits.get(entry_index) else {
            return;
        };

        let Some(repository) = self.get_repository(cx) else {
            return;
        };

        CommitView::open(
            commit_entry.data.sha.to_string(),
            repository.downgrade(),
            self.workspace.clone(),
            None,
            None,
            window,
            cx,
        );
    }

    fn copy_commit_sha(&mut self, entry_index: usize, cx: &mut Context<Self>) {
        let Some(commit) = self.graph_data.commits.get(entry_index) else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(commit.data.sha.to_string()));
    }

    fn copy_selected_commit_sha(
        &mut self,
        _: &CopyCommitSha,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(selected_entry_index) = self.selected_entry_idx else {
            return;
        };
        self.copy_commit_sha(selected_entry_index, cx);
    }

    fn copy_commit_tag(&mut self, entry_index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(commit) = self.graph_data.commits.get(entry_index) else {
            return;
        };

        let tag_names = commit
            .data
            .tag_names()
            .into_iter()
            .map(|tag_name| SharedString::from(tag_name.to_string()))
            .collect::<Vec<_>>();

        match tag_names.as_slice() {
            [] => {}
            [tag_name] => cx.write_to_clipboard(ClipboardItem::new_string(tag_name.to_string())),
            _ => {
                self.workspace
                    .update(cx, |workspace, cx| {
                        workspace.toggle_modal(window, cx, |window, cx| {
                            CommitTagPicker::new(tag_names, window, cx)
                        });
                    })
                    .ok();
            }
        }
    }

    fn copy_selected_commit_tag(
        &mut self,
        _: &CopyCommitTag,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(selected_entry_index) = self.selected_entry_idx else {
            return;
        };
        self.copy_commit_tag(selected_entry_index, window, cx);
    }

    fn deploy_entry_context_menu(
        &mut self,
        position: Point<Pixels>,
        index: usize,
        ref_name: Option<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(commit) = self.graph_data.commits.get(index) else {
            return;
        };
        let merge_request_entries =
            self.merge_request_menu_entries(&[commit.data.sha], commit.data.parents.len() <= 1, cx);
        let repository = self
            .get_repository(cx)
            .map(|repository| repository.downgrade());
        let context_menu = commit_context_menu(
            CommitContextMenuData {
                sha: commit.data.sha,
                tag_names: commit
                    .data
                    .tag_names()
                    .into_iter()
                    .map(|tag_name| SharedString::from(tag_name.to_string()))
                    .collect(),
                is_merge: Some(commit.data.parents.len() > 1),
            },
            CommitContextMenuSource::GitGraph,
            ref_name,
            self.focus_handle.clone(),
            repository,
            self.workspace.clone(),
            merge_request_entries,
            window,
            cx,
        );
        self.set_context_menu(context_menu, position, Some(index), window, cx);
    }

    fn set_context_menu(
        &mut self,
        context_menu: Entity<ContextMenu>,
        position: Point<Pixels>,
        target_entry_index: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&context_menu.focus_handle(cx), cx);

        let subscription = cx.subscribe_in(
            &context_menu,
            window,
            |this, _, _: &DismissEvent, window, cx| {
                if this.context_menu.as_ref().is_some_and(|context_menu| {
                    context_menu
                        .menu
                        .focus_handle(cx)
                        .contains_focused(window, cx)
                }) {
                    cx.focus_self(window);
                }
                this.context_menu.take();
                cx.notify();
            },
        );
        self.context_menu = Some(GitGraphContextMenu {
            menu: context_menu,
            position,
            target_entry_index,
            _subscription: subscription,
        });
        cx.notify();
    }

    fn toggle_column_visibility(&mut self, col_idx: usize, cx: &mut Context<Self>) {
        if let Some(slot) = self.column_visibility.as_mut_slice().get_mut(col_idx) {
            *slot = !*slot;
            // Column visibility is persisted per item, so schedule a workspace serialization.
            cx.emit(ItemEvent::Edit);
        }
    }

    fn deploy_header_context_menu(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let is_path_history = matches!(self.log_source, LogSource::Path(_));
        let columns: &[&str] = if is_path_history {
            &[
                i18n::t!("Description"),
                i18n::t!("Date"),
                i18n::t!("Author"),
                i18n::t!("Commit"),
            ]
        } else {
            &[
                i18n::t!("Graph"),
                i18n::t!("Description"),
                i18n::t!("Date"),
                i18n::t!("Author"),
                i18n::t!("Commit"),
            ]
        };

        let filter = self.column_visibility.clone();
        let visible_count = filter
            .as_slice()
            .iter()
            .filter(|filtered| !**filtered)
            .count();

        let focus_handle = self.focus_handle.clone();
        let git_graph = cx.entity();
        let context_menu = ContextMenu::build(window, cx, |mut context_menu, _window, _cx| {
            context_menu = context_menu
                .context(focus_handle)
                .header(i18n::t!("Columns"));
            for (col_idx, label) in columns.iter().enumerate() {
                let is_visible = !filter.get(col_idx).copied().unwrap_or(false);
                // Disable hiding the last remaining visible column.
                let can_toggle = !is_visible || visible_count > 1;
                let git_graph = git_graph.clone();
                context_menu = context_menu.toggleable_entry_disabled_when(
                    label.to_string(),
                    is_visible,
                    !can_toggle,
                    IconPosition::End,
                    None,
                    move |_window, cx| {
                        git_graph.update(cx, |this, cx| {
                            this.toggle_column_visibility(col_idx, cx);
                            cx.notify();
                        });
                    },
                );
            }
            context_menu
        });

        self.set_context_menu(context_menu, position, None, window, cx);
    }

    fn render_search_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let color = cx.theme().colors();
        let query_focus_handle = self
            .search_state
            .editor
            .focus_handle(cx)
            .tab_index(1)
            .tab_stop(true);

        h_flex()
            .key_context("GitGraphSearchBar")
            .tab_index(1)
            .tab_group()
            .tab_stop(false)
            .w_full()
            .p_1p5()
            .gap_1p5()
            .border_b_1()
            .border_color(color.border_variant)
            .child(
                h_flex()
                    .h_8()
                    .flex_1()
                    .min_w_0()
                    .px_1p5()
                    .gap_1()
                    .track_focus(&query_focus_handle)
                    .border_1()
                    .border_color(color.border_variant)
                    .rounded_md()
                    .bg(color.toolbar_background)
                    .on_action(cx.listener(Self::confirm_search))
                    .child(self.search_state.editor.clone())
                    .child(self.render_search_tools(cx))
                    .child({
                        let focus_handle = query_focus_handle.clone();
                        IconButton::new("git-graph-search-case-sensitive", IconName::CaseSensitive)
                            .shape(ui::IconButtonShape::Square)
                            .toggle_state(self.search_state.case_sensitive)
                            .on_click({
                                let focus_handle = query_focus_handle.clone();
                                move |_, window, cx| {
                                    if !focus_handle.is_focused(window) {
                                        window.focus(&focus_handle, cx);
                                    }
                                    window.dispatch_action(ToggleCaseSensitive.boxed_clone(), cx);
                                }
                            })
                            .tooltip(move |_window, cx| {
                                Tooltip::for_action_in(
                                    i18n::t!("Match Case Sensitivity"),
                                    &ToggleCaseSensitive,
                                    &focus_handle,
                                    cx,
                                )
                            })
                    }),
            )
            .child(
                h_flex()
                    .min_w_64()
                    .gap_1()
                    .child({
                        let focus_handle = self.focus_handle.clone();
                        IconButton::new("git-graph-search-prev", IconName::ChevronLeft)
                            .shape(ui::IconButtonShape::Square)
                            .icon_size(IconSize::Small)
                            .tooltip(move |_, cx| {
                                Tooltip::for_action_in(
                                    i18n::t!("Select Previous Match"),
                                    &SelectPreviousMatch,
                                    &focus_handle,
                                    cx,
                                )
                            })
                            .map(|this| {
                                if self.search_state.matches.is_empty() {
                                    this.disabled(true)
                                } else {
                                    this.disabled(false).on_click(cx.listener(|this, _, _, cx| {
                                        this.select_previous_match(cx);
                                    }))
                                }
                            })
                    })
                    .child({
                        let focus_handle = self.focus_handle.clone();
                        IconButton::new("git-graph-search-next", IconName::ChevronRight)
                            .shape(ui::IconButtonShape::Square)
                            .icon_size(IconSize::Small)
                            .tooltip(move |_, cx| {
                                Tooltip::for_action_in(
                                    i18n::t!("Select Next Match"),
                                    &SelectNextMatch,
                                    &focus_handle,
                                    cx,
                                )
                            })
                            .map(|this| {
                                if self.search_state.matches.is_empty() {
                                    this.disabled(true)
                                } else {
                                    this.disabled(false).on_click(cx.listener(|this, _, _, cx| {
                                        this.select_next_match(cx);
                                    }))
                                }
                            })
                    })
                    .child(
                        h_flex()
                            .gap_1p5()
                            .child(
                                Label::new(format!(
                                    "{}/{}",
                                    self.search_state
                                        .selected_index
                                        .map(|index| index + 1)
                                        .unwrap_or(0),
                                    self.search_state.matches.len()
                                ))
                                .size(LabelSize::Small)
                                .when(self.search_state.matches.is_empty(), |this| {
                                    this.color(Color::Disabled)
                                }),
                            )
                            .when(
                                matches!(
                                    &self.search_state.state,
                                    QueryState::Confirmed((_, task)) if !task.is_ready()
                                ),
                                |this| {
                                    this.child(
                                        Icon::new(IconName::ArrowCircle)
                                            .color(Color::Accent)
                                            .size(IconSize::Small)
                                            .with_rotate_animation(2)
                                            .into_any_element(),
                                    )
                                },
                            ),
                    ),
            )
    }

    fn render_loading_spinner(&self, cx: &App) -> AnyElement {
        let rems = TextSize::Large.rems(cx);
        Icon::new(IconName::LoadCircle)
            .size(IconSize::Custom(rems))
            .color(Color::Accent)
            .with_rotate_animation(3)
            .into_any_element()
    }

    fn render_commit_detail_panel(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let Some(selected_idx) = self.selected_entry_idx else {
            return Empty.into_any_element();
        };

        let Some(commit_entry) = self.graph_data.commits.get(selected_idx) else {
            return Empty.into_any_element();
        };

        let Some(repository) = self.get_repository(cx) else {
            return Empty.into_any_element();
        };

        let data = repository.update(cx, |repository, cx| {
            repository
                .fetch_commit_data(commit_entry.data.sha, false, cx)
                .clone()
        });

        let full_sha: SharedString = commit_entry.data.sha.to_string().into();
        let short_sha: SharedString = full_sha.chars().take(7).collect::<String>().into();
        let ref_chips = self
            .graph_data
            .ref_chips
            .get(&commit_entry.data.sha)
            .cloned();

        let accent_colors = cx.theme().accents();
        let accent_color = accent_colors
            .0
            .get(commit_entry.color_idx)
            .copied()
            .unwrap_or_else(|| accent_colors.0.first().copied().unwrap_or_default());

        let (author_name, author_email, commit_timestamp) = match &data {
            CommitDataState::Loaded(data) => (
                data.author_name.clone(),
                data.author_email.clone(),
                Some(data.commit_timestamp),
            ),
            CommitDataState::Loading(_) => (i18n::t!("Loading…").into(), "".into(), None),
        };

        let date_string = commit_timestamp.map(format_timestamp).unwrap_or_default();

        let remote = repository.update(cx, |repo, cx| {
            let remote_url = repo.default_remote_url()?;
            let provider_registry = GitHostingProviderRegistry::default_global(cx);
            let (provider, parsed) = parse_git_remote_url(provider_registry, &remote_url)?;
            Some(GitRemote {
                host: provider,
                owner: parsed.owner.into(),
                repo: parsed.repo.into(),
            })
        });

        let avatar = {
            let author_email_for_avatar = if author_email.is_empty() {
                None
            } else {
                Some(author_email.clone())
            };

            CommitAvatar::new(&full_sha, author_email_for_avatar, remote.as_ref())
                .size(px(32.))
                .render(window, cx)
        };

        let changed_files_count = self
            .selected_commit_diff
            .as_ref()
            .map(|diff| diff.files.len())
            .unwrap_or(0);

        let (total_lines_added, total_lines_removed) =
            self.selected_commit_diff_stats.unwrap_or((0, 0));

        let changed_file_entries: Vec<ChangedFileEntry> = self
            .selected_commit_diff
            .as_ref()
            .map(|diff| {
                let mut files = diff.files.iter().collect::<Vec<_>>();
                if !self.changed_files_view_mode.is_tree() {
                    files.sort_by_key(|file| file.status());
                }
                files
                    .into_iter()
                    .map(|file| ChangedFileEntry::from_commit_file(file, cx))
                    .collect()
            })
            .unwrap_or_default();
        let changed_file_entries = Rc::new(changed_file_entries);
        let tree_entries: Rc<Vec<ChangedFileTreeEntry>> = if self.changed_files_view_mode.is_tree()
        {
            Rc::new(build_changed_file_tree_entries(
                changed_file_entries.as_ref().clone(),
                &self.changed_files_expanded_dirs,
            ))
        } else {
            Rc::default()
        };

        let is_tree_view = self.changed_files_view_mode.is_tree();
        let view_toggle = IconButton::new("toggle-changed-files-view", IconName::ListTree)
            .icon_size(IconSize::Small)
            .toggle_state(self.changed_files_view_mode.is_tree())
            .tooltip({
                let tooltip = if is_tree_view {
                    i18n::t!("Show Flat View")
                } else {
                    i18n::t!("Show Tree View")
                };
                move |_, cx| Tooltip::for_action(tooltip, &ToggleChangedFilesView, cx)
            })
            .on_click(cx.listener(|this, _, _window, cx| {
                this.changed_files_view_mode = this.changed_files_view_mode.toggled();
                this.changed_files_scroll_handle
                    .scroll_to_item(0, ScrollStrategy::Top);
                cx.notify();
            }));

        v_flex()
            .min_w(px(300.))
            .h_full()
            .bg(cx.theme().colors().editor_background)
            .flex_basis(DefiniteLength::Fraction(
                self.commit_details_split_state.read(cx).right_ratio(),
            ))
            .child(
                v_flex()
                    .relative()
                    .w_full()
                    .p_2()
                    .gap_2()
                    .child(
                        div().absolute().top_2().right_2().child(
                            IconButton::new("close-detail", IconName::Close)
                                .icon_size(IconSize::Small)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.selected_entry_idx = None;
                                    this.selected_commit_diff = None;
                                    this.selected_commit_diff_stats = None;
                                    this.selected_commit_message = None;
                                    this._selected_commit_message_task = None;
                                    this.changed_files_expanded_dirs.clear();
                                    this._commit_diff_task = None;
                                    cx.notify();
                                })),
                        ),
                    )
                    .child(
                        div().absolute().top_2().left_2().child(
                            Disclosure::new(
                                "toggle-commit-metadata",
                                !self.commit_details_metadata_collapsed,
                            )
                            .tooltip({
                                let label = if self.commit_details_metadata_collapsed {
                                    i18n::t!("Show Commit Details")
                                } else {
                                    i18n::t!("Hide Commit Details")
                                };
                                move |_window, cx: &mut App| Tooltip::simple(label, cx)
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.commit_details_metadata_collapsed =
                                    !this.commit_details_metadata_collapsed;
                                cx.notify();
                            })),
                        ),
                    )
                    .map(|this| {
                        if self.commit_details_metadata_collapsed {
                            return this.child(
                                h_flex()
                                    .w_full()
                                    .px_6()
                                    .gap_1()
                                    .justify_center()
                                    .overflow_hidden()
                                    .child(
                                        Label::new(author_name).size(LabelSize::Small).truncate(),
                                    )
                                    .child(
                                        Label::new(short_sha)
                                            .size(LabelSize::Small)
                                            .color(Color::Muted),
                                    ),
                            );
                        }
                        this.child(
                            v_flex()
                                .py_1()
                                .w_full()
                                .items_center()
                                .child(avatar)
                                .child(Label::new(author_name).mt_1p5())
                                .child(
                                    Label::new(date_string)
                                        .color(Color::Muted)
                                        .size(LabelSize::Small),
                                ),
                        )
                        .children(ref_chips.map(|chips| {
                            self.render_detail_ref_chips(
                                commit_entry.data.sha,
                                &chips,
                                accent_color,
                                selected_idx,
                                cx,
                            )
                        }))
                        .child(self.render_latest_tag_row(commit_entry.data.sha))
                        .child(
                            v_flex()
                                .ml_neg_1()
                                .gap_1p5()
                                .when(!author_email.is_empty(), |this| {
                                    let copied_state: Entity<CopiedState> = window.use_keyed_state(
                                        "author-email-copy",
                                        cx,
                                        CopiedState::new,
                                    );
                                    let is_copied = copied_state.read(cx).is_copied();

                                    let (icon, icon_color, tooltip_label) = if is_copied {
                                        (IconName::Check, Color::Success, i18n::t!("Email Copied!"))
                                    } else {
                                        (IconName::Envelope, Color::Muted, i18n::t!("Copy Email"))
                                    };

                                    let copy_email = author_email.clone();
                                    let author_email_for_tooltip = author_email.clone();

                                    this.child(
                                        Button::new("author-email-copy", author_email.clone())
                                            .start_icon(
                                                Icon::new(icon)
                                                    .size(IconSize::Small)
                                                    .color(icon_color),
                                            )
                                            .label_size(LabelSize::Small)
                                            .truncate(true)
                                            .color(Color::Muted)
                                            .tooltip(move |_, cx| {
                                                Tooltip::with_meta(
                                                    tooltip_label,
                                                    None,
                                                    author_email_for_tooltip.clone(),
                                                    cx,
                                                )
                                            })
                                            .on_click(move |_, _, cx| {
                                                copied_state.update(cx, |state, _cx| {
                                                    state.mark_copied();
                                                });
                                                cx.write_to_clipboard(ClipboardItem::new_string(
                                                    copy_email.to_string(),
                                                ));
                                                let state_id = copied_state.entity_id();
                                                cx.spawn(async move |cx| {
                                                    cx.background_executor()
                                                        .timer(COPIED_STATE_DURATION)
                                                        .await;
                                                    cx.update(|cx| {
                                                        cx.notify(state_id);
                                                    })
                                                })
                                                .detach();
                                            }),
                                    )
                                })
                                .child({
                                    let copy_sha = full_sha.clone();
                                    let copied_state: Entity<CopiedState> =
                                        window.use_keyed_state("sha-copy", cx, CopiedState::new);
                                    let is_copied = copied_state.read(cx).is_copied();

                                    let (icon, icon_color, tooltip_label) = if is_copied {
                                        (
                                            IconName::Check,
                                            Color::Success,
                                            i18n::t!("Commit SHA Copied!"),
                                        )
                                    } else {
                                        (IconName::Hash, Color::Muted, i18n::t!("Copy Commit SHA"))
                                    };

                                    Button::new("sha-button", &full_sha)
                                        .start_icon(
                                            Icon::new(icon).size(IconSize::Small).color(icon_color),
                                        )
                                        .label_size(LabelSize::Small)
                                        .truncate(true)
                                        .color(Color::Muted)
                                        .tooltip({
                                            let full_sha = full_sha.clone();
                                            move |_, cx| {
                                                Tooltip::with_meta(
                                                    tooltip_label,
                                                    None,
                                                    full_sha.clone(),
                                                    cx,
                                                )
                                            }
                                        })
                                        .on_click(move |_, _, cx| {
                                            copied_state.update(cx, |state, _cx| {
                                                state.mark_copied();
                                            });
                                            cx.write_to_clipboard(ClipboardItem::new_string(
                                                copy_sha.to_string(),
                                            ));
                                            let state_id = copied_state.entity_id();
                                            cx.spawn(async move |cx| {
                                                cx.background_executor()
                                                    .timer(COPIED_STATE_DURATION)
                                                    .await;
                                                cx.update(|cx| {
                                                    cx.notify(state_id);
                                                })
                                            })
                                            .detach();
                                        })
                                })
                                .when_some(remote.clone(), |this, remote| {
                                    let provider_name = remote.host.name();
                                    let icon =
                                        ui::git_hosting_provider_icon(provider_name.as_str());
                                    let parsed_remote = ParsedGitRemote {
                                        owner: remote.owner.as_ref().into(),
                                        repo: remote.repo.as_ref().into(),
                                    };
                                    let params = BuildCommitPermalinkParams {
                                        sha: full_sha.as_ref(),
                                    };
                                    let url = remote
                                        .host
                                        .build_commit_permalink(&parsed_remote, params)
                                        .to_string();

                                    this.child(
                                        Button::new(
                                            "view-on-provider",
                                            i18n::t!("View on {}", provider_name),
                                        )
                                        .start_icon(
                                            Icon::new(icon)
                                                .size(IconSize::Small)
                                                .color(Color::Muted),
                                        )
                                        .label_size(LabelSize::Small)
                                        .truncate(true)
                                        .color(Color::Muted)
                                        .on_click(
                                            move |_, _, cx| {
                                                cx.open_url(&url);
                                            },
                                        ),
                                    )
                                }),
                        )
                    }),
            )
            .child(Divider::horizontal())
            .child(self.render_commit_message(window, cx))
            .child(Divider::horizontal())
            .child(
                v_flex()
                    .min_w_0()
                    .flex_1()
                    .overflow_hidden()
                    .child(
                        h_flex()
                            .p_2()
                            .pr_3()
                            .pb_1()
                            .gap_1()
                            .w_full()
                            .justify_between()
                            .child(
                                h_flex()
                                    .gap_1()
                                    .child(
                                        Label::new(i18n::tn!(
                                            changed_files_count,
                                            "{count} Changed File",
                                            "{count} Changed Files"
                                        ))
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                    )
                                    .child(Divider::vertical())
                                    .child(view_toggle),
                            )
                            .child(DiffStat::new(
                                "commit-diff-stat",
                                total_lines_added,
                                total_lines_removed,
                            )),
                    )
                    .child(
                        div()
                            .id("changed-files-container")
                            .flex_1()
                            .min_h_0()
                            .child({
                                let flat_entries = changed_file_entries;

                                let entry_count = if is_tree_view {
                                    tree_entries.len()
                                } else {
                                    flat_entries.len()
                                };
                                let commit_sha = full_sha.clone();
                                let repository = repository.downgrade();
                                let workspace = self.workspace.clone();
                                let git_graph = cx.weak_entity();
                                let indent_tree_entries = tree_entries.clone();

                                uniform_list(
                                    "changed-files-list",
                                    entry_count,
                                    move |range, _window, cx| {
                                        range
                                            .map(|ix| {
                                                if is_tree_view {
                                                    match &tree_entries[ix] {
                                                        ChangedFileTreeEntry::Directory(entry) => {
                                                            entry.render(ix, git_graph.clone(), cx)
                                                        }
                                                        ChangedFileTreeEntry::File(entry) => {
                                                            entry.entry.render(
                                                                ix,
                                                                entry.depth,
                                                                None,
                                                                commit_sha.clone(),
                                                                repository.clone(),
                                                                workspace.clone(),
                                                                cx,
                                                            )
                                                        }
                                                    }
                                                } else {
                                                    let directory_label = (!flat_entries[ix]
                                                        .dir_path
                                                        .is_empty())
                                                    .then(|| flat_entries[ix].dir_path.clone());
                                                    flat_entries[ix].render(
                                                        ix,
                                                        0,
                                                        directory_label,
                                                        commit_sha.clone(),
                                                        repository.clone(),
                                                        workspace.clone(),
                                                        cx,
                                                    )
                                                }
                                            })
                                            .collect()
                                    },
                                )
                                .when(is_tree_view, |list| {
                                    list.with_decoration(
                                        ui::indent_guides(
                                            px(TREE_INDENT),
                                            IndentGuideColors::panel(cx),
                                        )
                                        .with_left_offset(
                                            ui::LIST_ITEM_INDENT_GUIDE_LEFT_OFFSET - px(2.),
                                        )
                                        .with_compute_indents_fn(
                                            cx.entity(),
                                            move |_, range, _window, _cx| {
                                                range
                                                    .map(|ix| match indent_tree_entries.get(ix) {
                                                        Some(ChangedFileTreeEntry::Directory(
                                                            entry,
                                                        )) => entry.depth,
                                                        Some(ChangedFileTreeEntry::File(entry)) => {
                                                            entry.depth
                                                        }
                                                        None => 0,
                                                    })
                                                    .collect()
                                            },
                                        ),
                                    )
                                })
                                .size_full()
                                .track_scroll(&self.changed_files_scroll_handle)
                            })
                            .vertical_scrollbar_for(&self.changed_files_scroll_handle, window, cx),
                    ),
            )
            .child(Divider::horizontal())
            .child(
                h_flex().p_1p5().w_full().child(
                    Button::new("view-commit", i18n::t!("View Commit"))
                        .full_width()
                        .start_icon(
                            Icon::new(IconName::GitCommit)
                                .size(IconSize::Small)
                                .color(Color::Muted),
                        )
                        .style(ButtonStyle::OutlinedGhost)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.open_selected_commit_view(window, cx);
                        })),
                ),
            )
            .into_any_element()
    }

    fn render_graph_canvas(&self, window: &Window, cx: &mut Context<GitGraph>) -> impl IntoElement {
        let row_height = Self::row_height(window, cx);
        let visible_row_count = self.visible_row_count(window, cx);
        let table_state = self.table_interaction_state.read(cx);
        let viewport_height = table_state
            .scroll_handle
            .0
            .borrow()
            .last_item_size
            .map(|size| size.item.height)
            .unwrap_or(window.viewport_size().height);
        let loaded_commit_count = self.graph_data.commits.len();

        let content_height = row_height * loaded_commit_count;
        let max_scroll = (content_height - viewport_height).max(px(0.));
        let scroll_offset_y = (-table_state.scroll_offset().y).clamp(px(0.), max_scroll);

        let first_visible_row = (scroll_offset_y / row_height).floor() as usize;
        let vertical_scroll_offset = scroll_offset_y - (first_visible_row as f32 * row_height);

        let graph_width = self
            .graph_canvas_content_width()
            .max(self.graph_viewport_width(window, cx));
        let last_visible_row = first_visible_row + visible_row_count + 1;

        let viewport_range = first_visible_row.min(loaded_commit_count.saturating_sub(1))
            ..(last_visible_row).min(loaded_commit_count);
        let rows = self.graph_data.commits[viewport_range.clone()].to_vec();
        let commit_lines: Vec<_> = self
            .graph_data
            .lines
            .iter()
            .filter(|line| {
                line.full_interval.start <= viewport_range.end
                    && line.full_interval.end >= viewport_range.start
            })
            .cloned()
            .collect();

        let mut lines: BTreeMap<usize, Vec<_>> = BTreeMap::new();

        let hovered_entry_idx = self.hovered_entry_idx;
        let selected_entry_idx = self.selected_entry_idx;
        let context_menu_target_index = self
            .context_menu
            .as_ref()
            .and_then(|menu| menu.target_entry_index);
        let is_focused = self.focus_handle.is_focused(window);
        let graph_canvas_bounds = self.graph_canvas_bounds.clone();

        gpui::canvas(
            move |_bounds, _window, _cx| {},
            move |bounds: Bounds<Pixels>, _: (), window: &mut Window, cx: &mut App| {
                graph_canvas_bounds.set(Some(bounds));

                window.paint_layer(bounds, |window| {
                    let theme = cx.theme();
                    let accent_colors = theme.accents();
                    let background = theme.colors().editor_background;

                    let hover_bg = theme.colors().element_hover.opacity(0.6);
                    let selected_bg = if is_focused {
                        theme.colors().element_selected
                    } else {
                        theme.colors().element_hover
                    };

                    for visible_row_idx in 0..rows.len() {
                        let absolute_row_idx = first_visible_row + visible_row_idx;
                        let is_hovered = hovered_entry_idx == Some(absolute_row_idx);
                        let is_selected = selected_entry_idx == Some(absolute_row_idx);
                        let is_context_menu_target =
                            context_menu_target_index == Some(absolute_row_idx);

                        if is_hovered || is_selected || is_context_menu_target {
                            let row_y = bounds.origin.y + visible_row_idx as f32 * row_height
                                - vertical_scroll_offset;
                            let row_bounds = Bounds::new(
                                point(bounds.origin.x, row_y),
                                gpui::Size {
                                    width: bounds.size.width,
                                    height: row_height,
                                },
                            );

                            let bg_color = if is_selected || is_context_menu_target {
                                selected_bg
                            } else {
                                hover_bg
                            };
                            window.paint_quad(gpui::fill(row_bounds, bg_color));
                        }
                    }

                    for line in commit_lines {
                        let Some((start_segment_idx, start_column)) =
                            line.get_first_visible_segment_idx(first_visible_row)
                        else {
                            continue;
                        };

                        let line_x = lane_center_x(bounds, start_column as f32);

                        let start_row = line.full_interval.start as i32 - first_visible_row as i32;

                        let from_y =
                            bounds.origin.y + start_row as f32 * row_height + row_height / 2.0
                                - vertical_scroll_offset
                                + COMMIT_CIRCLE_RADIUS;

                        let mut current_row = from_y;
                        let mut current_column = line_x;

                        let mut builder = PathBuilder::stroke(LINE_WIDTH);
                        builder.move_to(point(line_x, from_y));

                        let segments = &line.segments[start_segment_idx..];
                        let desired_curve_height = row_height / 3.0;
                        let desired_curve_width = LANE_WIDTH / 3.0;

                        for (segment_idx, segment) in segments.iter().enumerate() {
                            let is_last = segment_idx + 1 == segments.len();

                            match segment {
                                CommitLineSegment::Straight { to_row } => {
                                    let mut dest_row = to_row_center(
                                        to_row - first_visible_row,
                                        row_height,
                                        vertical_scroll_offset,
                                        bounds,
                                    );
                                    if is_last {
                                        dest_row -= COMMIT_CIRCLE_RADIUS;
                                    }

                                    let dest_point = point(current_column, dest_row);

                                    current_row = dest_point.y;
                                    builder.line_to(dest_point);
                                    builder.move_to(dest_point);
                                }
                                CommitLineSegment::Curve {
                                    to_column,
                                    on_row,
                                    curve_kind,
                                } => {
                                    let mut to_column = lane_center_x(bounds, *to_column as f32);

                                    let mut to_row = to_row_center(
                                        *on_row - first_visible_row,
                                        row_height,
                                        vertical_scroll_offset,
                                        bounds,
                                    );

                                    // This means that this branch was a checkout
                                    let going_right = to_column > current_column;
                                    let column_shift = if going_right {
                                        COMMIT_CIRCLE_RADIUS + COMMIT_CIRCLE_STROKE_WIDTH
                                    } else {
                                        -COMMIT_CIRCLE_RADIUS - COMMIT_CIRCLE_STROKE_WIDTH
                                    };

                                    match curve_kind {
                                        CurveKind::Checkout => {
                                            if is_last {
                                                to_column -= column_shift;
                                            }

                                            let available_curve_width =
                                                (to_column - current_column).abs();
                                            let available_curve_height =
                                                (to_row - current_row).abs();
                                            let curve_width =
                                                desired_curve_width.min(available_curve_width);
                                            let curve_height =
                                                desired_curve_height.min(available_curve_height);
                                            let signed_curve_width = if going_right {
                                                curve_width
                                            } else {
                                                -curve_width
                                            };
                                            let curve_start =
                                                point(current_column, to_row - curve_height);
                                            let curve_end =
                                                point(current_column + signed_curve_width, to_row);
                                            let curve_control = point(current_column, to_row);

                                            builder.move_to(point(current_column, current_row));
                                            builder.line_to(curve_start);
                                            builder.move_to(curve_start);
                                            builder.curve_to(curve_end, curve_control);
                                            builder.move_to(curve_end);
                                            builder.line_to(point(to_column, to_row));
                                        }
                                        CurveKind::Merge => {
                                            if is_last {
                                                to_row -= COMMIT_CIRCLE_RADIUS;
                                            }

                                            let merge_start = point(
                                                current_column + column_shift,
                                                current_row - COMMIT_CIRCLE_RADIUS,
                                            );
                                            let available_curve_width =
                                                (to_column - merge_start.x).abs();
                                            let available_curve_height =
                                                (to_row - merge_start.y).abs();
                                            let curve_width =
                                                desired_curve_width.min(available_curve_width);
                                            let curve_height =
                                                desired_curve_height.min(available_curve_height);
                                            let signed_curve_width = if going_right {
                                                curve_width
                                            } else {
                                                -curve_width
                                            };
                                            let curve_start = point(
                                                to_column - signed_curve_width,
                                                merge_start.y,
                                            );
                                            let curve_end =
                                                point(to_column, merge_start.y + curve_height);
                                            let curve_control = point(to_column, merge_start.y);

                                            builder.move_to(merge_start);
                                            builder.line_to(curve_start);
                                            builder.move_to(curve_start);
                                            builder.curve_to(curve_end, curve_control);
                                            builder.move_to(curve_end);
                                            builder.line_to(point(to_column, to_row));
                                        }
                                    }
                                    current_row = to_row;
                                    current_column = to_column;
                                    builder.move_to(point(current_column, current_row));
                                }
                            }
                        }

                        builder.close();
                        lines.entry(line.color_idx).or_default().push(builder);
                    }

                    for (color_idx, builders) in lines {
                        let line_color = accent_colors.color_for_index(color_idx as u32);

                        for builder in builders {
                            if let Ok(path) = builder.build() {
                                // we paint each color on it's own layer to stop overlapping lines
                                // of different colors changing the color of a line
                                window.paint_layer(bounds, |window| {
                                    window.paint_path(path, line_color);
                                });
                            }
                        }
                    }

                    // Commit nodes are painted last so they sit on top of the
                    // connector lines, keeping each commit a clear, distinct point.
                    for (row_idx, row) in rows.into_iter().enumerate() {
                        let absolute_row_idx = first_visible_row + row_idx;
                        let row_color = accent_colors.color_for_index(row.color_idx as u32);
                        let row_y_center =
                            bounds.origin.y + row_idx as f32 * row_height + row_height / 2.0
                                - vertical_scroll_offset;

                        let commit_x = lane_center_x(bounds, row.lane as f32);

                        // Match the node halo to the row's effective background so
                        // the ring blends seamlessly on hovered/selected rows.
                        let node_background = if selected_entry_idx == Some(absolute_row_idx)
                            || context_menu_target_index == Some(absolute_row_idx)
                        {
                            background.blend(selected_bg)
                        } else if hovered_entry_idx == Some(absolute_row_idx) {
                            background.blend(hover_bg)
                        } else {
                            background
                        };

                        draw_commit_circle(
                            commit_x,
                            row_y_center,
                            row_color,
                            node_background,
                            window,
                        );
                    }
                })
            },
        )
        .w(graph_width)
        .h_full()
    }

    fn row_at_position(
        &self,
        position_y: Pixels,
        window: &Window,
        cx: &Context<Self>,
    ) -> Option<usize> {
        let canvas_bounds = self.graph_canvas_bounds.get()?;
        let table_state = self.table_interaction_state.read(cx);
        let scroll_offset_y = -table_state.scroll_offset().y;

        let local_y = position_y - canvas_bounds.origin.y;

        if local_y >= px(0.) && local_y < canvas_bounds.size.height {
            let absolute_y = local_y + scroll_offset_y;
            let row_height = Self::row_height(window, cx);
            let absolute_row = (absolute_y / row_height).floor() as usize;

            if absolute_row < self.graph_data.commits.len() {
                return Some(absolute_row);
            }
        }

        None
    }

    fn handle_graph_mouse_move(
        &mut self,
        event: &gpui::MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(row) = self.row_at_position(event.position.y, window, cx) {
            if self.hovered_entry_idx != Some(row) {
                self.hovered_entry_idx = Some(row);
                cx.notify();
            }
        } else if self.hovered_entry_idx.is_some() {
            self.hovered_entry_idx = None;
            cx.notify();
        }
    }

    fn handle_entry_click(
        &mut self,
        entry_idx: usize,
        event: &ClickEvent,
        scroll_strategy: ScrollStrategy,
        focus_handle: Option<&FocusHandle>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Right-clicks open the context menu, not the details panel.
        if event.is_right_click() {
            return;
        }

        if let Some(focus_handle) = focus_handle {
            focus_handle.focus(window, cx);
        }

        if entry_idx >= self.graph_data.commits.len() {
            return;
        }

        let gesture = SelectionGesture::from_modifiers(&event.modifiers());
        let (selection, anchor) = apply_selection_gesture(
            &self.multi_selection,
            self.selection_anchor_idx,
            entry_idx,
            gesture,
        );
        // Toggling a row off leaves the clicked row unpicked, so the details
        // panel follows whichever picked row is newest instead.
        let current = if selection.contains(&entry_idx) {
            entry_idx
        } else {
            selection.iter().next().copied().unwrap_or(entry_idx)
        };
        self.multi_selection = if selection.len() > 1 {
            selection
        } else {
            BTreeSet::default()
        };
        self.selection_anchor_idx = Some(anchor);
        self.select_entry(current, scroll_strategy, cx);
        cx.notify();

        if event.click_count() >= 2 && gesture == SelectionGesture::Replace {
            self.open_commit_view(entry_idx, window, cx);
        }
    }

    fn handle_graph_click(
        &mut self,
        event: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(row) = self.row_at_position(event.position().y, window, cx) {
            self.handle_entry_click(row, event, ScrollStrategy::Nearest, None, window, cx);
        }
    }

    fn handle_entry_secondary_mouse_down(
        &mut self,
        entry_idx: usize,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.multi_selection.len() > 1 && self.multi_selection.contains(&entry_idx) {
            self.deploy_multi_selection_menu(event.position, window, cx);
        } else {
            self.deploy_entry_context_menu(event.position, entry_idx, None, window, cx);
        }
        cx.stop_propagation();
    }

    fn handle_graph_secondary_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(row) = self.row_at_position(event.position.y, window, cx) else {
            return;
        };

        self.handle_entry_secondary_mouse_down(row, event, window, cx);
    }

    fn handle_graph_scroll(
        &mut self,
        event: &ScrollWheelEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let line_height = window.line_height();
        let delta = event.delta.pixel_delta(line_height);

        let table_state = self.table_interaction_state.read(cx);
        let current_offset = table_state.scroll_offset();

        let viewport_height = table_state.scroll_handle.viewport().size.height;

        let commit_count = match self.graph_data.max_commit_count {
            AllCommitCount::Loading(count) => count,
            AllCommitCount::FullyLoaded(count) => count,
            AllCommitCount::NotLoaded => self.graph_data.commits.len(),
        };
        let content_height = Self::row_height(window, cx) * commit_count;
        let max_vertical_scroll = (viewport_height - content_height).min(px(0.));

        let new_y = (current_offset.y + delta.y).clamp(max_vertical_scroll, px(0.));
        let new_offset = Point::new(current_offset.x, new_y);

        if new_offset != current_offset {
            table_state.set_scroll_offset(new_offset);
            cx.notify();
        }
    }

    fn commit_count_and_loading_state(&mut self, cx: &mut Context<Self>) -> (usize, bool) {
        match self.graph_data.max_commit_count {
            AllCommitCount::FullyLoaded(count) => (count, false),
            AllCommitCount::Loading(count) => {
                let is_loading = self
                    .get_repository(cx)
                    .map(|repository| {
                        repository.update(cx, |repository, cx| {
                            repository
                                .graph_data(self.log_source.clone(), self.log_order, 0..0, cx)
                                .is_loading
                        })
                    })
                    .unwrap_or(false);

                (count, is_loading)
            }
            AllCommitCount::NotLoaded => {
                let (commit_count, is_loading) = if let Some(repository) = self.get_repository(cx) {
                    repository.update(cx, |repository, cx| {
                        // Start loading the graph data if we haven't started already
                        let GraphDataResponse {
                            commits,
                            is_loading,
                            error: _,
                        } = repository.graph_data(
                            self.log_source.clone(),
                            self.log_order,
                            0..usize::MAX,
                            cx,
                        );
                        self.graph_data.add_commits(commits);
                        (commits.len(), is_loading)
                    })
                } else {
                    (0, false)
                };

                (commit_count, is_loading)
            }
        }
    }

    fn render_commit_view_resize_handle(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .id("commit-view-split-resize-container")
            .relative()
            .h_full()
            .flex_shrink_0()
            .w(px(1.))
            .bg(cx.theme().colors().border_variant)
            .child(
                div()
                    .id("commit-view-split-resize-handle")
                    .absolute()
                    .left(px(-RESIZE_HANDLE_WIDTH / 2.0))
                    .w(px(RESIZE_HANDLE_WIDTH))
                    .h_full()
                    .cursor_col_resize()
                    .block_mouse_except_scroll()
                    .on_click(cx.listener(|this, event: &ClickEvent, _window, cx| {
                        if event.click_count() >= 2 {
                            this.commit_details_split_state.update(cx, |state, _| {
                                state.on_double_click();
                            });
                        }
                        cx.stop_propagation();
                    }))
                    .on_drag(DraggedSplitHandle, |_, _, _, cx| cx.new(|_| gpui::Empty)),
            )
            .into_any_element()
    }

    fn render_commit_message(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let Some(DetailPanelCommitMessage {
            message,
            scroll_handle,
            ..
        }) = self.selected_commit_message.as_ref()
        else {
            return Empty.into_any_element();
        };

        let message_style = editor::hover_markdown_style(window, cx);
        let rem_size = window.rem_size();
        let line_height = message_style
            .base_text_style
            .line_height_in_pixels(rem_size);

        let collapsed = self.commit_details_message_collapsed;

        div()
            // Using grid over flexbox because the structure of this side
            // panel prvents taffy from calculating a concrete width correctly,
            // which causes problems with text reflow when using flexbox.
            // grid, on the other hand, doesn't appear to give taffy the same
            // problems.
            .w_full()
            .py_2()
            .pl_2()
            .grid()
            .grid_cols(1)
            .gap_1()
            .child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .child(
                        Disclosure::new("toggle-commit-message", !collapsed).on_click(cx.listener(
                            |this, _, _, cx| {
                                this.commit_details_message_collapsed =
                                    !this.commit_details_message_collapsed;
                                cx.notify();
                            },
                        )),
                    )
                    .child(
                        Label::new(i18n::tc!("commit details", "Message"))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .when(!collapsed, |this| {
                this.child(
                    div()
                        .relative()
                        .w_full()
                        .child(
                            div()
                                .id("commit-message")
                                .text_sm()
                                .w_full()
                                .max_h(line_height * 12.)
                                .overflow_y_scroll()
                                .track_scroll(scroll_handle)
                                .child(MarkdownElement::new(message.clone(), message_style)),
                        )
                        .vertical_scrollbar_for(scroll_handle, window, cx),
                )
            })
            .into_any_element()
    }

    /// The detail panel's 「直近のタグ」 line: which tag this commit came after
    /// and how many commits have landed since, which is the usual way to find
    /// the previous release from a commit.
    fn render_latest_tag_row(&self, sha: Oid) -> impl IntoElement {
        let text: SharedString = match &self.selected_commit_tag_distance {
            Some((loaded_sha, described)) if *loaded_sha == sha => match described {
                Some(distance) if distance.commits_since == 0 => {
                    let tag = &distance.tag;
                    i18n::t!("Latest tag: {tag} (this commit itself)").into()
                }
                Some(distance) => {
                    let tag = &distance.tag;
                    i18n::tn!(
                        distance.commits_since,
                        "Latest tag: {tag} ({count} commit before this one)",
                        "Latest tag: {tag} ({count} commits before this one)"
                    )
                    .into()
                }
                None => i18n::t!("Latest tag: none").into(),
            },
            _ => i18n::t!("Latest tag: checking…").into(),
        };
        h_flex().w_full().justify_center().child(
            Label::new(text)
                .size(LabelSize::Small)
                .color(Color::Muted)
                .truncate(),
        )
    }

    fn head_branch_name(&self, cx: &App) -> Option<SharedString> {
        self.get_repository(cx).and_then(|repository| {
            repository
                .read(cx)
                .snapshot()
                .branch
                .as_ref()
                .map(|branch| SharedString::from(branch.name().to_string()))
        })
    }

    /// Every ref the comparison picker can offer: local branches first (the
    /// ones people compare most), then remote branches, then tags. Tags are
    /// offered as `refs/tags/<name>`, which is also how they reach git, so that
    /// a branch of the same name cannot stand in for one.
    fn comparable_refs(&self, cx: &App) -> Vec<SharedString> {
        let Some(repository) = self.get_repository(cx) else {
            return Vec::new();
        };
        let snapshot = repository.read(cx).snapshot();
        let mut local = Vec::new();
        let mut remote = Vec::new();
        for branch in snapshot.branch_list.iter() {
            let name = SharedString::from(branch.name().to_string());
            if branch.is_remote() {
                remote.push(name);
            } else {
                local.push(name);
            }
        }
        local.extend(remote);
        local.extend(
            self.tags
                .iter()
                .flat_map(|tags| tags.iter())
                .map(|tag| SharedString::from(tag.rev())),
        );
        local
    }

    /// The tag list the toolbar's tag picker shows, if it has been loaded.
    pub(crate) fn tags(&self) -> Option<Arc<[TagInfo]>> {
        self.tags.clone()
    }

    /// (Re)loads the tag list behind the toolbar's tag picker. Tags change
    /// rarely, so this runs when the toolbar first shows and when the
    /// repository says they changed; the old list stays up in the meantime.
    fn load_tags(&mut self, cx: &mut Context<Self>) {
        let Some(repository) = self.get_repository(cx) else {
            return;
        };
        let requester = cx.entity_id();
        let receiver = repository.update(cx, |repository, _cx| repository.latest_tags(requester));
        self._tags_task = Some(cx.spawn(async move |this, cx| {
            let tags = match receiver.await {
                Ok(Ok(tags)) => Some(Arc::from(tags)),
                Ok(Err(error)) => {
                    log::warn!("failed to list git tags: {error:#}");
                    None
                }
                // A newer request of this graph took its place in the queue.
                Err(_) => return,
            };
            this.update(cx, |this, cx| {
                this._tags_task = None;
                match tags {
                    Some(tags) => this.tags = Some(tags),
                    // An empty list rather than none, or the toolbar would ask again on every
                    // frame; a list already shown stays, as the better guess.
                    None if this.tags.is_none() => this.tags = Some(Arc::from([])),
                    None => {}
                }
                this.search_again_if_waiting_for_tags(cx);
                cx.notify();
            })
            .ok();
        }));
    }

    fn open_tag_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.tags.is_none() && self._tags_task.is_none() {
            self.load_tags(cx);
        }
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let graph = cx.entity();
        let tags = self.tags.clone();
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, |window, cx| {
                crate::many_refs::GraphTagPicker::new(graph, tags, window, cx)
            });
        });
    }

    /// The suggestion to pack the loose refs, shown under the toolbar while
    /// there are enough of them to slow git down.
    fn render_loose_refs_banner(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if matches!(self.log_source, LogSource::Path(_)) {
            return None;
        }
        let repository = self.get_repository(cx)?;
        match crate::many_refs::awaiting_recount(&repository, cx) {
            Some(true) => return None,
            Some(false) => {
                let repository = repository.clone();
                cx.defer(move |cx| crate::many_refs::forget_count_before_packing(&repository, cx));
            }
            None => {}
        }
        let text = crate::many_refs::loose_refs_banner_text(repository.read(cx).loose_refs)?;
        let stage = crate::many_refs::pack_refs_stage(&repository, cx);
        Some(
            h_flex()
                .debug_selector(|| "git-graph-loose-refs-banner".into())
                .w_full()
                .px_2()
                .py_1()
                .gap_2()
                .border_b_1()
                .border_color(cx.theme().colors().border_variant)
                .bg(cx.theme().status().warning_background)
                .child(
                    Icon::new(IconName::Warning)
                        .size(IconSize::Small)
                        .color(Color::Warning),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(Label::new(text).size(LabelSize::Small)),
                )
                .child(
                    div().debug_selector(|| "git-graph-pack-refs".into()).child(
                        Button::new(
                            "git-graph-pack-refs",
                            crate::many_refs::pack_refs_button_label(stage),
                        )
                        .label_size(LabelSize::Small)
                        .disabled(stage.is_some())
                        .tooltip(Tooltip::text(i18n::t!(
                            "Runs git pack-refs --all. The branches and tags themselves do not change. With tens of thousands of them it takes about a minute, and other Git operations wait meanwhile."
                        )))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.pack_refs(window, cx);
                        })),
                    ),
                )
                .into_any_element(),
        )
    }

    fn pack_refs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repository) = self.get_repository(cx) else {
            return;
        };
        crate::many_refs::confirm_and_pack_refs(repository, self.workspace.clone(), window, cx);
    }

    /// Asks git which tag the selected commit came after, for the detail
    /// panel's 「直近のタグ」 line, once the selection has rested on it for
    /// [`TAG_DISTANCE_DEBOUNCE`].
    fn refresh_selected_commit_tag_distance(&mut self, cx: &mut Context<Self>) {
        let Some(sha) = self
            .selected_entry_idx
            .and_then(|idx| self.graph_data.commits.get(idx))
            .map(|commit| commit.data.sha)
        else {
            self.selected_commit_tag_distance = None;
            self._selected_commit_tag_distance_task = None;
            return;
        };
        if self
            .selected_commit_tag_distance
            .as_ref()
            .is_some_and(|(loaded_sha, _)| *loaded_sha == sha)
        {
            return;
        }
        let Some(repository) = self.get_repository(cx) else {
            return;
        };
        self.selected_commit_tag_distance = None;
        let requester = cx.entity_id();
        // Replacing the task drops a request still waiting out its debounce.
        self._selected_commit_tag_distance_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(TAG_DISTANCE_DEBOUNCE).await;
            let receiver = repository.update(cx, |repository, _cx| {
                repository.latest_describe(sha.to_string(), requester)
            });
            let described = match receiver.await {
                Ok(Ok(described)) => described,
                Ok(Err(error)) => {
                    log::warn!("git describe failed: {error:#}");
                    None
                }
                Err(_) => return,
            };
            this.update(cx, |this, cx| {
                this.selected_commit_tag_distance = Some((sha, described));
                cx.notify();
            })
            .ok();
        }));
    }

    fn set_log_source(&mut self, log_source: LogSource, cx: &mut Context<Self>) {
        if self.log_source == log_source {
            return;
        }
        // The file-history layout has no graph column, and the column widths
        // were sized for it when the tab was created, so a tab cannot move
        // between the two layouts.
        if matches!(self.log_source, LogSource::Path(_)) != matches!(log_source, LogSource::Path(_))
        {
            return;
        }
        self.log_source = log_source;
        self.selected_entry_idx = None;
        self.multi_selection.clear();
        self.selection_anchor_idx = None;
        self.selected_commit_diff = None;
        self.selected_commit_diff_stats = None;
        self.selected_commit_message = None;
        self._selected_commit_message_task = None;
        self.selected_commit_tag_distance = None;
        self.invalidate_state(cx);
        self.fetch_initial_graph_data(cx);
        cx.emit(ItemEvent::Edit);
        cx.notify();
    }

    /// Shows what git said about an operation. A non-zero exit status is git
    /// declining (a conflict, a dirty worktree); an `Err` means the command
    /// could not run at all.
    fn report_git_result(
        &self,
        headline: &str,
        result: Result<
            anyhow::Result<git::repository::GitCommandOutput>,
            futures::channel::oneshot::Canceled,
        >,
        cx: &mut App,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let (message, failed) = match result {
            Ok(Ok(output)) if output.succeeded() => (i18n::t!("{headline}: done"), false),
            Ok(Ok(output)) => {
                let failure = output.failure_message();
                (i18n::t!("{headline}: failed\n{failure}"), true)
            }
            Ok(Err(error)) => {
                let error = format!("{error:#}");
                (i18n::t!("{headline}: could not run\n{error}"), true)
            }
            Err(_) => return,
        };
        workspace.update(cx, |workspace, cx| {
            let toast = StatusToast::new(message, cx, move |this, _cx| {
                this.icon(
                    Icon::new(if failed {
                        IconName::XCircle
                    } else {
                        IconName::GitBranch
                    })
                    .size(IconSize::Small)
                    .color(if failed { Color::Error } else { Color::Muted }),
                )
                .dismiss_button(true)
            });
            workspace.toggle_status_toast(toast, cx);
        });
    }

    /// The picked rows, newest first, as the graph shows them.
    fn selected_rows(&self) -> Vec<usize> {
        if self.multi_selection.is_empty() {
            self.selected_entry_idx.into_iter().collect()
        } else {
            self.multi_selection.iter().copied().collect()
        }
    }

    /// The picked commits in the order git wants for `cherry-pick`: oldest
    /// first. Graph rows run newest first, so this reverses them.
    fn selected_shas_oldest_first(&self) -> Vec<String> {
        self.selected_rows()
            .into_iter()
            .rev()
            .filter_map(|row| self.graph_data.commits.get(row))
            .map(|commit| commit.data.sha.to_string())
            .collect()
    }

    /// The picked commits' subjects, oldest first, which is what the single
    /// commit that replaces them says by default. A subject that git has not
    /// handed over yet is left out; both callers let the user edit or confirm
    /// the message before it is used.
    fn selected_subjects_oldest_first(&mut self, cx: &mut Context<Self>) -> Vec<String> {
        let shas = self
            .selected_rows()
            .into_iter()
            .rev()
            .filter_map(|row| self.graph_data.commits.get(row))
            .map(|commit| commit.data.sha)
            .collect::<Vec<_>>();
        let Some(repository) = self.get_repository(cx) else {
            return Vec::new();
        };
        repository.update(cx, |repository, cx| {
            shas.iter()
                .filter_map(|sha| match repository.fetch_commit_data(*sha, false, cx) {
                    CommitDataState::Loaded(data) => Some(data.subject.to_string()),
                    CommitDataState::Loading(_) => None,
                })
                .collect()
        })
    }

    /// Whether the newest picked row sits on the branch that is checked out.
    /// `squash_range` rewrites that branch, so squashing commits that belong to
    /// some other branch shown by 「すべてのブランチ」 would rewrite the wrong
    /// history.
    fn selection_is_on_current_branch(&self, cx: &App) -> bool {
        let Some(newest) = self
            .selected_rows()
            .first()
            .and_then(|row| self.graph_data.commits.get(*row))
            .map(|commit| commit.data.sha)
        else {
            return false;
        };
        let Some(head_sha) = self.get_repository(cx).and_then(|repository| {
            repository
                .read(cx)
                .snapshot()
                .head_commit
                .as_ref()
                .and_then(|commit| Oid::try_from(commit.sha.as_ref()).ok())
        }) else {
            return false;
        };
        commit_reaches(&self.graph_data.commits, head_sha, newest)
    }

    /// Whether HEAD is a branch rather than detached, as the last status scan saw it.
    fn head_is_branch(&self, cx: &App) -> bool {
        self.get_repository(cx)
            .is_some_and(|repository| repository.read(cx).branch.is_some())
    }

    /// Refuses an operation that rewrites history while the working tree still
    /// has uncommitted changes, which is the most common way for it to fail
    /// half-way and leave a beginner stuck.
    fn worktree_is_dirty(&self, cx: &App) -> bool {
        self.get_repository(cx).is_some_and(|repository| {
            repository
                .read(cx)
                .cached_status()
                .any(|entry| entry.status.has_changes())
        })
    }

    fn cherry_pick_rows(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let shas = self.selected_shas_oldest_first();
        if shas.is_empty() {
            return;
        }
        let Some(repository) = self.get_repository(cx) else {
            return;
        };
        let detail = i18n::tn!(
            shas.len(),
            "Adds the changes of the {count} picked commit to the tip of the current branch as a new commit (cherry-pick).",
            "Adds the changes of the {count} picked commits to the tip of the current branch as new commits (cherry-pick)."
        );
        let confirmation = window.prompt(
            gpui::PromptLevel::Info,
            i18n::t!("Cherry-pick the selected commits onto the current branch?"),
            Some(&detail),
            &[i18n::tc!("git prompt", "Cherry-Pick"), i18n::t!("Cancel")],
            cx,
        );
        cx.spawn(async move |this, cx| {
            if confirmation.await.unwrap_or(1) != 0 {
                return;
            }
            let receiver =
                repository.update(cx, |repository, cx| repository.cherry_pick(shas, false, cx));
            let result = receiver.await;
            this.update(cx, |this, cx| {
                this.report_git_result(i18n::tc!("git result", "Cherry-pick"), result, cx);
                this.invalidate_state(cx);
                this.fetch_initial_graph_data(cx);
            })
            .ok();
        })
        .detach();
    }

    fn squash_rows(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rows: BTreeSet<usize> = self.selected_rows().into_iter().collect();
        if rows.len() < 2
            || !is_contiguous_commit_range(&self.graph_data.commits, &rows)
            || !self.selection_is_on_current_branch(cx)
        {
            return;
        }
        let (Some(newest), Some(oldest)) = (rows.iter().next(), rows.iter().next_back()) else {
            return;
        };
        let (Some(newest), Some(oldest)) = (
            self.graph_data.commits.get(*newest),
            self.graph_data.commits.get(*oldest),
        ) else {
            return;
        };
        let newest_sha = newest.data.sha.to_string();
        let oldest_sha = oldest.data.sha.to_string();
        if self.worktree_is_dirty(cx) {
            self.report_dirty_worktree(i18n::tc!("git result", "Squash"), cx);
            return;
        }
        // The squashed commit keeps what the picked commits said, so that the
        // messages are not thrown away without the user ever seeing them.
        let subjects = self.selected_subjects_oldest_first(cx);
        let message = if subjects.is_empty() {
            i18n::tn!(
                rows.len(),
                "Squash {count} commit",
                "Squash {count} commits"
            )
        } else {
            subjects.join("\n")
        };
        let Some(repository) = self.get_repository(cx) else {
            return;
        };
        let first_line = message.lines().next().unwrap_or_default();
        let detail = i18n::tn!(
            rows.len(),
            "The {count} commit becomes one, and its SHA changes (squash).\nMessage after squashing: {first_line}",
            "The {count} commits become one, and each SHA changes (squash).\nMessage after squashing: {first_line}"
        );
        let confirmation = window.prompt(
            gpui::PromptLevel::Warning,
            i18n::t!("Squash the selected commits into one?"),
            Some(&detail),
            &[i18n::tc!("git prompt", "Squash"), i18n::t!("Cancel")],
            cx,
        );
        cx.spawn(async move |this, cx| {
            if confirmation.await.unwrap_or(1) != 0 {
                return;
            }
            // Queued ahead of the squash, which replaces the commit it asks about.
            let pushed = repository.update(cx, |repository, _| {
                repository.commit_is_pushed(oldest_sha.clone())
            });
            let receiver = repository.update(cx, |repository, cx| {
                repository.squash_range(oldest_sha, newest_sha, message, cx)
            });
            let result = receiver.await;
            let left_in_stash = crate::commit_context_menu::left_changes_in_stash(&result);
            let succeeded = matches!(&result, Ok(Ok(output)) if output.succeeded());
            let needs_force_push =
                (succeeded || left_in_stash) && matches!(pushed.await, Ok(Ok(true)));
            this.update(cx, |this, cx| {
                match this.workspace.upgrade() {
                    Some(workspace) if left_in_stash => {
                        crate::commit_context_menu::show_rewrite_left_changes_in_stash(
                            &workspace,
                            i18n::t!("Squashed the commits (squash)."),
                            None,
                            needs_force_push,
                            cx,
                        )
                    }
                    Some(workspace) if needs_force_push => {
                        crate::commit_context_menu::show_rewrite_succeeded(
                            &workspace,
                            {
                                let headline = i18n::tc!("git result", "Squash commits");
                                i18n::t!("{headline}: done")
                            },
                            None,
                            true,
                            cx,
                        )
                    }
                    _ => this.report_git_result(
                        i18n::tc!("git result", "Squash commits"),
                        result,
                        cx,
                    ),
                }
                this.invalidate_state(cx);
                this.fetch_initial_graph_data(cx);
            })
            .ok();
        })
        .detach();
    }

    /// The picked commits, oldest first, with what the todo editor shows for each. A subject git
    /// has not handed over yet falls back to the SHA rather than shifting the rows out of step
    /// with the commits they stand for.
    fn selected_commits_oldest_first(&mut self, cx: &mut Context<Self>) -> Vec<RewriteCommitRow> {
        let shas = self
            .selected_rows()
            .into_iter()
            .rev()
            .filter_map(|row| self.graph_data.commits.get(row))
            .map(|commit| commit.data.sha)
            .collect::<Vec<_>>();
        self.commit_rows_for(&shas, cx)
    }

    /// What the todo editor shows for each of `shas`, in the same order.
    fn commit_rows_for(&mut self, shas: &[Oid], cx: &mut Context<Self>) -> Vec<RewriteCommitRow> {
        let Some(repository) = self.get_repository(cx) else {
            return Vec::new();
        };
        repository.update(cx, |repository, cx| {
            shas.iter()
                .map(|sha| match repository.fetch_commit_data(*sha, false, cx) {
                    CommitDataState::Loaded(data) => RewriteCommitRow {
                        sha: sha.to_string(),
                        subject: data.subject.clone(),
                        message: data.message.clone(),
                    },
                    CommitDataState::Loading(_) => RewriteCommitRow {
                        sha: sha.to_string(),
                        subject: SharedString::from(sha.display_short()),
                        message: SharedString::default(),
                    },
                })
                .collect()
        })
    }

    /// Opens the todo editor for the picked range: the same conditions as squashing, because the
    /// rewrite replays the range onto the branch that is checked out.
    fn open_rewrite_range_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rows: BTreeSet<usize> = self.selected_rows().into_iter().collect();
        if rows.len() < 2 {
            self.report_refused(
                i18n::t!("Pick at least two commits to reorder.").to_string(),
                cx,
            );
            return;
        }
        if !is_contiguous_commit_range(&self.graph_data.commits, &rows) {
            self.report_refused(
                i18n::t!(
                    "Pick an unbroken range. A range that contains a merge commit cannot be rewritten."
                )
                .to_string(),
                cx,
            );
            return;
        }
        if !self.selection_is_on_current_branch(cx) {
            self.report_refused(
                i18n::t!("Pick commits on the current branch.").to_string(),
                cx,
            );
            return;
        }
        if self.worktree_is_dirty(cx) {
            self.report_dirty_worktree(i18n::tc!("git result", "Edit commits"), cx);
            return;
        }
        let (Some(newest), Some(oldest)) = (rows.iter().next(), rows.iter().next_back()) else {
            return;
        };
        let (Some(newest), Some(oldest)) = (
            self.graph_data.commits.get(*newest),
            self.graph_data.commits.get(*oldest),
        ) else {
            return;
        };
        let newest_sha = newest.data.sha.to_string();
        let oldest_sha = oldest.data.sha.to_string();
        let commits = self.selected_commits_oldest_first(cx);
        // Every commit of the range has to have a row: a commit the list left out would be one
        // git is told to leave out of the rewritten history, without the user having said so.
        if commits.len() != rows.len() {
            self.report_refused(
                i18n::t!("Could not load the selected commits. Wait a moment and try again.")
                    .to_string(),
                cx,
            );
            return;
        }
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let graph = cx.weak_entity();
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, |window, cx| {
                RewriteRangeModal::new(
                    commits,
                    Box::new(move |groups, expected_shas, window, cx| {
                        graph
                            .update(cx, |graph, cx| {
                                graph.rewrite_range(
                                    oldest_sha.clone(),
                                    newest_sha.clone(),
                                    groups,
                                    expected_shas,
                                    window,
                                    cx,
                                );
                            })
                            .ok();
                    }),
                    window,
                    cx,
                )
            });
        });
    }

    /// Confirms the rewrite the todo editor collected and runs it.
    fn rewrite_range(
        &mut self,
        oldest_sha: String,
        newest_sha: String,
        groups: Vec<git::repository::RewriteGroup>,
        expected_shas: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repository) = self.get_repository(cx) else {
            return;
        };
        let scratch_branch = git::repository::rewrite_scratch_branch_name(&newest_sha);
        let (branch, work_directory_abs_path, old_head) = {
            let repository = repository.read(cx);
            (
                repository
                    .branch
                    .as_ref()
                    .map(|branch| branch.name().to_string()),
                repository.work_directory_abs_path.clone(),
                repository
                    .head_commit
                    .as_ref()
                    .map(|commit| commit.sha.to_string()),
            )
        };
        let Some(branch) = branch else {
            self.report_refused(
                i18n::t!("A detached HEAD cannot be reordered. Switch to a branch first.")
                    .to_string(),
                cx,
            );
            return;
        };
        let detail = i18n::tn!(
            groups.len(),
            "After the rewrite there will be {count} commit, with a new SHA.\nThe rewrite applies to the current branch \"{branch}\". The work happens on a temporary branch \"{scratch_branch}\", and \"{branch}\" is moved there at the end (the temporary branch is deleted once it succeeds).\nIf the branch was already pushed, it needs a force push afterwards.",
            "After the rewrite there will be {count} commits, each with a new SHA.\nThe rewrite applies to the current branch \"{branch}\". The work happens on a temporary branch \"{scratch_branch}\", and \"{branch}\" is moved there at the end (the temporary branch is deleted once it succeeds).\nIf the branch was already pushed, it needs a force push afterwards."
        );
        let confirmation = window.prompt(
            gpui::PromptLevel::Warning,
            i18n::t!("Rewrite the selected commits like this?"),
            Some(&detail),
            &[i18n::tc!("git prompt", "Rewrite"), i18n::t!("Cancel")],
            cx,
        );
        cx.spawn(async move |this, cx| {
            if confirmation.await.unwrap_or(1) != 0 {
                return;
            }
            // Written before anything runs, and awaited: if the rewrite stops on a conflict it
            // parks the user on the scratch branch, and the panel's banner can only take them back
            // to a branch that was remembered first.
            cx.update(|cx| {
                rewrite_range_modal::remember_rewrite_origin(
                    &work_directory_abs_path,
                    &scratch_branch,
                    &branch,
                    cx,
                )
            })
            .await;
            // Queued ahead of the rewrite: every commit of the range gets a new SHA, and the
            // oldest one is pushed whenever any of them is.
            let pushed = repository.update(cx, |repository, _| {
                repository.commit_is_pushed(oldest_sha.clone())
            });
            let receiver = repository.update(cx, |repository, cx| {
                repository.rewrite_range(oldest_sha, newest_sha, groups, expected_shas, cx)
            });
            let result = receiver.await;
            // The branch has moved all the same; only the user's uncommitted changes did not come
            // back from `--autostash`.
            let left_in_stash = crate::commit_context_menu::left_changes_in_stash(&result);
            let succeeded = matches!(&result, Ok(Ok(output)) if output.succeeded());
            let needs_force_push =
                (succeeded || left_in_stash) && matches!(pushed.await, Ok(Ok(true)));
            if succeeded || left_in_stash {
                cx.update(|cx| {
                    rewrite_range_modal::forget_rewrite_origin(
                        &work_directory_abs_path,
                        &scratch_branch,
                        cx,
                    )
                })
                .await;
            }
            // The branch has moved by now, so this is the commit the "undo" below has to find HEAD
            // at before it resets anything.
            let new_head = if succeeded {
                let receiver = repository.update(cx, |repository, _cx| {
                    repository.run_git_command(vec!["rev-parse".into(), "HEAD".into()], None)
                });
                match receiver.await {
                    Ok(Ok(output)) if output.succeeded() => {
                        Some(output.stdout.trim().to_string()).filter(|sha| !sha.is_empty())
                    }
                    _ => None,
                }
            } else {
                None
            };
            this.update(cx, |this, cx| {
                match (succeeded, old_head) {
                    (_, old_head) if left_in_stash => {
                        if let Some(workspace) = this.workspace.upgrade() {
                            crate::commit_context_menu::show_rewrite_left_changes_in_stash(
                                &workspace,
                                i18n::t!("Reorder commits: done."),
                                old_head.as_deref(),
                                needs_force_push,
                                cx,
                            );
                        }
                    }
                    (true, Some(old_head)) => this.report_rewrite_succeeded(
                        repository,
                        old_head,
                        new_head,
                        needs_force_push,
                        cx,
                    ),
                    _ => this.report_git_result(
                        i18n::tc!("git result", "Reorder commits"),
                        result,
                        cx,
                    ),
                }
                this.invalidate_state(cx);
                this.fetch_initial_graph_data(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Says the rewrite worked and names the commit the branch pointed at before it, which is what
    /// the user needs to find the old history in the reflog.
    ///
    /// The toast's undo button is the same thing done for them, and it is deliberately narrow:
    /// only a clean working tree with HEAD still at the commit the rewrite produced, so that it
    /// cannot throw away work done afterwards. `needs_force_push` (the range had been pushed)
    /// adds the force push the branch now needs.
    fn report_rewrite_succeeded(
        &self,
        repository: Entity<Repository>,
        old_head: String,
        new_head: Option<String>,
        needs_force_push: bool,
        cx: &mut App,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let short_old: String = old_head.chars().take(git::SHORT_SHA_LENGTH).collect();
        let message = i18n::t!(
            "Reorder commits: done\nPrevious tip: {short_old} (can be restored from the reflog)"
        );
        let host = self.workspace.clone();
        let undo = new_head.map(|new_head| -> crate::commit_context_menu::FollowUp {
            Rc::new(move |window, cx| {
                undo_rewrite(
                    repository.clone(),
                    host.clone(),
                    old_head.clone(),
                    new_head.clone(),
                    window,
                    cx,
                );
            })
        });
        crate::commit_context_menu::show_rewrite_succeeded(
            &workspace,
            message,
            undo,
            needs_force_push,
            cx,
        );
    }

    fn report_dirty_worktree(&self, operation: &str, cx: &mut App) {
        self.report_refused(
            i18n::t!("Commit or stash the uncommitted changes first ({operation})."),
            cx,
        );
    }

    /// Says why the graph did not run an operation the user asked for.
    fn report_refused(&self, message: String, cx: &mut App) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            let toast = StatusToast::new(message, cx, |this, _cx| {
                this.icon(
                    Icon::new(IconName::Warning)
                        .size(IconSize::Small)
                        .color(Color::Warning),
                )
                .dismiss_button(true)
            });
            workspace.toggle_status_toast(toast, cx);
        });
    }

    fn copy_selected_shas(&mut self, cx: &mut Context<Self>) {
        let shas = self
            .selected_rows()
            .into_iter()
            .filter_map(|row| self.graph_data.commits.get(row))
            .map(|commit| commit.data.sha.to_string())
            .collect::<Vec<_>>();
        if shas.is_empty() {
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(shas.join("\n")));
    }

    /// Asks for the two refs to compare and opens the comparison tab.
    fn open_ref_comparison(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let options = self.comparable_refs(cx);
        if options.len() < 2 {
            return;
        }
        let Some(repository) = self.get_repository(cx) else {
            return;
        };
        let workspace = self.workspace.clone();
        let repository = repository.downgrade();
        let base_prompt = crate::picker_prompt::prompt(
            i18n::t!("Branch / tag to compare from (the source)"),
            options.clone(),
            workspace.clone(),
            window,
            cx,
        );
        cx.spawn_in(window, async move |_this, cx| {
            let Some(base_index) = base_prompt.await else {
                return;
            };
            let Some(base) = options.get(base_index).cloned() else {
                return;
            };
            // Comparing a ref with itself has no answer to give, so the second
            // picker does not offer the one already chosen.
            let head_options = options
                .iter()
                .filter(|option| **option != base)
                .cloned()
                .collect::<Vec<_>>();
            let Ok(head_prompt) = cx.update(|window, cx| {
                crate::picker_prompt::prompt(
                    i18n::t!("Branch / tag to compare with (the target)"),
                    head_options.clone(),
                    workspace.clone(),
                    window,
                    cx,
                )
            }) else {
                return;
            };
            let Some(head_index) = head_prompt.await else {
                return;
            };
            let Some(head) = head_options.get(head_index).cloned() else {
                return;
            };
            workspace
                .update_in(cx, |workspace, window, cx| {
                    let Some(repository) = repository.upgrade() else {
                        return;
                    };
                    crate::branch_diff::RefComparison::open(
                        workspace, repository, base, head, window, cx,
                    );
                })
                .ok();
        })
        .detach();
    }

    /// Runs what a drag and drop inside the graph stands for, after asking the
    /// user to confirm in the words of the operation.
    fn perform_drop_action(
        &mut self,
        action: GraphDropAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repository) = self.get_repository(cx) else {
            return;
        };
        match action {
            GraphDropAction::CherryPickOntoHead { shas, branch } => {
                let (title, detail) = cherry_pick_drop_prompt(&shas, &branch, false);
                let confirmation = window.prompt(
                    gpui::PromptLevel::Info,
                    &title,
                    Some(&detail),
                    &[i18n::tc!("git prompt", "Cherry-Pick"), i18n::t!("Cancel")],
                    cx,
                );
                let shas = shas.iter().map(|sha| sha.to_string()).collect::<Vec<_>>();
                cx.spawn(async move |this, cx| {
                    if confirmation.await.unwrap_or(1) != 0 {
                        return;
                    }
                    let receiver = repository
                        .update(cx, |repository, cx| repository.cherry_pick(shas, false, cx));
                    let result = receiver.await;
                    this.update(cx, |this, cx| {
                        this.report_git_result(i18n::tc!("git result", "Cherry-pick"), result, cx);
                        this.invalidate_state(cx);
                        this.fetch_initial_graph_data(cx);
                    })
                    .ok();
                })
                .detach();
            }
            GraphDropAction::CherryPickAfterSwitch { shas, branch } => {
                let (title, detail) = cherry_pick_drop_prompt(&shas, &branch, true);
                let confirmation = window.prompt(
                    gpui::PromptLevel::Warning,
                    &title,
                    Some(&detail),
                    &[
                        i18n::tc!("git prompt", "Switch and Cherry-Pick"),
                        i18n::t!("Cancel"),
                    ],
                    cx,
                );
                let shas = shas.iter().map(|sha| sha.to_string()).collect::<Vec<_>>();
                cx.spawn(async move |this, cx| {
                    if confirmation.await.unwrap_or(1) != 0 {
                        return;
                    }
                    let switched = repository.update(cx, |repository, _cx| {
                        repository.change_branch(branch.to_string())
                    });
                    match switched.await {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => {
                            this.update(cx, |this, cx| {
                                this.report_git_result(
                                    i18n::tc!("git result", "Switch branch"),
                                    Ok(Err(error)),
                                    cx,
                                );
                            })
                            .ok();
                            return;
                        }
                        Err(_) => return,
                    }
                    let receiver = repository
                        .update(cx, |repository, cx| repository.cherry_pick(shas, false, cx));
                    let result = receiver.await;
                    this.update(cx, |this, cx| {
                        this.report_git_result(i18n::tc!("git result", "Cherry-pick"), result, cx);
                        this.invalidate_state(cx);
                        this.fetch_initial_graph_data(cx);
                    })
                    .ok();
                })
                .detach();
            }
            GraphDropAction::ResetHeadBranch { branch, sha } => {
                let short_sha = sha.display_short();
                let detail = i18n::t!(
                    "Resets the current branch {branch} to {short_sha}.\n- Undo Commits Only (--soft): the changes stay staged\n- Unstage Too (--mixed): the changes stay in the working tree\n- Discard Everything (--hard): uncommitted changes are lost too"
                );
                let choice = window.prompt(
                    gpui::PromptLevel::Warning,
                    &i18n::t!("Reset {branch} to this commit?"),
                    Some(&detail),
                    &[
                        i18n::t!("Undo Commits Only (--soft)"),
                        i18n::t!("Unstage Too (--mixed)"),
                        i18n::t!("Discard Everything (--hard)"),
                        i18n::t!("Cancel"),
                    ],
                    cx,
                );
                cx.spawn(async move |this, cx| {
                    let mode = match choice.await {
                        Ok(0) => ResetMode::Soft,
                        Ok(1) => ResetMode::Mixed,
                        Ok(2) => ResetMode::Hard,
                        _ => return,
                    };
                    let receiver = repository.update(cx, |repository, cx| {
                        repository.reset_to(sha.to_string(), mode, cx)
                    });
                    let result = receiver.await;
                    this.update(cx, |this, cx| {
                        this.report_git_result(i18n::tc!("git result", "Reset branch"), result, cx);
                        this.invalidate_state(cx);
                        this.fetch_initial_graph_data(cx);
                    })
                    .ok();
                })
                .detach();
            }
            GraphDropAction::MoveBranchRef { branch, sha } => {
                let holder = worktree_holding_branch(
                    repository.read(cx).snapshot().linked_worktrees(),
                    &branch,
                )
                .map(|worktree| worktree.path.display().to_string());
                if let Some(path) = holder {
                    self.report_refused(
                        i18n::t!(
                            "{branch} is checked out in another worktree ({path}), so it cannot be moved."
                        ),
                        cx,
                    );
                    return;
                }
                let short_sha = sha.display_short();
                let detail = i18n::t!(
                    "{branch}, which is not checked out, will point to {short_sha}. The working tree does not change."
                );
                let confirmation = window.prompt(
                    gpui::PromptLevel::Warning,
                    &i18n::t!("Move {branch} to this commit?"),
                    Some(&detail),
                    &[i18n::tc!("git prompt", "Move"), i18n::t!("Cancel")],
                    cx,
                );
                cx.spawn(async move |this, cx| {
                    if confirmation.await.unwrap_or(1) != 0 {
                        return;
                    }
                    let receiver = repository.update(cx, |repository, _cx| {
                        repository.update_ref(format!("refs/heads/{branch}"), sha.to_string())
                    });
                    let result = receiver.await.map(|inner| {
                        inner.map(|()| git::repository::GitCommandOutput {
                            exit_code: Some(0),
                            ..Default::default()
                        })
                    });
                    this.update(cx, |this, cx| {
                        this.report_git_result(i18n::tc!("git result", "Move branch"), result, cx);
                        this.invalidate_state(cx);
                        this.fetch_initial_graph_data(cx);
                    })
                    .ok();
                })
                .detach();
            }
            GraphDropAction::MoveCommit { sha, target } => {
                self.open_move_commit_modal(sha, target, window, cx);
            }
            GraphDropAction::Fixup { sha, repo_paths } => {
                let Some(workspace) = self.workspace.upgrade() else {
                    return;
                };
                // The fixup asks for confirmation in a modal of the workspace's, and the graph is
                // still being updated by the drop here.
                window.defer(cx, move |window, cx| {
                    crate::commit_context_menu::fixup_commit(
                        repository,
                        workspace,
                        sha.to_string(),
                        Some(repo_paths),
                        window,
                        cx,
                    );
                });
            }
            GraphDropAction::Refused(reason) => {
                self.report_refused(reason.to_string(), cx);
            }
        }
    }

    fn handle_graph_drop(
        &mut self,
        dragged: GraphDragEnd,
        target: GraphDragEnd,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(action) = graph_drop_action(&dragged, &target) else {
            return;
        };
        self.perform_drop_action(action, window, cx);
    }

    /// A commit row dropped on another commit row.
    fn handle_commit_drop_on_row(
        &mut self,
        dragged: &DraggedCommit,
        row: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.hovered_drop_chip = None;
        let Some(target) = self
            .graph_data
            .commits
            .get(row)
            .map(|commit| commit.data.sha)
        else {
            return;
        };
        self.handle_graph_drop(
            GraphDragEnd::dragged_commit(dragged),
            GraphDragEnd::Commit(target),
            window,
            cx,
        );
    }

    /// Files from the git panel dropped on a commit row, which puts them into that commit once
    /// the user confirms (fixup).
    fn handle_files_drop_on_row(
        &mut self,
        dragged: &crate::git_panel::DraggedGitPanelEntries,
        row: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(target) = self
            .graph_data
            .commits
            .get(row)
            .map(|commit| commit.data.sha)
        else {
            return;
        };
        let graph_repository = self
            .get_repository(cx)
            .map(|repository| repository.entity_id());
        let dragged_repository = dragged
            .repository
            .upgrade()
            .map(|repository| repository.entity_id());
        if let Some(reason) = dropped_files_refusal(dragged_repository, graph_repository) {
            self.report_refused(reason.to_string(), cx);
            return;
        }
        self.handle_graph_drop(
            GraphDragEnd::Files(dragged.repo_paths.clone()),
            GraphDragEnd::Commit(target),
            window,
            cx,
        );
    }

    fn head_commit_sha(&self, cx: &App) -> Option<Oid> {
        self.get_repository(cx).and_then(|repository| {
            repository
                .read(cx)
                .snapshot()
                .head_commit
                .as_ref()
                .and_then(|commit| Oid::try_from(commit.sha.as_ref()).ok())
        })
    }

    /// See [`CachedRewritableHistory`].
    fn cached_rewritable_history(&mut self, cx: &App) -> Option<Rc<RewritableHistory>> {
        let head = self.head_commit_sha(cx)?;
        let commit_count = self.graph_data.commits.len();
        let first_commit = self
            .graph_data
            .commits
            .first()
            .map(|commit| commit.data.sha);
        if let Some(cached) = &self.rewritable_history_cache
            && cached.head == head
            && cached.commit_count == commit_count
            && cached.first_commit == first_commit
        {
            return Some(cached.history.clone());
        }
        let history = Rc::new(rewritable_history(&self.graph_data.commits, head));
        self.rewritable_history_cache = Some(CachedRewritableHistory {
            head,
            commit_count,
            first_commit,
            history: history.clone(),
        });
        Some(history)
    }

    /// What the drag in flight would do on the commit rows (see [`RowDropContext`]).
    fn row_drop_context(&mut self, cx: &App) -> RowDropContext {
        if !cx.has_active_drag() {
            self.rewritable_history_cache = None;
            self.hovered_drop_chip = None;
            return RowDropContext::Nothing;
        }
        if let Some(dragged) = cx.active_drag_value::<DraggedRef>() {
            return RowDropContext::Ref {
                name: dragged.name.clone(),
                kind: dragged.kind,
            };
        }
        if let Some(dragged) = cx.active_drag_value::<DraggedCommit>() {
            if dragged.selection.len() > 1 {
                return RowDropContext::Nothing;
            }
            let dragged = dragged.sha;
            return match self.cached_rewritable_history(cx) {
                Some(history) if history.commits.contains(&dragged) => {
                    RowDropContext::MoveCommit { dragged, history }
                }
                _ => RowDropContext::Nothing,
            };
        }
        if let Some(dragged) = cx.active_drag_value::<crate::git_panel::DraggedGitPanelEntries>() {
            let graph_repository = self
                .get_repository(cx)
                .map(|repository| repository.entity_id());
            let dragged_repository = dragged
                .repository
                .upgrade()
                .map(|repository| repository.entity_id());
            if dropped_files_refusal(dragged_repository, graph_repository).is_some() {
                return RowDropContext::Nothing;
            }
            return match self.cached_rewritable_history(cx) {
                Some(history) => RowDropContext::Files { history },
                None => RowDropContext::Nothing,
            };
        }
        RowDropContext::Nothing
    }

    /// The row whose branch chip a dragged commit hangs over, with what dropping it there does.
    fn chip_drop_note(&self, cx: &App) -> Option<(usize, SharedString)> {
        let dragged = cx.active_drag_value::<DraggedCommit>()?;
        let (row, branch, kind) = self.hovered_drop_chip.as_ref()?;
        Some((
            *row,
            chip_drop_hint(dragged.selection.len(), branch, *kind)?,
        ))
    }

    /// Opens the todo editor for a commit dragged onto another: the commits between the two, with
    /// the dragged one already moved to where it was dropped. Confirming the modal is what
    /// rewrites the branch, and the rewrite checks the range again before it runs.
    fn open_move_commit_modal(
        &mut self,
        sha: Oid,
        target: Oid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repository) = self.get_repository(cx) else {
            return;
        };
        if repository.read(cx).branch.is_none() {
            self.report_refused(
                i18n::t!("A detached HEAD cannot be reordered. Switch to a branch first.")
                    .to_string(),
                cx,
            );
            return;
        }
        let Some(head) = self.head_commit_sha(cx) else {
            self.report_refused(
                i18n::t!(
                    "The tip commit of the current branch is not known, so nothing can be reordered."
                )
                .to_string(),
                cx,
            );
            return;
        };
        // Worked out afresh rather than from the cache the highlighting uses: this is what the
        // rewrite is built from.
        let history = rewritable_history(&self.graph_data.commits, head);
        let plan = {
            let commits = &self.graph_data.commits;
            plan_commit_move(&history.commits, sha, target, |sha| {
                commit_reaches(commits, head, sha)
            })
        };
        let plan = match plan {
            Ok(plan) => plan,
            Err(reason) => {
                self.report_refused(reason.to_string(), cx);
                return;
            }
        };
        if self.worktree_is_dirty(cx) {
            self.report_dirty_worktree(i18n::tc!("git result", "Reorder commits"), cx);
            return;
        }
        let (Some(oldest), Some(newest)) = (
            plan.commits_oldest_first.first(),
            plan.commits_oldest_first.last(),
        ) else {
            return;
        };
        let oldest_sha = oldest.to_string();
        let newest_sha = newest.to_string();
        let commits = self.commit_rows_for(&plan.commits_oldest_first, cx);
        if commits.len() != plan.commits_oldest_first.len() {
            self.report_refused(
                i18n::t!("Could not load the commits to reorder. Wait a moment and try again.")
                    .to_string(),
                cx,
            );
            return;
        }
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let graph = cx.weak_entity();
        let initial_order = plan.initial_order;
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, |window, cx| {
                RewriteRangeModal::with_initial_order(
                    commits,
                    &initial_order,
                    Box::new(move |groups, expected_shas, window, cx| {
                        graph
                            .update(cx, |graph, cx| {
                                graph.rewrite_range(
                                    oldest_sha.clone(),
                                    newest_sha.clone(),
                                    groups,
                                    expected_shas,
                                    window,
                                    cx,
                                );
                            })
                            .ok();
                    }),
                    window,
                    cx,
                )
            });
        });
    }

    /// The context menu shown when more than one row is picked. The shared
    /// commit menu only ever talks about a single commit, so the graph builds
    /// its own here.
    fn deploy_multi_selection_menu(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let rows: BTreeSet<usize> = self.selected_rows().into_iter().collect();
        let count = rows.len();
        let selected_shas: Vec<Oid> = rows
            .iter()
            .filter_map(|row| self.graph_data.commits.get(*row))
            .map(|commit| commit.data.sha)
            .collect();
        // This menu has its own entries for putting the selection in the box.
        let merge_request_entries = self.merge_request_menu_entries(&selected_shas, false, cx);
        let rewrite_facts = action_bar::RangeRewriteFacts {
            contiguous: is_contiguous_commit_range(&self.graph_data.commits, &rows),
            on_current_branch: self.selection_is_on_current_branch(cx),
            head_is_branch: self.head_is_branch(cx),
            worktree_dirty: self.worktree_is_dirty(cx),
        };
        let focus_handle = self.focus_handle.clone();
        let graph = cx.entity();
        let context_menu = ContextMenu::build(window, cx, move |menu, _window, _cx| {
            let mut menu = menu.context(focus_handle).header(i18n::tn!(
                count,
                "{count} Commit Selected",
                "{count} Commits Selected"
            ));
            menu = menu.item(
                ContextMenuEntry::new(i18n::tn!(
                    count,
                    "Cherry-Pick the {count} Selected Commit onto the Current Branch",
                    "Cherry-Pick the {count} Selected Commits onto the Current Branch"
                ))
                .handler({
                    let graph = graph.clone();
                    move |window, cx| {
                        graph.update(cx, |graph, cx| graph.cherry_pick_rows(window, cx));
                    }
                })
                .documentation_aside(
                    DocumentationSide::Right,
                    help_aside(git_terms::cherry_pick_selection_help(), None),
                ),
            );
            // Squashing rewrites the checked-out branch in place, so it is only
            // offered for an unbroken range that lives on that branch; the
            // disabled entry says which of the two is missing.
            let range_refusal = action_bar::range_rewrite_refusal(rewrite_facts);
            let squash_aside = help_aside(
                git_terms::squash_selection_help(),
                range_refusal.map(SharedString::from),
            );
            menu = match range_refusal {
                None => menu.item(
                    ContextMenuEntry::new(i18n::tn!(
                        count,
                        "Squash the {count} Selected Commit into One",
                        "Squash the {count} Selected Commits into One"
                    ))
                    .handler({
                        let graph = graph.clone();
                        move |window, cx| {
                            graph.update(cx, |graph, cx| graph.squash_rows(window, cx));
                        }
                    })
                    .documentation_aside(DocumentationSide::Right, squash_aside),
                ),
                Some(reason) => menu.item(
                    ContextMenuEntry::new(i18n::t!("Squash into One — {reason}"))
                        .disabled(true)
                        .documentation_aside(DocumentationSide::Right, squash_aside),
                ),
            };
            // The todo editor rewrites the same range squashing does, so it is offered under the
            // same two conditions.
            let rewrite_aside = help_aside(
                git_terms::rewrite_selection_help(),
                range_refusal.map(SharedString::from),
            );
            menu = match range_refusal {
                None => menu.item(
                    ContextMenuEntry::new(i18n::tn!(
                        count,
                        "Reorder / Squash / Drop the {count} Selected Commit…",
                        "Reorder / Squash / Drop the {count} Selected Commits…"
                    ))
                    .action(EditSelectedCommits.boxed_clone())
                    .handler({
                        let graph = graph.clone();
                        move |window, cx| {
                            graph
                                .update(cx, |graph, cx| graph.open_rewrite_range_modal(window, cx));
                        }
                    })
                    .documentation_aside(DocumentationSide::Right, rewrite_aside),
                ),
                Some(reason) => menu.item(
                    ContextMenuEntry::new(i18n::t!("Reorder / Squash / Drop — {reason}"))
                        .disabled(true)
                        .documentation_aside(DocumentationSide::Right, rewrite_aside),
                ),
            };
            // Both open the merge-request box with the picked commits in it; the first keeps the
            // wording people know the feature by.
            for label in [
                i18n::tn!(
                    count,
                    "Squash the {count} Selected Commit into One Commit on a New MR Branch…",
                    "Squash the {count} Selected Commits into One Commit on a New MR Branch…"
                ),
                i18n::tn!(
                    count,
                    "Add the {count} Selected Commit to the MR Box",
                    "Add the {count} Selected Commits to the MR Box"
                ),
            ] {
                menu = menu.item(
                    ContextMenuEntry::new(label)
                        .handler({
                            let graph = graph.clone();
                            move |window, cx| {
                                graph.update(cx, |graph, cx| {
                                    graph.add_selection_to_merge_request_box(window, cx)
                                });
                            }
                        })
                        .documentation_aside(
                            DocumentationSide::Right,
                            help_aside(git_terms::merge_request_box_help(), None),
                        ),
                );
            }
            menu.map(merge_request_entries).separator().entry(
                i18n::tn!(
                    count,
                    "Copy SHA ({count} Commit)",
                    "Copy SHAs ({count} Commits)"
                ),
                None,
                move |_window, cx| {
                    graph.update(cx, |graph, cx| graph.copy_selected_shas(cx));
                },
            )
        });
        self.set_context_menu(context_menu, position, self.selected_entry_idx, window, cx);
    }

    /// The row of controls above the table: which commits to show, the tag
    /// list, and the entry point for comparing two refs. File-history tabs are about a
    /// single file, so they keep the plain table.
    fn render_graph_toolbar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        if matches!(self.log_source, LogSource::Path(_)) {
            return Empty.into_any_element();
        }
        // The tag menu is built when it is opened, so it can only show tags
        // that are already in hand; loading them here means the very first
        // open is populated instead of reading 「読み込み中」.
        if self.tags.is_none() && self._tags_task.is_none() {
            self.load_tags(cx);
        }
        let color = cx.theme().colors();
        let head_branch_name = self.head_branch_name(cx);
        let source_label: SharedString = match &self.log_source {
            LogSource::All => i18n::tc!("log source", "All Branches").into(),
            LogSource::Branch(branch) => branch.clone(),
            LogSource::Sha(sha) => SharedString::from(sha.display_short()),
            LogSource::Path(_) => i18n::tc!("log source", "File History").into(),
        };
        let graph = cx.entity();
        let selected_count = self.selected_rows().len();

        h_flex()
            .w_full()
            .px_1p5()
            .py_1()
            .gap_1p5()
            .border_b_1()
            .border_color(color.border_variant)
            .child(
                Label::new(i18n::t!("Show:"))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                self.help_hover_area(
                    "git-graph-log-source-hover",
                    action_bar::HoveredHelp::Toolbar(action_bar::ToolbarHelp::LogSource),
                    cx,
                )
                .child(
                    PopoverMenu::new("git-graph-log-source")
                        .menu({
                            move |window, cx| {
                                let graph = graph.clone();
                                let head_branch_name = head_branch_name.clone();
                                Some(ContextMenu::build(window, cx, move |menu, _window, _cx| {
                                    let mut menu = menu.entry(
                                        i18n::tc!("log source", "All Branches"),
                                        None,
                                        {
                                            let graph = graph.clone();
                                            move |_window, cx| {
                                                graph.update(cx, |graph, cx| {
                                                    graph.set_log_source(LogSource::All, cx);
                                                });
                                            }
                                        },
                                    );
                                    if let Some(branch) = head_branch_name.clone() {
                                        menu = menu.entry(
                                            i18n::t!("Current Branch ({branch})"),
                                            None,
                                            {
                                                let graph = graph.clone();
                                                move |_window, cx| {
                                                    let branch = branch.clone();
                                                    graph.update(cx, |graph, cx| {
                                                        graph.set_log_source(
                                                            LogSource::Branch(branch),
                                                            cx,
                                                        );
                                                    });
                                                }
                                            },
                                        );
                                    }
                                    menu.separator().entry(
                                        i18n::t!("Choose a Branch…"),
                                        None,
                                        move |window, cx| {
                                            graph.update(cx, |graph, cx| {
                                                graph.pick_log_source_branch(window, cx);
                                            });
                                        },
                                    )
                                }))
                            }
                        })
                        .trigger_with_tooltip(
                            Button::new("git-graph-log-source-trigger", source_label)
                                .label_size(LabelSize::Small)
                                .end_icon(
                                    Icon::new(IconName::ChevronDown)
                                        .size(IconSize::XSmall)
                                        .color(Color::Muted),
                                ),
                            Tooltip::text(action_bar::ToolbarHelp::LogSource.help().sentence()),
                        ),
                ),
            )
            .child(Divider::vertical())
            .child(
                self.help_hover_area(
                    "git-graph-tags-hover",
                    action_bar::HoveredHelp::Toolbar(action_bar::ToolbarHelp::TagList),
                    cx,
                )
                .child(
                    Button::new(
                        "git-graph-tags-trigger",
                        i18n::tc!("git graph toolbar", "Tags"),
                    )
                    .label_size(LabelSize::Small)
                    .start_icon(
                        Icon::new(IconName::Bookmark)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .end_icon(
                        Icon::new(IconName::ChevronDown)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .tooltip(Tooltip::text(
                        action_bar::ToolbarHelp::TagList.help().sentence(),
                    ))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_tag_picker(window, cx);
                    })),
                ),
            )
            .child(Divider::vertical())
            .child(
                self.help_hover_area(
                    "git-graph-compare-refs-hover",
                    action_bar::HoveredHelp::Toolbar(action_bar::ToolbarHelp::CompareRefs),
                    cx,
                )
                .child(
                    Button::new(
                        "git-graph-compare-refs",
                        i18n::tc!("git graph toolbar", "Compare…"),
                    )
                    .label_size(LabelSize::Small)
                    .start_icon(
                        Icon::new(IconName::Diff)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .tooltip(Tooltip::text(
                        action_bar::ToolbarHelp::CompareRefs.help().sentence(),
                    ))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_ref_comparison(window, cx);
                    })),
                ),
            )
            .child(self.render_merge_request_box_button(cx))
            .child(div().flex_1())
            .when(selected_count > 1, |this| {
                this.child(
                    Label::new(i18n::tn!(
                        selected_count,
                        "{count} selected",
                        "{count} selected"
                    ))
                    .size(LabelSize::Small)
                    .color(Color::Accent),
                )
                .child(
                    Button::new("git-graph-selection-menu", i18n::t!("Selection Actions…"))
                        .label_size(LabelSize::Small)
                        .tooltip(Tooltip::text(i18n::t!(
                            "cherry-pick / squash / create an MR branch (also on right-click)"
                        )))
                        .on_click(cx.listener(|this, event: &ClickEvent, window, cx| {
                            this.deploy_multi_selection_menu(event.position(), window, cx);
                        })),
                )
            })
            .child(crate::git_guide::guide_button("git-graph-guide", None))
            .into_any_element()
    }

    fn pick_log_source_branch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repository) = self.get_repository(cx) else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let workspace_weak = self.workspace.clone();
        let graph = cx.weak_entity();
        let on_select: crate::branch_picker::SelectBranchCallback =
            Arc::new(move |branch: Branch, _window: &mut Window, cx: &mut App| {
                let branch_name = SharedString::from(branch.name().to_string());
                graph
                    .update(cx, |graph, cx| {
                        graph.set_log_source(LogSource::Branch(branch_name), cx);
                    })
                    .ok();
            });
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, |window, cx| {
                crate::branch_picker::select_modal(
                    workspace_weak,
                    Some(repository),
                    None,
                    on_select,
                    window,
                    cx,
                )
            });
        });
    }
}

impl GitGraph {
    pub(crate) fn repository_id(&self) -> RepositoryId {
        self.repo_id
    }

    /// A file-history tab has no graph column to drag from, and it is about one file rather than
    /// a branch.
    pub(crate) fn can_hold_merge_request_box(&self) -> bool {
        !matches!(self.log_source, LogSource::Path(_))
    }

    pub(crate) fn visible_merge_request_box(&self) -> Option<Entity<MergeRequestBox>> {
        self.merge_request_box
            .clone()
            .filter(|_| self.merge_request_box_open)
    }

    pub(crate) fn merge_request_row_marks(&self, cx: &App) -> Option<Rc<RowMarks>> {
        Some(self.visible_merge_request_box()?.read(cx).row_marks())
    }

    pub(crate) fn toggle_merge_request_box(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.merge_request_box_open {
            self.close_merge_request_box(cx);
        } else {
            self.open_merge_request_box(window, cx);
        }
    }

    pub(crate) fn close_merge_request_box(&mut self, cx: &mut Context<Self>) {
        self.merge_request_box_open = false;
        cx.notify();
    }

    pub(crate) fn open_merge_request_box(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<MergeRequestBox>> {
        if !self.can_hold_merge_request_box() {
            self.report_refused(action_bar::merge_request_box_refusal().to_string(), cx);
            return None;
        }
        let merge_request_box = match &self.merge_request_box {
            Some(merge_request_box) => merge_request_box.clone(),
            None => {
                let graph = cx.weak_entity();
                let git_store = self.git_store.clone();
                let workspace = self.workspace.clone();
                let repo_id = self.repo_id;
                let merge_request_box = cx.new(|cx| {
                    MergeRequestBox::new(git_store, repo_id, workspace, graph, window, cx)
                });
                self._merge_request_box_observation =
                    Some(cx.observe(&merge_request_box, |_, _, cx| cx.notify()));
                self.merge_request_box = Some(merge_request_box.clone());
                merge_request_box
            }
        };
        self.merge_request_box_open = true;
        cx.notify();
        Some(merge_request_box)
    }

    pub(crate) fn add_selection_to_merge_request_box(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let shas: Vec<Oid> = self
            .selected_rows()
            .into_iter()
            .rev()
            .filter_map(|row| self.graph_data.commits.get(row))
            .map(|commit| commit.data.sha)
            .collect();
        self.add_commits_to_merge_request_box(&shas, window, cx);
    }

    fn add_commits_to_merge_request_box(
        &mut self,
        shas: &[Oid],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if shas.is_empty() {
            return;
        }
        let candidates = self.merge_request_candidates(shas, cx);
        let Some(merge_request_box) = self.open_merge_request_box(window, cx) else {
            return;
        };
        merge_request_box.update(cx, |merge_request_box, cx| {
            merge_request_box.add_candidates(candidates, window, cx);
        });
    }

    /// "MR の箱に入れる", "MR から除外する" and "MR の除外をやめる" for the rows a menu is about.
    /// The box has a say only while it is open, and only about the development branch's own
    /// commits when it comes to setting them aside. `offer_add` is false for a merge, which the
    /// box never takes. A menu can stay open while the box is closed, so each entry opens the box
    /// again before changing it rather than changing it out of sight.
    fn merge_request_menu_entries(
        &self,
        shas: &[Oid],
        offer_add: bool,
        cx: &Context<Self>,
    ) -> impl FnOnce(ContextMenu) -> ContextMenu + 'static {
        let choices = self.visible_merge_request_box().map(|merge_request_box| {
            let mut choices = merge_request_box.read(cx).menu_choices(shas);
            if !offer_add {
                choices.addable.clear();
            }
            choices
        });
        let graph = cx.weak_entity();
        let several = shas.len() > 1;
        move |mut menu| {
            let Some(choices) = choices else {
                return menu;
            };
            if choices.addable.is_empty()
                && choices.excludable.is_empty()
                && choices.restorable.is_empty()
            {
                return menu;
            }
            menu = menu.separator();
            if !choices.addable.is_empty() {
                let label = if several {
                    i18n::tn!(
                        choices.addable.len(),
                        "Add {count} Commit to MR Box",
                        "Add {count} Commits to MR Box"
                    )
                } else {
                    i18n::tc!("commit action", "Add to MR Box").to_string()
                };
                let graph = graph.clone();
                let addable = choices.addable;
                menu = menu.item(
                    ContextMenuEntry::new(label)
                        .handler(move |window, cx| {
                            graph
                                .update(cx, |graph, cx| {
                                    graph.add_commits_to_merge_request_box(&addable, window, cx)
                                })
                                .log_err();
                        })
                        .documentation_aside(
                            DocumentationSide::Right,
                            help_aside(git_terms::merge_request_box_help(), None),
                        ),
                );
            }
            if !choices.excludable.is_empty() {
                let label = if several {
                    i18n::tn!(
                        choices.excludable.len(),
                        "Exclude {count} Commit from MR",
                        "Exclude {count} Commits from MR"
                    )
                } else {
                    i18n::tc!("commit action", "Exclude from MR").to_string()
                };
                let graph = graph.clone();
                let excludable = choices.excludable;
                menu = menu.item(
                    ContextMenuEntry::new(label)
                        .handler(move |window, cx| {
                            graph
                                .update(cx, |graph, cx| {
                                    let Some(merge_request_box) =
                                        graph.open_merge_request_box(window, cx)
                                    else {
                                        return;
                                    };
                                    merge_request_box.update(cx, |merge_request_box, cx| {
                                        merge_request_box.exclude_commits(&excludable, window, cx);
                                    });
                                })
                                .log_err();
                        })
                        .documentation_aside(
                            DocumentationSide::Right,
                            help_aside(git_terms::merge_request_exclude_help(), None),
                        ),
                );
            }
            if !choices.restorable.is_empty() {
                let label = if several {
                    i18n::tn!(
                        choices.restorable.len(),
                        "Stop Excluding {count} Commit from MR",
                        "Stop Excluding {count} Commits from MR"
                    )
                } else {
                    i18n::tc!("commit action", "Stop Excluding from MR").to_string()
                };
                let restorable = choices.restorable;
                menu = menu.item(
                    ContextMenuEntry::new(label)
                        .handler(move |window, cx| {
                            graph
                                .update(cx, |graph, cx| {
                                    let Some(merge_request_box) =
                                        graph.open_merge_request_box(window, cx)
                                    else {
                                        return;
                                    };
                                    merge_request_box.update(cx, |merge_request_box, cx| {
                                        merge_request_box.restore_commits(&restorable, window, cx);
                                    });
                                })
                                .log_err();
                        })
                        .documentation_aside(
                            DocumentationSide::Right,
                            help_aside(git_terms::merge_request_restore_help(), None),
                        ),
                );
            }
            menu
        }
    }

    /// What the box needs to know about commits dragged out of the graph: subjects and messages
    /// from the commit data when it has loaded, and whether each one is a merge, which the graph
    /// always knows.
    pub(crate) fn merge_request_candidates(
        &mut self,
        shas: &[Oid],
        cx: &mut Context<Self>,
    ) -> Vec<MergeRequestCandidate> {
        let Some(repository) = self.get_repository(cx) else {
            return Vec::new();
        };
        let wanted: HashSet<Oid> = shas.iter().copied().collect();
        let parent_counts: HashMap<Oid, usize> = self
            .graph_data
            .commits
            .iter()
            .filter(|commit| wanted.contains(&commit.data.sha))
            .map(|commit| (commit.data.sha, commit.data.parents.len()))
            .collect();
        repository.update(cx, |repository, cx| {
            shas.iter()
                .map(|sha| {
                    let (subject, message, loaded_parents) =
                        match repository.fetch_commit_data(*sha, false, cx) {
                            CommitDataState::Loaded(data) => (
                                data.subject.clone(),
                                data.message.clone(),
                                data.parents.len(),
                            ),
                            CommitDataState::Loading(_) => (
                                SharedString::from(sha.display_short()),
                                SharedString::default(),
                                0,
                            ),
                        };
                    let parents = parent_counts
                        .get(sha)
                        .copied()
                        .unwrap_or(0)
                        .max(loaded_parents);
                    MergeRequestCandidate {
                        sha: *sha,
                        subject,
                        message,
                        is_merge: parents > 1,
                    }
                })
                .collect()
        })
    }

    fn render_merge_request_box_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        self.help_hover_area(
            "git-graph-merge-request-box-hover",
            action_bar::HoveredHelp::Toolbar(action_bar::ToolbarHelp::MergeRequest),
            cx,
        )
        .child(
            Button::new(
                "git-graph-merge-request-box",
                i18n::tc!("git graph toolbar", "Create MR"),
            )
            .label_size(LabelSize::Small)
            .start_icon(
                Icon::new(IconName::GitBranch)
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
            )
            .toggle_state(self.merge_request_box_open)
            .tooltip(|_window, cx| {
                Tooltip::for_action(
                    action_bar::ToolbarHelp::MergeRequest.help().sentence(),
                    &git::ToggleMergeRequestBox,
                    cx,
                )
            })
            .on_click(cx.listener(|this, _, window, cx| {
                this.toggle_merge_request_box(window, cx);
            })),
        )
    }

    #[cfg(test)]
    pub(crate) fn select_rows_for_test(&mut self, rows: &[usize]) {
        self.multi_selection = rows.iter().copied().collect();
        self.selected_entry_idx = rows.first().copied();
    }

    /// The menu a right click on row `index` opens, or the multi-selection menu for `None`.
    #[cfg(test)]
    pub(crate) fn open_row_menu_for_test(
        &mut self,
        index: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<ContextMenu>> {
        let position = point(px(20.), px(20.));
        match index {
            Some(index) => self.deploy_entry_context_menu(position, index, None, window, cx),
            None => self.deploy_multi_selection_menu(position, window, cx),
        }
        self.context_menu
            .as_ref()
            .map(|context_menu| context_menu.menu.clone())
    }

    #[cfg(test)]
    pub(crate) fn selected_row_for_test(&self) -> Option<usize> {
        self.selected_entry_idx
    }

    #[cfg(test)]
    pub(crate) fn set_log_source_for_test(
        &mut self,
        log_source: LogSource,
        cx: &mut Context<Self>,
    ) {
        self.set_log_source(log_source, cx);
    }

    /// Where the commit details go otherwise, sharing their resize handle: at 1280px there is no
    /// room for the graph, the details and the box side by side, and the box has to be next to
    /// the rows it is filled from.
    fn render_merge_request_box_column(
        &self,
        merge_request_box: Entity<MergeRequestBox>,
        cx: &App,
    ) -> AnyElement {
        div()
            .h_full()
            .min_w(px(360.))
            .flex_basis(DefiniteLength::Fraction(
                self.commit_details_split_state.read(cx).right_ratio(),
            ))
            .bg(cx.theme().colors().editor_background)
            .child(merge_request_box)
            .into_any_element()
    }
}

impl Render for GitGraph {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // This happens when we changed branches, we should refresh our search as well
        if let QueryState::Pending(query) = &mut self.search_state.state {
            let query = std::mem::take(query);
            self.search_state.state = QueryState::Empty;
            self.search(query, cx);
        }
        let (commit_count, is_loading) = self.commit_count_and_loading_state(cx);

        let error = self.get_repository(cx).and_then(|repo| {
            repo.read(cx)
                .get_graph_data(self.log_source.clone(), self.log_order)
                .and_then(|data| data.error.clone())
        });

        let content = if commit_count == 0 {
            let message = if let Some(error) = &error {
                i18n::t!("Error loading: {}", error)
            } else if is_loading {
                i18n::t!("Loading").to_string()
            } else {
                i18n::t!("No commits found").to_string()
            };
            // The empty graph is where a beginner most needs to know what this
            // tab is for, so the hint names the two things that are actually
            // missing: commits, or a wider 表示 range.
            let hint = (!is_loading && error.is_none()).then(|| {
                i18n::t!(
                    "There are no commits yet, or none in the chosen range. Try widening the range with \"Show\" above."
                )
            });

            v_flex()
                .size_full()
                .gap_1()
                .justify_center()
                .items_center()
                .child(
                    h_flex()
                        .gap_1()
                        .child(
                            Label::new(message)
                                .color(Color::Muted)
                                .size(LabelSize::Large),
                        )
                        .when(is_loading && error.is_none(), |this| {
                            this.child(self.render_loading_spinner(cx))
                        }),
                )
                .children(
                    hint.map(|hint| Label::new(hint).color(Color::Muted).size(LabelSize::Small)),
                )
        } else {
            self.fit_date_column(window, cx);
            let is_path_history = matches!(self.log_source, LogSource::Path(_));
            let header_resize_info =
                HeaderResizeInfo::from_redistributable(&self.column_widths, cx);

            let column_filter = self.column_visibility.clone();

            // The graph column (index 0) only exists in the non-path-history layout and is
            // rendered as a separate canvas outside the table.
            let graph_visible =
                is_path_history || !column_filter.get(0usize).copied().unwrap_or(false);

            let table_offset = if is_path_history { 0 } else { 1 };
            let table_filter = column_filter
                .as_slice()
                .get(table_offset..table_offset + TABLE_COLUMN_COUNT)
                .map(|slice| TableRow::from_vec(slice.to_vec(), TABLE_COLUMN_COUNT))
                .unwrap_or_else(|| TableRow::from_element(false, TABLE_COLUMN_COUNT));
            let header_widths = redistribute_hidden_widths(
                &self.column_widths.read(cx).widths_to_render(),
                Some(&column_filter),
            );
            let header_context = TableRenderContext::for_column_widths(Some(header_widths), true)
                .with_column_filter(Some(column_filter));

            let [
                graph_fraction,
                description_fraction,
                date_fraction,
                author_fraction,
                commit_fraction,
            ] = self.preview_column_fractions(window, cx);
            let table_fraction =
                description_fraction + date_fraction + author_fraction + commit_fraction;
            let table_width_config = self.table_column_width_config(window, cx);

            let table_collapsed = table_fraction <= f32::EPSILON;
            let graph_content_width = self.graph_canvas_content_width();

            h_flex()
                .size_full()
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .size_full()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .on_mouse_down(
                                    MouseButton::Right,
                                    cx.listener(|this, event: &MouseDownEvent, window, cx| {
                                        this.deploy_header_context_menu(event.position, window, cx);
                                        cx.stop_propagation();
                                    }),
                                )
                                .child(render_table_header(
                                    if !is_path_history {
                                        TableRow::from_vec(
                                            vec![
                                                Label::new(i18n::t!("Graph"))
                                                    .color(Color::Muted)
                                                    .truncate()
                                                    .into_any_element(),
                                                Label::new(i18n::t!("Description"))
                                                    .color(Color::Muted)
                                                    .into_any_element(),
                                                Label::new(i18n::t!("Date"))
                                                    .color(Color::Muted)
                                                    .into_any_element(),
                                                Label::new(i18n::t!("Author"))
                                                    .color(Color::Muted)
                                                    .into_any_element(),
                                                Label::new(i18n::t!("Commit"))
                                                    .color(Color::Muted)
                                                    .into_any_element(),
                                            ],
                                            5,
                                        )
                                    } else {
                                        TableRow::from_vec(
                                            vec![
                                                Label::new(i18n::t!("Description"))
                                                    .color(Color::Muted)
                                                    .into_any_element(),
                                                Label::new(i18n::t!("Date"))
                                                    .color(Color::Muted)
                                                    .into_any_element(),
                                                Label::new(i18n::t!("Author"))
                                                    .color(Color::Muted)
                                                    .into_any_element(),
                                                Label::new(i18n::t!("Commit"))
                                                    .color(Color::Muted)
                                                    .into_any_element(),
                                            ],
                                            4,
                                        )
                                    },
                                    header_context,
                                    Some(header_resize_info),
                                    Some(self.column_widths.entity_id()),
                                    cx,
                                )),
                        )
                        .child({
                            let row_height = Self::row_height(window, cx);
                            let selected_entry_idx = self.selected_entry_idx;
                            let hovered_entry_idx = self.hovered_entry_idx;
                            let context_menu_target_index = self
                                .context_menu
                                .as_ref()
                                .and_then(|menu| menu.target_entry_index);
                            let weak_self = cx.weak_entity();
                            let row_drop = self.row_drop_context(cx);
                            let chip_note = self.chip_drop_note(cx);
                            let focus_handle = self.focus_handle.clone();
                            let table_focus_handle =
                                self.table_interaction_state.read(cx).focus_handle.clone();

                            let graph_canvas = div()
                                .id("graph-canvas")
                                .size_full()
                                .overflow_hidden()
                                .child(
                                    div()
                                        .size_full()
                                        .child(self.render_graph_canvas(window, cx)),
                                );

                            let commits_table = Table::new(4)
                                .interactable(&self.table_interaction_state)
                                .hide_row_borders()
                                .hide_row_hover()
                                .width_config(table_width_config)
                                .column_filter(table_filter)
                                .map_row(move |(index, row), window, cx| {
                                    let is_selected = selected_entry_idx == Some(index);
                                    let is_hovered = hovered_entry_idx == Some(index);
                                    let is_context_menu_target =
                                        context_menu_target_index == Some(index);
                                    let table_focus_handle = table_focus_handle.clone();
                                    let is_focused = focus_handle.is_focused(window)
                                        || table_focus_handle.is_focused(window);
                                    let weak = weak_self.clone();
                                    let weak_for_hover = weak.clone();
                                    let weak_for_context_menu = weak.clone();
                                    let weak_for_drop = weak.clone();

                                    let hover_bg = cx.theme().colors().element_hover.opacity(0.6);
                                    let selected_bg = if is_focused {
                                        cx.theme().colors().element_selected
                                    } else {
                                        cx.theme().colors().element_hover
                                    };
                                    let row = decorate_row_for_drops(
                                        row,
                                        index,
                                        &row_drop,
                                        chip_note.as_ref(),
                                        weak_self.clone(),
                                        cx,
                                    );

                                    row.h(row_height)
                                        .cursor_pointer()
                                        .drag_over::<DraggedRef>(|style, _, _, cx| {
                                            style.bg(cx.theme().colors().drop_target_background)
                                        })
                                        .on_drop(move |dragged: &DraggedRef, window, cx| {
                                            let dragged = dragged.clone();
                                            weak_for_drop
                                                .update(cx, |this, cx| {
                                                    let Some(commit) =
                                                        this.graph_data.commits.get(index)
                                                    else {
                                                        return;
                                                    };
                                                    let sha = commit.data.sha;
                                                    this.handle_graph_drop(
                                                        GraphDragEnd::Ref {
                                                            name: dragged.name,
                                                            kind: dragged.kind,
                                                        },
                                                        GraphDragEnd::Commit(sha),
                                                        window,
                                                        cx,
                                                    );
                                                })
                                                .ok();
                                        })
                                        .when(is_selected || is_context_menu_target, |row| {
                                            row.bg(selected_bg)
                                        })
                                        .when(
                                            is_hovered && !is_selected && !is_context_menu_target,
                                            |row| row.bg(hover_bg),
                                        )
                                        .on_hover(move |&is_hovered, _, cx| {
                                            weak_for_hover
                                                .update(cx, |this, cx| {
                                                    if is_hovered {
                                                        if this.hovered_entry_idx != Some(index) {
                                                            this.hovered_entry_idx = Some(index);
                                                            cx.notify();
                                                        }
                                                    } else if this.hovered_entry_idx == Some(index)
                                                    {
                                                        this.hovered_entry_idx = None;
                                                        cx.notify();
                                                    }
                                                })
                                                .ok();
                                        })
                                        .on_click(move |event, window, cx| {
                                            weak.update(cx, |this, cx| {
                                                this.handle_entry_click(
                                                    index,
                                                    event,
                                                    ScrollStrategy::Center,
                                                    Some(&table_focus_handle),
                                                    window,
                                                    cx,
                                                );
                                            })
                                            .ok();
                                        })
                                        .on_mouse_down(
                                            MouseButton::Right,
                                            move |event: &MouseDownEvent, window, cx| {
                                                weak_for_context_menu
                                                    .update(cx, |this, cx| {
                                                        this.handle_entry_secondary_mouse_down(
                                                            index, event, window, cx,
                                                        );
                                                    })
                                                    .ok();
                                            },
                                        )
                                        .into_any_element()
                                })
                                .uniform_list(
                                    "git-graph-commits",
                                    commit_count,
                                    cx.processor(Self::render_table_rows),
                                );

                            bind_redistributable_columns(
                                div()
                                    .relative()
                                    .flex_1()
                                    .w_full()
                                    .overflow_hidden()
                                    .child(
                                        h_flex()
                                            .size_full()
                                            .when(!is_path_history && graph_visible, |this| {
                                                this.child(
                                                    h_flex()
                                                        .id("graph-column")
                                                        .map(|this| {
                                                            if table_collapsed {
                                                                this.w(graph_content_width)
                                                            } else {
                                                                this.w(DefiniteLength::Fraction(
                                                                    graph_fraction,
                                                                ))
                                                            }
                                                        })
                                                        .h_full()
                                                        .min_w_0()
                                                        .overflow_hidden()
                                                        .cursor_pointer()
                                                        .on_scroll_wheel(
                                                            cx.listener(Self::handle_graph_scroll),
                                                        )
                                                        .on_mouse_move(cx.listener(
                                                            Self::handle_graph_mouse_move,
                                                        ))
                                                        .on_click(
                                                            cx.listener(Self::handle_graph_click),
                                                        )
                                                        .on_mouse_down(
                                                            MouseButton::Right,
                                                            cx.listener(
                                                                Self::handle_graph_secondary_mouse_down,
                                                            ),
                                                        )
                                                        .on_hover(cx.listener(
                                                            |this, &is_hovered: &bool, _, cx| {
                                                                if !is_hovered
                                                                    && this
                                                                        .hovered_entry_idx
                                                                        .is_some()
                                                                {
                                                                    this.hovered_entry_idx = None;
                                                                    cx.notify();
                                                                }
                                                            },
                                                        ))
                                                        .child(
                                                            div()
                                                                .relative()
                                                                .flex_1()
                                                                .h_full()
                                                                .min_w_0()
                                                                .overflow_hidden()
                                                                .child(graph_canvas),
                                                        ),
                                                )
                                            })
                                            .child(
                                                div()
                                                    .tab_index(2)
                                                    .tab_group()
                                                    .tab_stop(false)
                                                    .map(|this| {
                                                        if table_collapsed {
                                                            this.flex_1()
                                                        } else {
                                                            this.w(DefiniteLength::Fraction(
                                                                table_fraction,
                                                            ))
                                                        }
                                                    })
                                                    .h_full()
                                                    .min_w_0()
                                                    .child(commits_table),
                                            ),
                                    )
                                    .child(render_redistributable_columns_resize_handles(
                                        &self.column_widths,
                                        Some(&self.column_visibility),
                                        window,
                                        cx,
                                    )),
                                self.column_widths.clone(),
                                Some(self.column_visibility.clone()),
                            )
                        }),
                )
                .on_drag_move::<DraggedSplitHandle>(cx.listener(|this, event, window, cx| {
                    this.commit_details_split_state.update(cx, |state, cx| {
                        state.on_drag_move(event, window, cx);
                    });
                }))
                .on_drop::<DraggedSplitHandle>(cx.listener(|this, _event, _window, cx| {
                    this.commit_details_split_state.update(cx, |state, _cx| {
                        state.commit_ratio();
                    });
                }))
                .map(|this| match self.visible_merge_request_box() {
                    Some(merge_request_box) => this
                        .child(self.render_commit_view_resize_handle(window, cx))
                        .child(self.render_merge_request_box_column(merge_request_box, cx)),
                    None => this.when(self.selected_entry_idx.is_some(), |this| {
                        this.child(self.render_commit_view_resize_handle(window, cx))
                            .child(self.render_commit_detail_panel(window, cx))
                    }),
                })
        };
        // Nothing to drag yet, but the box still has to show: it lists the development branch's
        // commits itself, and a toggle that shows nothing would look broken.
        let content = match self
            .visible_merge_request_box()
            .filter(|_| commit_count == 0)
        {
            Some(merge_request_box) => h_flex()
                .size_full()
                .child(content)
                .child(self.render_merge_request_box_column(merge_request_box, cx)),
            None => content,
        };

        div()
            .key_context("GitGraph")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .on_action(cx.listener(|this, _: &OpenCommitView, window, cx| {
                this.open_selected_commit_view(window, cx);
            }))
            .on_action(cx.listener(Self::copy_selected_commit_sha))
            .on_action(cx.listener(Self::copy_selected_commit_tag))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(|this, _: &FocusSearch, window, cx| {
                this.search_state
                    .editor
                    .update(cx, |editor, cx| editor.focus_handle(cx).focus(window, cx));
                this.activate_search_editor_if_focused(window, cx);
            }))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_prev))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(Self::scroll_up))
            .on_action(cx.listener(Self::scroll_down))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::toggle_changed_files_view))
            .on_action(cx.listener(|this, _: &EditSelectedCommits, window, cx| {
                this.open_rewrite_range_modal(window, cx);
            }))
            .on_action(cx.listener(Self::focus_next_tab_stop))
            .on_action(cx.listener(Self::focus_previous_tab_stop))
            .on_action(cx.listener(|this, _: &SelectNextMatch, _window, cx| {
                this.select_next_match(cx);
            }))
            .on_action(cx.listener(|this, _: &SelectPreviousMatch, _window, cx| {
                this.select_previous_match(cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleCaseSensitive, _window, cx| {
                this.search_state.case_sensitive = !this.search_state.case_sensitive;
                this.search_again_after_change();
                cx.emit(ItemEvent::Edit);
                cx.notify();
            }))
            .child(
                v_flex()
                    .size_full()
                    .child(self.render_search_bar(cx))
                    .children(self.render_search_details(cx))
                    .child(self.render_graph_toolbar(cx))
                    .children(self.render_loose_refs_banner(cx))
                    .children(self.render_first_run_hints(cx))
                    .children(self.render_commit_action_bar(cx))
                    .children(self.render_action_description(cx))
                    // Without `min_h_0` the content cannot get shorter than its contents, where the
                    // MR box's scrolling body counts at full height: the column then overflows and
                    // squeezes the rows above instead.
                    .child(
                        div()
                            .debug_selector(|| "git-graph-content".into())
                            .flex_1()
                            .min_h_0()
                            .child(content),
                    ),
            )
            .children(self.context_menu.as_ref().map(|context_menu| {
                deferred(
                    anchored()
                        .position(context_menu.position)
                        .anchor(Anchor::TopLeft)
                        .child(context_menu.menu.clone()),
                )
                .with_priority(1)
            }))
            .on_action(cx.listener(|_, _: &buffer_search::Deploy, window, cx| {
                window.dispatch_action(Box::new(FocusSearch), cx);
                cx.stop_propagation();
            }))
    }
}

impl EventEmitter<ItemEvent> for GitGraph {}

impl Focusable for GitGraph {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.search_state.editor.read(cx).focus_handle(cx)
    }
}

impl Item for GitGraph {
    type Event = ItemEvent;

    fn added_to_workspace(
        &mut self,
        _workspace: &mut workspace::Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // An editor's focus listeners belong to the window they were
        // registered against, so the search field inside a tab moved to
        // another window has to register them again.
        self.search_state.editor.update(cx, |editor, cx| {
            editor.rebind_window_focus_listeners(window, cx);
        });
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::GitGraph))
    }

    fn tab_tooltip_content(&self, cx: &App) -> Option<TabTooltipContent> {
        let repo_name = self.get_repository(cx).and_then(|repo| {
            repo.read(cx)
                .work_directory_abs_path
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
        });
        let path_history_path = match &self.log_source {
            LogSource::Path(path) => Some(path.as_unix_str().to_string()),
            _ => None,
        };

        Some(TabTooltipContent::Custom(Box::new(Tooltip::element({
            move |_, _| {
                v_flex()
                    .child(Label::new(if path_history_path.is_some() {
                        i18n::t!("Path History")
                    } else {
                        i18n::t!("Git Graph")
                    }))
                    .when_some(path_history_path.clone(), |this, path| {
                        this.child(Label::new(path).color(Color::Muted).size(LabelSize::Small))
                    })
                    .when_some(repo_name.clone(), |this, name| {
                        this.child(Label::new(name).color(Color::Muted).size(LabelSize::Small))
                    })
                    .into_any_element()
            }
        }))))
    }

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        if let LogSource::Path(path) = &self.log_source {
            return path
                .as_ref()
                .file_name()
                .map(|name| SharedString::from(name.to_string()))
                .unwrap_or_else(|| SharedString::from(path.as_unix_str().to_string()));
        }

        self.get_repository(cx)
            .and_then(|repo| {
                repo.read(cx)
                    .work_directory_abs_path
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
            })
            .map_or_else(
                || i18n::t!("Git Graph").into(),
                |name| SharedString::from(name),
            )
    }

    fn show_toolbar(&self) -> bool {
        false
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }

    fn deactivated(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(nav_history) = self.nav_history.as_mut() {
            nav_history.push::<()>(None, None, cx);
        }
    }

    fn set_nav_history(
        &mut self,
        nav_history: ItemNavHistory,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        self.nav_history = Some(nav_history);
    }
}

impl workspace::SerializableItem for GitGraph {
    fn serialized_item_kind() -> &'static str {
        "GitGraph"
    }

    fn cleanup(
        workspace_id: workspace::WorkspaceId,
        alive_items: Vec<workspace::ItemId>,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<gpui::Result<()>> {
        workspace::delete_unloaded_items(
            alive_items,
            workspace_id,
            "git_graphs",
            &persistence::GitGraphsDb::global(cx),
            cx,
        )
    }

    fn deserialize(
        project: Entity<project::Project>,
        workspace: WeakEntity<Workspace>,
        workspace_id: workspace::WorkspaceId,
        item_id: workspace::ItemId,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<gpui::Result<Entity<Self>>> {
        let db = persistence::GitGraphsDb::global(cx);
        let Some((
            repo_work_path,
            log_source_type,
            log_source_value,
            log_order,
            selected_sha,
            search_query,
            search_case_sensitive,
            hidden_columns,
        )) = db.get_git_graph(item_id, workspace_id).ok().flatten()
        else {
            return Task::ready(Err(anyhow::anyhow!("No git graph to deserialize")));
        };

        let state = persistence::SerializedGitGraphState {
            log_source_type,
            log_source_value,
            log_order,
            selected_sha,
            search_query,
            search_case_sensitive,
            hidden_columns,
        };

        let window_handle = window.window_handle();
        let project = project.read(cx);
        let git_store = project.git_store().clone();
        let wait = project.wait_for_initial_scan(cx);

        cx.spawn(async move |cx| {
            wait.await;

            cx.update_window(window_handle, |_, window, cx| {
                let path = repo_work_path.as_path();

                let repositories = git_store.read(cx).repositories();
                let repo_id = repositories.iter().find_map(|(&repo_id, repo)| {
                    if repo.read(cx).snapshot().work_directory_abs_path.as_ref() == path {
                        Some(repo_id)
                    } else {
                        None
                    }
                });

                let Some(repo_id) = repo_id else {
                    return Err(anyhow::anyhow!("Repository not found for path: {:?}", path));
                };

                let log_source = persistence::deserialize_log_source(&state);
                let log_order = persistence::deserialize_log_order(&state);

                let git_graph = cx.new(|cx| {
                    let mut graph =
                        GitGraph::new(repo_id, git_store, workspace, Some(log_source), window, cx);
                    graph.log_order = log_order;

                    if let Some(sha) = &state.selected_sha {
                        graph.select_commit_by_sha(sha.as_str(), cx);
                    }

                    graph
                });

                git_graph.update(cx, |graph, cx| {
                    if let Some(bits) = state.hidden_columns {
                        let cols = graph.column_visibility.cols();
                        let mask = persistence::deserialize_hidden_columns(bits, cols);
                        // Never restore an all-hidden mask (e.g. from corrupt data); the UI
                        // guarantees at least one column stays visible.
                        if mask.iter().any(|is_hidden| !is_hidden) {
                            graph.column_visibility = TableRow::from_vec(mask, cols);
                        }
                    }

                    graph.search_state.case_sensitive =
                        state.search_case_sensitive.unwrap_or(false);

                    if let Some(query) = &state.search_query
                        && !query.is_empty()
                    {
                        graph
                            .search_state
                            .editor
                            .update(cx, |editor, cx| editor.set_text(query.as_str(), window, cx));
                        graph.search(query.clone().into(), cx);
                    }
                });

                Ok(git_graph)
            })?
        })
    }

    fn serialize(
        &mut self,
        workspace: &mut Workspace,
        item_id: workspace::ItemId,
        _closing: bool,
        cx: &mut Context<Self>,
    ) -> Option<Task<gpui::Result<()>>> {
        let workspace_id = workspace.database_id()?;
        let repo = self.get_repository(cx)?;
        let repo_working_path = repo
            .read(cx)
            .snapshot()
            .work_directory_abs_path
            .to_string_lossy()
            .to_string();

        let selected_sha = self
            .selected_entry_idx
            .and_then(|idx| self.graph_data.commits.get(idx))
            .map(|commit| commit.data.sha.to_string());

        let search_query = self.search_state.editor.read(cx).text(cx);
        let search_query = if search_query.is_empty() {
            None
        } else {
            Some(search_query)
        };

        let log_source_type = Some(persistence::serialize_log_source_type(&self.log_source));
        let log_source_value = persistence::serialize_log_source_value(&self.log_source);
        let log_order = Some(persistence::serialize_log_order(&self.log_order));
        let search_case_sensitive = Some(self.search_state.case_sensitive);
        let hidden_columns = Some(persistence::serialize_hidden_columns(
            self.column_visibility.as_slice(),
        ));

        let db = persistence::GitGraphsDb::global(cx);
        Some(cx.background_spawn(async move {
            db.save_git_graph(
                item_id,
                workspace_id,
                repo_working_path,
                log_source_type,
                log_source_value,
                log_order,
                selected_sha,
                search_query,
                search_case_sensitive,
                hidden_columns,
            )
            .await
        }))
    }

    fn should_serialize(&self, event: &Self::Event) -> bool {
        match event {
            ItemEvent::UpdateTab | ItemEvent::Edit => true,
            _ => false,
        }
    }
}

mod persistence {
    use std::{path::PathBuf, str::FromStr};

    use db::{
        query,
        sqlez::{domain::Domain, thread_safe_connection::ThreadSafeConnection},
        sqlez_macros::sql,
    };
    use git::{
        Oid,
        repository::{LogOrder, LogSource, RepoPath},
    };
    use workspace::WorkspaceDb;

    pub struct GitGraphsDb(ThreadSafeConnection);

    impl Domain for GitGraphsDb {
        const NAME: &str = stringify!(GitGraphsDb);

        const MIGRATIONS: &[&str] = &[
            sql!(
                CREATE TABLE git_graphs (
                    workspace_id INTEGER,
                    item_id INTEGER UNIQUE,
                    is_open INTEGER DEFAULT FALSE,

                    PRIMARY KEY(workspace_id, item_id),
                    FOREIGN KEY(workspace_id) REFERENCES workspaces(workspace_id)
                    ON DELETE CASCADE
                ) STRICT;
            ),
            sql!(
                ALTER TABLE git_graphs ADD COLUMN repo_working_path TEXT;
            ),
            sql!(
                ALTER TABLE git_graphs ADD COLUMN log_source_type TEXT;
                ALTER TABLE git_graphs ADD COLUMN log_source_value TEXT;
                ALTER TABLE git_graphs ADD COLUMN log_order TEXT;
                ALTER TABLE git_graphs ADD COLUMN selected_sha TEXT;
                ALTER TABLE git_graphs ADD COLUMN search_query TEXT;
                ALTER TABLE git_graphs ADD COLUMN search_case_sensitive INTEGER;
            ),
            sql!(
                ALTER TABLE git_graphs ADD COLUMN hidden_columns INTEGER;
            ),
        ];
    }

    db::static_connection!(GitGraphsDb, [WorkspaceDb]);

    pub const LOG_SOURCE_ALL: i32 = 0;
    pub const LOG_SOURCE_BRANCH: i32 = 1;
    pub const LOG_SOURCE_SHA: i32 = 2;
    pub const LOG_SOURCE_PATH: i32 = 3;

    pub const LOG_ORDER_DATE: i32 = 0;
    pub const LOG_ORDER_TOPO: i32 = 1;
    pub const LOG_ORDER_AUTHOR_DATE: i32 = 2;
    pub const LOG_ORDER_REVERSE: i32 = 3;

    pub fn serialize_log_source_type(log_source: &LogSource) -> i32 {
        match log_source {
            LogSource::All => LOG_SOURCE_ALL,
            LogSource::Branch(_) => LOG_SOURCE_BRANCH,
            LogSource::Sha(_) => LOG_SOURCE_SHA,
            LogSource::Path(_) => LOG_SOURCE_PATH,
        }
    }

    pub fn serialize_log_source_value(log_source: &LogSource) -> Option<String> {
        match log_source {
            LogSource::All => None,
            LogSource::Branch(branch) => Some(branch.to_string()),
            LogSource::Sha(oid) => Some(oid.to_string()),
            LogSource::Path(path) => Some(path.as_unix_str().to_string()),
        }
    }

    pub fn serialize_log_order(log_order: &LogOrder) -> i32 {
        match log_order {
            LogOrder::DateOrder => LOG_ORDER_DATE,
            LogOrder::TopoOrder => LOG_ORDER_TOPO,
            LogOrder::AuthorDateOrder => LOG_ORDER_AUTHOR_DATE,
            LogOrder::ReverseChronological => LOG_ORDER_REVERSE,
        }
    }

    pub fn deserialize_log_source(state: &SerializedGitGraphState) -> LogSource {
        match state.log_source_type {
            Some(LOG_SOURCE_ALL) => LogSource::All,
            Some(LOG_SOURCE_BRANCH) => state
                .log_source_value
                .as_ref()
                .map(|v| LogSource::Branch(v.clone().into()))
                .unwrap_or_default(),
            Some(LOG_SOURCE_SHA) => state
                .log_source_value
                .as_ref()
                .and_then(|v| Oid::from_str(v).ok())
                .map(LogSource::Sha)
                .unwrap_or_default(),
            Some(LOG_SOURCE_PATH) => state
                .log_source_value
                .as_ref()
                .and_then(|v| RepoPath::new(v).ok())
                .map(LogSource::Path)
                .unwrap_or_default(),
            None | Some(_) => LogSource::default(),
        }
    }

    pub fn deserialize_log_order(state: &SerializedGitGraphState) -> LogOrder {
        match state.log_order {
            Some(LOG_ORDER_DATE) => LogOrder::DateOrder,
            Some(LOG_ORDER_TOPO) => LogOrder::TopoOrder,
            Some(LOG_ORDER_AUTHOR_DATE) => LogOrder::AuthorDateOrder,
            Some(LOG_ORDER_REVERSE) => LogOrder::ReverseChronological,
            _ => LogOrder::default(),
        }
    }

    /// Packs the per-column visibility mask into a bitmask (bit `i` set means column `i` is
    /// hidden), so it fits in a single integer database column regardless of column count.
    pub fn serialize_hidden_columns(hidden: &[bool]) -> i32 {
        hidden.iter().enumerate().fold(
            0,
            |bits, (idx, &is_hidden)| {
                if is_hidden { bits | (1 << idx) } else { bits }
            },
        )
    }

    /// Inverse of [`serialize_hidden_columns`]. Bits beyond `cols` are ignored, and missing
    /// bits default to visible, so a mask saved with a different column count degrades safely.
    pub fn deserialize_hidden_columns(bits: i32, cols: usize) -> Vec<bool> {
        (0..cols).map(|idx| bits & (1 << idx) != 0).collect()
    }

    #[derive(Debug, Default, Clone)]
    pub struct SerializedGitGraphState {
        pub log_source_type: Option<i32>,
        pub log_source_value: Option<String>,
        pub log_order: Option<i32>,
        pub selected_sha: Option<String>,
        pub search_query: Option<String>,
        pub search_case_sensitive: Option<bool>,
        pub hidden_columns: Option<i32>,
    }

    impl GitGraphsDb {
        query! {
            pub async fn save_git_graph(
                item_id: workspace::ItemId,
                workspace_id: workspace::WorkspaceId,
                repo_working_path: String,
                log_source_type: Option<i32>,
                log_source_value: Option<String>,
                log_order: Option<i32>,
                selected_sha: Option<String>,
                search_query: Option<String>,
                search_case_sensitive: Option<bool>,
                hidden_columns: Option<i32>
            ) -> Result<()> {
                INSERT OR REPLACE INTO git_graphs(
                    item_id, workspace_id, repo_working_path,
                    log_source_type, log_source_value, log_order,
                    selected_sha, search_query, search_case_sensitive,
                    hidden_columns
                )
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            }
        }

        query! {
            pub fn get_git_graph(
                item_id: workspace::ItemId,
                workspace_id: workspace::WorkspaceId
            ) -> Result<Option<(
                PathBuf,
                Option<i32>,
                Option<String>,
                Option<i32>,
                Option<String>,
                Option<String>,
                Option<bool>,
                Option<i32>
            )>> {
                SELECT
                    repo_working_path,
                    log_source_type,
                    log_source_value,
                    log_order,
                    selected_sha,
                    search_query,
                    search_case_sensitive,
                    hidden_columns
                FROM git_graphs
                WHERE item_id = ? AND workspace_id = ?
            }
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
impl GitGraph {
    pub fn search_for_test(&mut self, query: SharedString, cx: &mut Context<Self>) {
        self.search(query, cx);
    }

    pub fn search_matches_for_test(&self) -> Vec<Oid> {
        self.search_state.matches.iter().copied().collect()
    }

    pub fn initial_commit_data_for_test(&self) -> Vec<Arc<InitialGraphCommitData>> {
        self.graph_data
            .commits
            .iter()
            .map(|commit| commit.data.clone())
            .collect()
    }

    pub fn commit_count_and_loading_state_for_test(
        &mut self,
        cx: &mut Context<Self>,
    ) -> (usize, bool) {
        self.commit_count_and_loading_state(cx)
    }

    pub fn log_source_for_test(&self) -> &LogSource {
        &self.log_source
    }
}

/// Generates a random commit DAG suitable for testing git graph rendering.
///
/// The commits are ordered newest-first (like git log output), so:
/// - Index 0 = most recent commit (HEAD)
/// - Last index = oldest commit (root, has no parents)
/// - Parents of commit at index I must have index > I
///
/// When `adversarial` is true, generates complex topologies with many branches
/// and octopus merges. Otherwise generates more realistic linear histories
/// with occasional branches.
#[cfg(any(test, feature = "test-support"))]
pub fn generate_random_commit_dag(
    rng: &mut rand::rngs::StdRng,
    num_commits: usize,
    adversarial: bool,
) -> Vec<Arc<InitialGraphCommitData>> {
    use rand::Rng as _;

    if num_commits == 0 {
        return Vec::new();
    }

    let mut commits: Vec<Arc<InitialGraphCommitData>> = Vec::with_capacity(num_commits);
    let oids: Vec<Oid> = (0..num_commits).map(|_| Oid::random(rng)).collect();

    for i in 0..num_commits {
        let sha = oids[i];

        let parents = if i == num_commits - 1 {
            smallvec![]
        } else {
            generate_parents_from_oids(rng, &oids, i, num_commits, adversarial)
        };

        let ref_names = if i == 0 {
            vec!["HEAD".into(), "main".into()]
        } else if adversarial && rng.random_bool(0.1) {
            vec![format!("branch-{i}").into()]
        } else {
            Vec::new()
        };

        commits.push(Arc::new(InitialGraphCommitData {
            sha,
            parents,
            ref_names,
        }));
    }

    commits
}

#[cfg(any(test, feature = "test-support"))]
fn generate_parents_from_oids(
    rng: &mut rand::rngs::StdRng,
    oids: &[Oid],
    current_idx: usize,
    num_commits: usize,
    adversarial: bool,
) -> SmallVec<[Oid; 1]> {
    use rand::{Rng as _, seq::SliceRandom as _};

    let remaining = num_commits - current_idx - 1;
    if remaining == 0 {
        return smallvec![];
    }

    if adversarial {
        let merge_chance = 0.4;
        let octopus_chance = 0.15;

        if remaining >= 3 && rng.random_bool(octopus_chance) {
            let num_parents = rng.random_range(3..=remaining.min(5));
            let mut parent_indices: Vec<usize> = (current_idx + 1..num_commits).collect();
            parent_indices.shuffle(rng);
            parent_indices
                .into_iter()
                .take(num_parents)
                .map(|idx| oids[idx])
                .collect()
        } else if remaining >= 2 && rng.random_bool(merge_chance) {
            let mut parent_indices: Vec<usize> = (current_idx + 1..num_commits).collect();
            parent_indices.shuffle(rng);
            parent_indices
                .into_iter()
                .take(2)
                .map(|idx| oids[idx])
                .collect()
        } else {
            let parent_idx = rng.random_range(current_idx + 1..num_commits);
            smallvec![oids[parent_idx]]
        }
    } else {
        let merge_chance = 0.15;
        let skip_chance = 0.1;

        if remaining >= 2 && rng.random_bool(merge_chance) {
            let first_parent = current_idx + 1;
            let second_parent = rng.random_range(current_idx + 2..num_commits);
            smallvec![oids[first_parent], oids[second_parent]]
        } else if rng.random_bool(skip_chance) && remaining >= 2 {
            let skip = rng.random_range(1..remaining.min(3));
            smallvec![oids[current_idx + 1 + skip]]
        } else {
            smallvec![oids[current_idx + 1]]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{Context, Result, bail};
    use collections::{HashMap, HashSet};
    use fs::FakeFs;
    use git::Oid;
    use git::commit_search::{CommitSearchConditionKind, MergeFilter};
    use git::repository::{CommitData, InitialGraphCommitData};
    use gpui::{TestAppContext, UpdateGlobal, VisualTestContext};
    use project::git_store::{GitStoreEvent, RepositoryEvent};
    use project::{
        GIT_COMMAND_TASK_TAG, Project, TaskSourceKind, task_store::TaskSettingsLocation,
    };
    use rand::prelude::*;
    use serde_json::json;
    use settings::{SettingsStore, ThemeSettingsContent};
    use smallvec::{SmallVec, smallvec};
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            language_model::init(cx);
            // The tag picker's search box is built through the editor factory, which only
            // `editor::init` registers; without it the test depends on another test running first.
            editor::init(cx);
            crate::init(cx);
        });
    }

    fn build_oid_to_row_map(graph: &GraphData) -> HashMap<Oid, usize> {
        graph
            .commits
            .iter()
            .enumerate()
            .map(|(idx, entry)| (entry.data.sha, idx))
            .collect()
    }

    fn verify_commit_order(
        graph: &GraphData,
        commits: &[Arc<InitialGraphCommitData>],
    ) -> Result<()> {
        if graph.commits.len() != commits.len() {
            bail!(
                "Commit count mismatch: graph has {} commits, expected {}",
                graph.commits.len(),
                commits.len()
            );
        }

        for (idx, (graph_commit, expected_commit)) in
            graph.commits.iter().zip(commits.iter()).enumerate()
        {
            if graph_commit.data.sha != expected_commit.sha {
                bail!(
                    "Commit order mismatch at index {}: graph has {:?}, expected {:?}",
                    idx,
                    graph_commit.data.sha,
                    expected_commit.sha
                );
            }
        }

        Ok(())
    }

    fn verify_line_endpoints(graph: &GraphData, oid_to_row: &HashMap<Oid, usize>) -> Result<()> {
        for line in &graph.lines {
            let child_row = *oid_to_row
                .get(&line.child)
                .context("Line references non-existent child commit")?;

            let parent_row = *oid_to_row
                .get(&line.parent)
                .context("Line references non-existent parent commit")?;

            if child_row >= parent_row {
                bail!(
                    "child_row ({}) must be < parent_row ({})",
                    child_row,
                    parent_row
                );
            }

            if line.full_interval.start != child_row {
                bail!(
                    "full_interval.start ({}) != child_row ({})",
                    line.full_interval.start,
                    child_row
                );
            }

            if line.full_interval.end != parent_row {
                bail!(
                    "full_interval.end ({}) != parent_row ({})",
                    line.full_interval.end,
                    parent_row
                );
            }

            if let Some(last_segment) = line.segments.last() {
                let segment_end_row = match last_segment {
                    CommitLineSegment::Straight { to_row } => *to_row,
                    CommitLineSegment::Curve { on_row, .. } => *on_row,
                };

                if segment_end_row != line.full_interval.end {
                    bail!(
                        "last segment ends at row {} but full_interval.end is {}",
                        segment_end_row,
                        line.full_interval.end
                    );
                }
            }
        }

        Ok(())
    }

    fn verify_column_correctness(
        graph: &GraphData,
        oid_to_row: &HashMap<Oid, usize>,
    ) -> Result<()> {
        for line in &graph.lines {
            let child_row = *oid_to_row
                .get(&line.child)
                .context("Line references non-existent child commit")?;

            let parent_row = *oid_to_row
                .get(&line.parent)
                .context("Line references non-existent parent commit")?;

            let child_lane = graph.commits[child_row].lane;
            if line.child_column != child_lane {
                bail!(
                    "child_column ({}) != child's lane ({})",
                    line.child_column,
                    child_lane
                );
            }

            let mut current_column = line.child_column;
            for segment in &line.segments {
                if let CommitLineSegment::Curve { to_column, .. } = segment {
                    current_column = *to_column;
                }
            }

            let parent_lane = graph.commits[parent_row].lane;
            if current_column != parent_lane {
                bail!(
                    "ending column ({}) != parent's lane ({})",
                    current_column,
                    parent_lane
                );
            }
        }

        Ok(())
    }

    fn verify_segment_continuity(graph: &GraphData) -> Result<()> {
        for line in &graph.lines {
            if line.segments.is_empty() {
                bail!("Line has no segments");
            }

            let mut current_row = line.full_interval.start;

            for (idx, segment) in line.segments.iter().enumerate() {
                let segment_end_row = match segment {
                    CommitLineSegment::Straight { to_row } => *to_row,
                    CommitLineSegment::Curve { on_row, .. } => *on_row,
                };

                if segment_end_row < current_row {
                    bail!(
                        "segment {} ends at row {} which is before current row {}",
                        idx,
                        segment_end_row,
                        current_row
                    );
                }

                current_row = segment_end_row;
            }
        }

        Ok(())
    }

    fn verify_line_overlaps(graph: &GraphData) -> Result<()> {
        for line in &graph.lines {
            let child_row = line.full_interval.start;

            let mut current_column = line.child_column;
            let mut current_row = child_row;

            for segment in &line.segments {
                match segment {
                    CommitLineSegment::Straight { to_row } => {
                        for row in (current_row + 1)..*to_row {
                            if row < graph.commits.len() {
                                let commit_at_row = &graph.commits[row];
                                if commit_at_row.lane == current_column {
                                    bail!(
                                        "straight segment from row {} to {} in column {} passes through commit {:?} at row {}",
                                        current_row,
                                        to_row,
                                        current_column,
                                        commit_at_row.data.sha,
                                        row
                                    );
                                }
                            }
                        }
                        current_row = *to_row;
                    }
                    CommitLineSegment::Curve {
                        to_column, on_row, ..
                    } => {
                        current_column = *to_column;
                        current_row = *on_row;
                    }
                }
            }
        }

        Ok(())
    }

    fn verify_keep_shared_parents_on_leftmost_lane(graph: &GraphData) -> Result<()> {
        let mut active_lane_parents: Vec<Option<Oid>> = Vec::new();
        let mut parent_to_lanes: HashMap<Oid, SmallVec<[usize; 1]>> = HashMap::default();

        for (row, entry) in graph.commits.iter().enumerate() {
            let pending_lanes = parent_to_lanes.remove(&entry.data.sha).unwrap_or_default();

            if pending_lanes.len() > 1
                && let Some(expected_lane) = pending_lanes.iter().copied().min()
                && entry.lane != expected_lane
            {
                bail!(
                    "commit {:?} at row {} uses lane {}, but shared parent should use leftmost pending lane {} from {:?}",
                    entry.data.sha,
                    row,
                    entry.lane,
                    expected_lane,
                    pending_lanes
                );
            }

            for lane in pending_lanes {
                let Some(active_lane_parent) = active_lane_parents.get_mut(lane) else {
                    bail!(
                        "commit {:?} at row {} was pending on missing lane {}",
                        entry.data.sha,
                        row,
                        lane
                    );
                };

                if *active_lane_parent != Some(entry.data.sha) {
                    bail!(
                        "commit {:?} at row {} was pending on lane {}, but that lane points to {:?}",
                        entry.data.sha,
                        row,
                        lane,
                        active_lane_parent
                    );
                }

                *active_lane_parent = None;
            }

            for (parent_index, parent) in entry.data.parents.iter().enumerate() {
                let lane = if parent_index == 0 {
                    entry.lane
                } else if let Some(empty_lane) =
                    active_lane_parents.iter().position(Option::is_none)
                {
                    empty_lane
                } else {
                    active_lane_parents.push(None);
                    active_lane_parents.len() - 1
                };

                if lane >= active_lane_parents.len() {
                    active_lane_parents.resize(lane + 1, None);
                }

                active_lane_parents[lane] = Some(*parent);
                parent_to_lanes.entry(*parent).or_default().push(lane);
            }
        }

        Ok(())
    }

    fn verify_coverage(graph: &GraphData) -> Result<()> {
        let mut expected_edges: HashSet<(Oid, Oid)> = HashSet::default();
        for entry in &graph.commits {
            for parent in &entry.data.parents {
                expected_edges.insert((entry.data.sha, *parent));
            }
        }

        let mut found_edges: HashSet<(Oid, Oid)> = HashSet::default();
        for line in &graph.lines {
            let edge = (line.child, line.parent);

            if !found_edges.insert(edge) {
                bail!(
                    "Duplicate line found for edge {:?} -> {:?}",
                    line.child,
                    line.parent
                );
            }

            if !expected_edges.contains(&edge) {
                bail!(
                    "Orphan line found: {:?} -> {:?} is not in the commit graph",
                    line.child,
                    line.parent
                );
            }
        }

        for (child, parent) in &expected_edges {
            if !found_edges.contains(&(*child, *parent)) {
                bail!("Missing line for edge {:?} -> {:?}", child, parent);
            }
        }

        assert_eq!(
            expected_edges.symmetric_difference(&found_edges).count(),
            0,
            "The symmetric difference should be zero"
        );

        Ok(())
    }

    fn verify_merge_line_optimality(
        graph: &GraphData,
        oid_to_row: &HashMap<Oid, usize>,
    ) -> Result<()> {
        for line in &graph.lines {
            let first_segment = line.segments.first();
            let is_merge_line = matches!(
                first_segment,
                Some(CommitLineSegment::Curve {
                    curve_kind: CurveKind::Merge,
                    ..
                })
            );

            if !is_merge_line {
                continue;
            }

            let child_row = *oid_to_row
                .get(&line.child)
                .context("Line references non-existent child commit")?;

            let parent_row = *oid_to_row
                .get(&line.parent)
                .context("Line references non-existent parent commit")?;

            let parent_lane = graph.commits[parent_row].lane;

            let Some(CommitLineSegment::Curve { to_column, .. }) = first_segment else {
                continue;
            };

            let curves_directly_to_parent = *to_column == parent_lane;

            if !curves_directly_to_parent {
                continue;
            }

            let curve_row = child_row + 1;
            let has_commits_in_path = graph.commits[curve_row..parent_row]
                .iter()
                .any(|c| c.lane == parent_lane);

            if has_commits_in_path {
                bail!(
                    "Merge line from {:?} to {:?} curves directly to parent lane {} but there are commits in that lane between rows {} and {}",
                    line.child,
                    line.parent,
                    parent_lane,
                    curve_row,
                    parent_row
                );
            }

            let curve_ends_at_parent = curve_row == parent_row;

            if curve_ends_at_parent {
                if line.segments.len() != 1 {
                    bail!(
                        "Merge line from {:?} to {:?} curves directly to parent (curve_row == parent_row), but has {} segments instead of 1 [MergeCurve]",
                        line.child,
                        line.parent,
                        line.segments.len()
                    );
                }
            } else {
                if line.segments.len() != 2 {
                    bail!(
                        "Merge line from {:?} to {:?} curves directly to parent lane without overlap, but has {} segments instead of 2 [MergeCurve, Straight]",
                        line.child,
                        line.parent,
                        line.segments.len()
                    );
                }

                let is_straight_segment = matches!(
                    line.segments.get(1),
                    Some(CommitLineSegment::Straight { .. })
                );

                if !is_straight_segment {
                    bail!(
                        "Merge line from {:?} to {:?} curves directly to parent lane without overlap, but second segment is not a Straight segment",
                        line.child,
                        line.parent
                    );
                }
            }
        }

        Ok(())
    }

    fn verify_all_invariants(
        graph: &GraphData,
        commits: &[Arc<InitialGraphCommitData>],
    ) -> Result<()> {
        let oid_to_row = build_oid_to_row_map(graph);

        verify_commit_order(graph, commits).context("commit order")?;
        verify_line_endpoints(graph, &oid_to_row).context("line endpoints")?;
        verify_column_correctness(graph, &oid_to_row).context("column correctness")?;
        verify_segment_continuity(graph).context("segment continuity")?;
        verify_merge_line_optimality(graph, &oid_to_row).context("merge line optimality")?;
        verify_keep_shared_parents_on_leftmost_lane(graph)
            .context("keep shared parents on leftmost lane")?;
        verify_coverage(graph).context("coverage")?;
        verify_line_overlaps(graph).context("line overlaps")?;
        Ok(())
    }

    #[test]
    fn test_git_graph_merge_commits() {
        let mut rng = StdRng::seed_from_u64(42);

        let oid1 = Oid::random(&mut rng);
        let oid2 = Oid::random(&mut rng);
        let oid3 = Oid::random(&mut rng);
        let oid4 = Oid::random(&mut rng);

        let commits = vec![
            Arc::new(InitialGraphCommitData {
                sha: oid1,
                parents: smallvec![oid2, oid3],
                ref_names: vec!["HEAD".into()],
            }),
            Arc::new(InitialGraphCommitData {
                sha: oid2,
                parents: smallvec![oid4],
                ref_names: vec![],
            }),
            Arc::new(InitialGraphCommitData {
                sha: oid3,
                parents: smallvec![oid4],
                ref_names: vec![],
            }),
            Arc::new(InitialGraphCommitData {
                sha: oid4,
                parents: smallvec![],
                ref_names: vec![],
            }),
        ];

        let mut graph_data = GraphData::new(8);
        graph_data.add_commits(&commits);

        if let Err(error) = verify_all_invariants(&graph_data, &commits) {
            panic!("Graph invariant violation for merge commits:\n{}", error);
        }
    }

    #[test]
    fn test_git_graph_linear_commits() {
        let mut rng = StdRng::seed_from_u64(42);

        let oid1 = Oid::random(&mut rng);
        let oid2 = Oid::random(&mut rng);
        let oid3 = Oid::random(&mut rng);

        let commits = vec![
            Arc::new(InitialGraphCommitData {
                sha: oid1,
                parents: smallvec![oid2],
                ref_names: vec!["HEAD".into()],
            }),
            Arc::new(InitialGraphCommitData {
                sha: oid2,
                parents: smallvec![oid3],
                ref_names: vec![],
            }),
            Arc::new(InitialGraphCommitData {
                sha: oid3,
                parents: smallvec![],
                ref_names: vec![],
            }),
        ];

        let mut graph_data = GraphData::new(8);
        graph_data.add_commits(&commits);

        if let Err(error) = verify_all_invariants(&graph_data, &commits) {
            panic!("Graph invariant violation for linear commits:\n{}", error);
        }
    }

    #[test]
    fn test_git_graph_random_commits() {
        for seed in 0..100 {
            let mut rng = StdRng::seed_from_u64(seed);

            let adversarial = rng.random_bool(0.2);
            let num_commits = if adversarial {
                rng.random_range(10..100)
            } else {
                rng.random_range(5..50)
            };

            let commits = generate_random_commit_dag(&mut rng, num_commits, adversarial);

            assert_eq!(
                num_commits,
                commits.len(),
                "seed={}: Generate random commit dag didn't generate the correct amount of commits",
                seed
            );

            let mut graph_data = GraphData::new(8);
            graph_data.add_commits(&commits);

            if let Err(error) = verify_all_invariants(&graph_data, &commits) {
                panic!(
                    "Graph invariant violation (seed={}, adversarial={}, num_commits={}):\n{:#}",
                    seed, adversarial, num_commits, error
                );
            }
        }
    }

    // The full integration test has less iterations because it's significantly slower
    // than the random commit test
    #[gpui::test(iterations = 10)]
    async fn test_git_graph_random_integration(mut rng: StdRng, cx: &mut TestAppContext) {
        init_test(cx);

        let adversarial = rng.random_bool(0.2);
        let num_commits = if adversarial {
            rng.random_range(10..100)
        } else {
            rng.random_range(5..50)
        };

        let commits = generate_random_commit_dag(&mut rng, num_commits, adversarial);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;

        fs.set_graph_commits(Path::new("/project/.git"), commits.clone());

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });

        repository.update(cx, |repo, cx| {
            repo.graph_data(LogSource::default(), LogOrder::default(), 0..usize::MAX, cx);
        });
        cx.run_until_parked();

        let graph_commits: Vec<Arc<InitialGraphCommitData>> = repository.update(cx, |repo, cx| {
            repo.graph_data(LogSource::default(), LogOrder::default(), 0..usize::MAX, cx)
                .commits
                .to_vec()
        });

        let mut graph_data = GraphData::new(8);
        graph_data.add_commits(&graph_commits);

        if let Err(error) = verify_all_invariants(&graph_data, &commits) {
            panic!(
                "Graph invariant violation (adversarial={}, num_commits={}):\n{:#}",
                adversarial, num_commits, error
            );
        }
    }

    #[gpui::test]
    async fn test_empty_nested_repository_graph_stops_loading(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({
                "repo_a": {
                    ".git": {},
                    "file_a.txt": "content",
                },
                "repo_b": {
                    ".git": {},
                    "file_b.txt": "content",
                },
            }),
        )
        .await;

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        project
            .update(cx, |project, cx| project.git_scans_complete(cx))
            .await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            assert_eq!(project.repositories(cx).len(), 2);
            project
                .active_repository(cx)
                .expect("should have an active repository")
        });

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = multi_workspace.read_with(&*cx, |multi, _| multi.workspace().downgrade());
        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace,
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();

        let (commit_count, is_loading) = git_graph.update(cx, |graph, cx| {
            graph.commit_count_and_loading_state_for_test(cx)
        });

        assert_eq!(commit_count, 0);
        assert!(!is_loading, "empty graph data should stop loading");
    }

    #[gpui::test]
    async fn test_initial_graph_data_not_cleared_on_initial_loading(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;

        let mut rng = StdRng::seed_from_u64(42);
        let commits = generate_random_commit_dag(&mut rng, 10, false);
        fs.set_graph_commits(Path::new("/project/.git"), commits.clone());

        let project = Project::test(fs.clone(), [], cx).await;
        let observed_repository_events = Arc::new(Mutex::new(Vec::new()));
        project.update(cx, |project, cx| {
            let observed_repository_events = observed_repository_events.clone();
            cx.subscribe(project.git_store(), move |_, _, event, _| {
                if let GitStoreEvent::RepositoryUpdated(_, repository_event, true) = event {
                    observed_repository_events
                        .lock()
                        .expect("repository event mutex should be available")
                        .push(repository_event.clone());
                }
            })
            .detach();
        });
        project
            .update(cx, |project, cx| {
                project.create_worktree("/project", true, cx)
            })
            .await
            .unwrap();
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });

        repository.update(cx, |repo, cx| {
            repo.graph_data(LogSource::default(), LogOrder::default(), 0..usize::MAX, cx);
        });

        project
            .update(cx, |project, cx| project.git_scans_complete(cx))
            .await;
        cx.run_until_parked();

        let observed_repository_events = observed_repository_events
            .lock()
            .expect("repository event mutex should be available");
        assert!(
            observed_repository_events
                .iter()
                .any(|event| matches!(event, RepositoryEvent::HeadChanged)),
            "initial repository scan should emit HeadChanged"
        );
        let commit_count_after = repository.read_with(cx, |repo, _| {
            repo.get_graph_data(LogSource::default(), LogOrder::default())
                .map(|data| data.commit_data.len())
                .unwrap()
        });
        assert_eq!(
            commits.len(),
            commit_count_after,
            "initial_graph_data should remain populated after events emitted by initial repository scan"
        );
    }

    #[gpui::test]
    async fn test_initial_graph_data_propagates_error(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;

        fs.set_graph_error(
            Path::new("/project/.git"),
            Some("fatal: bad default revision 'HEAD'".to_string()),
        );

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });

        repository.update(cx, |repo, cx| {
            repo.graph_data(LogSource::default(), LogOrder::default(), 0..usize::MAX, cx);
        });

        cx.run_until_parked();

        let error = repository.read_with(cx, |repo, _| {
            repo.get_graph_data(LogSource::default(), LogOrder::default())
                .and_then(|data| data.error.clone())
        });

        assert!(
            error.is_some(),
            "graph data should contain an error after initial_graph_data fails"
        );
        let error_message = error.unwrap();
        assert!(
            error_message.contains("bad default revision"),
            "error should contain the git error message, got: {}",
            error_message
        );
    }

    #[gpui::test]
    async fn test_graph_data_repopulated_from_cache_after_repo_switch(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project_a"),
            json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;
        fs.insert_tree(
            Path::new("/project_b"),
            json!({
                ".git": {},
                "other.txt": "content",
            }),
        )
        .await;

        let mut rng = StdRng::seed_from_u64(42);
        let commits = generate_random_commit_dag(&mut rng, 10, false);
        fs.set_graph_commits(Path::new("/project_a/.git"), commits.clone());

        let project = Project::test(
            fs.clone(),
            [Path::new("/project_a"), Path::new("/project_b")],
            cx,
        )
        .await;
        cx.run_until_parked();

        let (first_repository, second_repository) = project.read_with(cx, |project, cx| {
            let mut first_repository = None;
            let mut second_repository = None;

            for repository in project.repositories(cx).values() {
                let work_directory_abs_path = &repository.read(cx).work_directory_abs_path;
                if work_directory_abs_path.as_ref() == Path::new("/project_a") {
                    first_repository = Some(repository.clone());
                } else if work_directory_abs_path.as_ref() == Path::new("/project_b") {
                    second_repository = Some(repository.clone());
                }
            }

            (
                first_repository.expect("should have repository for /project_a"),
                second_repository.expect("should have repository for /project_b"),
            )
        });
        first_repository.update(cx, |repository, cx| repository.set_as_active_repository(cx));
        cx.run_until_parked();

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });

        let workspace_weak =
            multi_workspace.read_with(&*cx, |multi, _| multi.workspace().downgrade());
        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                first_repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace_weak,
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();

        // Verify initial graph data is loaded
        let initial_commit_count =
            git_graph.read_with(&*cx, |graph, _| graph.graph_data.commits.len());
        assert!(
            initial_commit_count > 0,
            "graph data should have been loaded, got 0 commits"
        );

        git_graph.update(cx, |graph, cx| {
            graph.set_repo_id(second_repository.read(cx).id, cx)
        });
        cx.run_until_parked();

        let commit_count_after_clear =
            git_graph.read_with(&*cx, |graph, _| graph.graph_data.commits.len());
        assert_eq!(
            commit_count_after_clear, 0,
            "graph_data should be cleared after switching away"
        );

        git_graph.update(cx, |graph, cx| {
            graph.set_repo_id(first_repository.read(cx).id, cx)
        });
        cx.run_until_parked();

        cx.draw(
            point(px(0.), px(0.)),
            gpui::size(px(1200.), px(800.)),
            |_, _| git_graph.clone().into_any_element(),
        );
        cx.run_until_parked();

        // Verify graph data is reloaded from repository cache on switch back
        let reloaded_commit_count =
            git_graph.read_with(&*cx, |graph, _| graph.graph_data.commits.len());
        assert_eq!(
            reloaded_commit_count,
            commits.len(),
            "graph data should be reloaded after switching back"
        );
    }

    #[gpui::test]
    async fn test_file_history_action_uses_git_panel_and_editor_sources(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new(util::path!("/project")),
            json!({
                ".git": {},
                "tracked1.txt": "tracked 1",
                "tracked2.txt": "tracked 2",
            }),
        )
        .await;
        fs.set_status_for_repo(
            Path::new(util::path!("/project/.git")),
            &[
                ("tracked1.txt", StatusCode::Modified.worktree()),
                ("tracked2.txt", StatusCode::Modified.worktree()),
            ],
        );

        let commits = vec![Arc::new(InitialGraphCommitData {
            sha: Oid::from_bytes(&[1; 20]).unwrap(),
            parents: smallvec![],
            ref_names: vec!["HEAD".into(), "refs/heads/main".into()],
        })];
        fs.set_graph_commits(Path::new(util::path!("/project/.git")), commits);

        let project = Project::test(fs.clone(), [Path::new(util::path!("/project"))], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have active repository")
        });
        let tracked1_repo_path = RepoPath::new(&"tracked1.txt").unwrap();
        let tracked2_repo_path = RepoPath::new(&"tracked2.txt").unwrap();
        let tracked1 = repository
            .read_with(cx, |repository, cx| {
                repository.repo_path_to_project_path(&tracked1_repo_path, cx)
            })
            .expect("tracked1 should resolve to project path");
        let tracked2 = repository
            .read_with(cx, |repository, cx| {
                repository.repo_path_to_project_path(&tracked2_repo_path, cx)
            })
            .expect("tracked2 should resolve to project path");

        let workspace_window = cx.add_window(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = workspace_window
            .read_with(cx, |multi, _| multi.workspace().clone())
            .expect("workspace should exist");

        let (weak_workspace, async_window_cx) = workspace_window
            .update(cx, |multi, window, cx| {
                (multi.workspace().downgrade(), window.to_async(cx))
            })
            .expect("window should be available");
        cx.background_executor.allow_parking();
        let git_panel = cx
            .foreground_executor()
            .clone()
            .block_test(crate::git_panel::GitPanel::load(
                weak_workspace,
                async_window_cx,
            ))
            .expect("git panel should load");
        cx.background_executor.forbid_parking();

        workspace_window
            .update(cx, |multi, window, cx| {
                let workspace = multi.workspace();
                workspace.update(cx, |workspace, cx| {
                    workspace.add_panel(git_panel.clone(), window, cx);
                });
            })
            .expect("workspace window should be available");
        cx.executor().advance_clock(Duration::from_millis(100));
        cx.run_until_parked();

        workspace_window
            .update(cx, |_, window, cx| {
                git_panel.update(cx, |panel, cx| {
                    panel.select_entry_by_path(tracked1.clone(), window, cx);
                });
                git_panel.update(cx, |panel, cx| {
                    panel.focus_handle(cx).focus(window, cx);
                });
            })
            .expect("workspace window should be available");
        cx.run_until_parked();
        workspace_window
            .update(cx, |_, window, cx| {
                window.dispatch_action(Box::new(git::FileHistory), cx);
            })
            .expect("workspace window should be available");
        cx.run_until_parked();

        workspace.read_with(cx, |workspace, cx| {
            let graphs = workspace.items_of_type::<GitGraph>(cx).collect::<Vec<_>>();
            assert_eq!(graphs.len(), 1);
            assert_eq!(
                graphs[0].read(cx).log_source,
                LogSource::Path(tracked1_repo_path.clone())
            );
        });

        workspace_window
            .update(cx, |_, window, cx| {
                git_panel.update(cx, |panel, cx| {
                    panel.select_entry_by_path(tracked1.clone(), window, cx);
                });
                git_panel.update(cx, |panel, cx| {
                    panel.focus_handle(cx).focus(window, cx);
                });
            })
            .expect("workspace window should be available");
        cx.run_until_parked();
        workspace_window
            .update(cx, |_, window, cx| {
                window.dispatch_action(Box::new(git::FileHistory), cx);
            })
            .expect("workspace window should be available");
        cx.run_until_parked();

        workspace.read_with(cx, |workspace, cx| {
            let graphs = workspace.items_of_type::<GitGraph>(cx).collect::<Vec<_>>();
            assert_eq!(graphs.len(), 1);
            assert_eq!(
                graphs[0].read(cx).log_source,
                LogSource::Path(tracked1_repo_path.clone())
            );
        });

        let tracked1_buffer = project
            .update(cx, |project, cx| project.open_buffer(tracked1.clone(), cx))
            .await
            .expect("tracked1 buffer should open");
        let tracked2_buffer = project
            .update(cx, |project, cx| project.open_buffer(tracked2.clone(), cx))
            .await
            .expect("tracked2 buffer should open");
        workspace_window
            .update(cx, |multi, window, cx| {
                let workspace = multi.workspace();
                let multibuffer = cx.new(|cx| {
                    let mut multibuffer = editor::MultiBuffer::new(language::Capability::ReadWrite);
                    multibuffer.set_excerpts_for_buffer(
                        tracked1_buffer.clone(),
                        [Default::default()..tracked1_buffer.read(cx).max_point()],
                        0,
                        cx,
                    );
                    multibuffer.set_excerpts_for_buffer(
                        tracked2_buffer.clone(),
                        [Default::default()..tracked2_buffer.read(cx).max_point()],
                        0,
                        cx,
                    );
                    multibuffer
                });
                let editor = cx.new(|cx| {
                    Editor::for_multibuffer(multibuffer, Some(project.clone()), window, cx)
                });
                workspace.update(cx, |workspace, cx| {
                    workspace.add_item_to_active_pane(
                        Box::new(editor.clone()),
                        None,
                        true,
                        window,
                        cx,
                    );
                });
                editor.update(cx, |editor, cx| {
                    let snapshot = editor.buffer().read(cx).snapshot(cx);
                    let second_excerpt_point = snapshot
                        .range_for_buffer(tracked2_buffer.read(cx).remote_id())
                        .expect("tracked2 excerpt should exist")
                        .start;
                    let anchor = snapshot.anchor_before(second_excerpt_point);
                    editor.change_selections(
                        editor::SelectionEffects::no_scroll(),
                        window,
                        cx,
                        |selections| {
                            selections.select_anchor_ranges([anchor..anchor]);
                        },
                    );
                    window.focus(&editor.focus_handle(cx), cx);
                });
            })
            .expect("workspace window should be available");
        cx.run_until_parked();

        workspace_window
            .update(cx, |_, window, cx| {
                window.dispatch_action(Box::new(git::FileHistory), cx);
            })
            .expect("workspace window should be available");
        cx.run_until_parked();

        workspace.read_with(cx, |workspace, cx| {
            let graphs = workspace.items_of_type::<GitGraph>(cx).collect::<Vec<_>>();
            assert_eq!(graphs.len(), 2);
            let latest = graphs
                .into_iter()
                .max_by_key(|graph| graph.entity_id())
                .expect("expected a git graph");
            assert_eq!(
                latest.read(cx).log_source,
                LogSource::Path(tracked2_repo_path)
            );
        });
    }

    #[gpui::test]
    async fn test_file_history_action_resolves_through_project_diff(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new(util::path!("/project")),
            json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;
        fs.set_head_and_index_for_repo(
            Path::new(util::path!("/project/.git")),
            &[("file.txt", "tracked".to_owned())],
        );

        let project = Project::test(fs.clone(), [Path::new(util::path!("/project"))], cx).await;
        cx.run_until_parked();

        let tracked_repo_path = RepoPath::new(&"file.txt").unwrap();

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = multi_workspace.read_with(cx, |multi, _| multi.workspace().clone());

        // A project diff is a `ProjectDiff` item wrapping a `SplittableEditor`, not
        // an `Editor` itself, so resolving the file-history target must go through
        // `act_as` rather than a direct downcast of the active item.
        cx.update(|window, cx| {
            window.dispatch_action(Box::new(crate::project_diff::Diff), cx);
        });
        cx.run_until_parked();

        workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_item_as::<crate::ProjectDiff>(cx)
                .expect("project diff should be the active item");
        });

        cx.update(|window, cx| {
            window.dispatch_action(Box::new(git::FileHistory), cx);
        });
        cx.run_until_parked();

        workspace.read_with(cx, |workspace, cx| {
            let graphs = workspace.items_of_type::<GitGraph>(cx).collect::<Vec<_>>();
            assert_eq!(
                graphs.len(),
                1,
                "dispatching FileHistory from a project diff should open a git graph"
            );
            assert_eq!(
                graphs[0].read(cx).log_source,
                LogSource::Path(tracked_repo_path)
            );
        });
    }

    #[gpui::test]
    fn test_serialized_state_roundtrip(_cx: &mut TestAppContext) {
        use persistence::SerializedGitGraphState;

        let path = RepoPath::new(&"src/main.rs").unwrap();
        let sha = Oid::from_bytes(&[0xab; 20]).unwrap();

        let state = SerializedGitGraphState {
            log_source_type: Some(persistence::LOG_SOURCE_PATH),
            log_source_value: Some("src/main.rs".to_string()),
            log_order: Some(persistence::LOG_ORDER_TOPO),
            selected_sha: Some(sha.to_string()),
            search_query: Some("fix bug".to_string()),
            search_case_sensitive: Some(true),
            hidden_columns: None,
        };

        assert_eq!(
            persistence::deserialize_log_source(&state),
            LogSource::Path(path)
        );
        assert!(matches!(
            persistence::deserialize_log_order(&state),
            LogOrder::TopoOrder
        ));
        assert_eq!(
            state.selected_sha.as_deref(),
            Some(sha.to_string()).as_deref()
        );
        assert_eq!(state.search_query.as_deref(), Some("fix bug"));
        assert_eq!(state.search_case_sensitive, Some(true));

        let all_state = SerializedGitGraphState {
            log_source_type: Some(persistence::LOG_SOURCE_ALL),
            log_source_value: None,
            log_order: Some(persistence::LOG_ORDER_DATE),
            selected_sha: None,
            search_query: None,
            search_case_sensitive: None,
            hidden_columns: None,
        };
        assert_eq!(
            persistence::deserialize_log_source(&all_state),
            LogSource::All
        );
        assert!(matches!(
            persistence::deserialize_log_order(&all_state),
            LogOrder::DateOrder
        ));

        let branch_state = SerializedGitGraphState {
            log_source_type: Some(persistence::LOG_SOURCE_BRANCH),
            log_source_value: Some("refs/heads/main".to_string()),
            ..Default::default()
        };
        assert_eq!(
            persistence::deserialize_log_source(&branch_state),
            LogSource::Branch("refs/heads/main".into())
        );

        let sha_state = SerializedGitGraphState {
            log_source_type: Some(persistence::LOG_SOURCE_SHA),
            log_source_value: Some(sha.to_string()),
            ..Default::default()
        };
        assert_eq!(
            persistence::deserialize_log_source(&sha_state),
            LogSource::Sha(sha)
        );

        let empty_state = SerializedGitGraphState::default();
        assert_eq!(
            persistence::deserialize_log_source(&empty_state),
            LogSource::All
        );
        assert!(matches!(
            persistence::deserialize_log_order(&empty_state),
            LogOrder::DateOrder
        ));
    }

    #[gpui::test]
    fn test_hidden_columns_bitmask_roundtrip(_cx: &mut TestAppContext) {
        let mask = [false, true, false, true, false];
        let bits = persistence::serialize_hidden_columns(&mask);
        assert_eq!(
            persistence::deserialize_hidden_columns(bits, mask.len()),
            mask.to_vec()
        );

        assert_eq!(persistence::serialize_hidden_columns(&[false; 5]), 0);
        assert_eq!(
            persistence::deserialize_hidden_columns(0, 4),
            vec![false; 4]
        );

        // A mask saved with more columns than we restore with is truncated safely, and one
        // saved with fewer columns defaults the extra columns to visible.
        let bits = persistence::serialize_hidden_columns(&[true, false, true, false, true]);
        assert_eq!(
            persistence::deserialize_hidden_columns(bits, 4),
            vec![true, false, true, false]
        );
        assert_eq!(
            persistence::deserialize_hidden_columns(bits, 6),
            vec![true, false, true, false, true, false]
        );
    }

    #[gpui::test]
    async fn test_git_graph_state_persists_across_serialization_roundtrip(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;

        let mut rng = StdRng::seed_from_u64(99);
        let commits = generate_random_commit_dag(&mut rng, 20, false);
        fs.set_graph_commits(Path::new("/project/.git"), commits.clone());

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace_weak =
            multi_workspace.read_with(&*cx, |multi, _| multi.workspace().downgrade());

        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace_weak.clone(),
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();

        cx.draw(
            point(px(0.), px(0.)),
            gpui::size(px(1200.), px(800.)),
            |_, _| git_graph.clone().into_any_element(),
        );
        cx.run_until_parked();

        let commit_count = git_graph.read_with(&*cx, |graph, _| graph.graph_data.commits.len());
        assert!(commit_count > 0, "graph should have loaded commits, got 0");

        let target_sha = commits[5].sha;
        git_graph.update(cx, |graph, _| {
            graph.selected_entry_idx = Some(5);
        });

        let selected_sha = git_graph.read_with(&*cx, |graph, _| {
            graph
                .selected_entry_idx
                .and_then(|idx| graph.graph_data.commits.get(idx))
                .map(|c| c.data.sha.to_string())
        });
        assert_eq!(selected_sha, Some(target_sha.to_string()));

        let item_id = workspace::ItemId::from(999_u64);
        let workspace_db = cx.read(|cx| workspace::WorkspaceDb::global(cx));
        let workspace_id = workspace_db
            .next_id()
            .await
            .expect("should create workspace id");
        let db = cx.read(|cx| persistence::GitGraphsDb::global(cx));
        // Hide the "Date" column (index 2 in the non-path-history layout).
        let hidden_columns =
            persistence::serialize_hidden_columns(&[false, false, true, false, false]);
        db.save_git_graph(
            item_id,
            workspace_id,
            "/project".to_string(),
            Some(persistence::LOG_SOURCE_ALL),
            None,
            Some(persistence::LOG_ORDER_DATE),
            selected_sha.clone(),
            Some("some query".to_string()),
            Some(true),
            Some(hidden_columns),
        )
        .await
        .expect("save should succeed");

        let restored_graph = cx
            .update(|window, cx| {
                <GitGraph as workspace::SerializableItem>::deserialize(
                    project.clone(),
                    workspace_weak,
                    workspace_id,
                    item_id,
                    window,
                    cx,
                )
            })
            .await
            .expect("deserialization should succeed");
        cx.run_until_parked();

        cx.draw(
            point(px(0.), px(0.)),
            gpui::size(px(1200.), px(800.)),
            |_, _| restored_graph.clone().into_any_element(),
        );
        cx.run_until_parked();

        let restored_commit_count =
            restored_graph.read_with(&*cx, |graph, _| graph.graph_data.commits.len());
        assert_eq!(
            restored_commit_count, commit_count,
            "restored graph should have the same number of commits"
        );

        restored_graph.read_with(&*cx, |graph, _| {
            assert_eq!(
                graph.log_source,
                LogSource::All,
                "log_source should be restored"
            );

            let restored_selected_sha = graph
                .selected_entry_idx
                .and_then(|idx| graph.graph_data.commits.get(idx))
                .map(|c| c.data.sha.to_string());
            assert_eq!(
                restored_selected_sha, selected_sha,
                "selected commit should be restored via pending_select_sha"
            );

            assert_eq!(
                graph.search_state.case_sensitive, true,
                "search case sensitivity should be restored"
            );

            assert_eq!(
                graph.column_visibility.as_slice(),
                &[false, false, true, false, false],
                "hidden columns should be restored"
            );
        });

        restored_graph.read_with(&*cx, |graph, cx| {
            let editor_text = graph.search_state.editor.read(cx).text(cx);
            assert_eq!(
                editor_text, "some query",
                "search query text should be restored in editor"
            );
        });
    }

    #[gpui::test]
    async fn test_git_graph_search_matches_commit_hash_prefix(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;

        let first_sha = Oid::from_bytes(&[1; 20]).unwrap();
        let target_sha = Oid::from_bytes(&[2; 20]).unwrap();
        let third_sha = Oid::from_bytes(&[3; 20]).unwrap();
        let commits = vec![
            Arc::new(InitialGraphCommitData {
                sha: first_sha,
                parents: smallvec![target_sha],
                ref_names: vec!["HEAD".into(), "refs/heads/main".into()],
            }),
            Arc::new(InitialGraphCommitData {
                sha: target_sha,
                parents: smallvec![third_sha],
                ref_names: vec![],
            }),
            Arc::new(InitialGraphCommitData {
                sha: third_sha,
                parents: smallvec![],
                ref_names: vec![],
            }),
        ];
        fs.set_graph_commits(Path::new("/project/.git"), commits);
        fs.set_commit_data(
            Path::new("/project/.git"),
            [
                (
                    CommitData {
                        sha: first_sha,
                        parents: smallvec![target_sha],
                        author_name: "Author".into(),
                        author_email: "author@example.com".into(),
                        commit_timestamp: 1,
                        subject: "Add feature".into(),
                        message: "Add feature".into(),
                    },
                    false,
                ),
                (
                    CommitData {
                        sha: target_sha,
                        parents: smallvec![third_sha],
                        author_name: "Author".into(),
                        author_email: "author@example.com".into(),
                        commit_timestamp: 2,
                        subject: "Fix branch loading".into(),
                        message: "Fix branch loading".into(),
                    },
                    false,
                ),
                (
                    CommitData {
                        sha: third_sha,
                        parents: smallvec![],
                        author_name: "山田 太郎".into(),
                        author_email: "yamada@example.com".into(),
                        commit_timestamp: 3,
                        subject: "Update docs".into(),
                        message: "Update docs".into(),
                    },
                    false,
                ),
            ],
        );

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });
        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace_weak =
            multi_workspace.read_with(&*cx, |multi, _| multi.workspace().downgrade());
        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace_weak,
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();

        git_graph.update(cx, |graph, cx| {
            graph.search_for_test("0202020".into(), cx);
        });
        cx.run_until_parked();

        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(graph.search_matches_for_test(), vec![target_sha]);
            let selected_sha = graph
                .selected_entry_idx
                .and_then(|idx| graph.graph_data.commits.get(idx))
                .map(|commit| commit.data.sha);
            assert_eq!(selected_sha, Some(target_sha));
        });

        git_graph.update(cx, |graph, cx| {
            graph.search_for_test("docs".into(), cx);
        });
        cx.run_until_parked();

        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(graph.search_matches_for_test(), vec![third_sha]);
        });

        // The `著者:` prefix is a filter rather than text to look for in the message.
        for (query, expected) in [
            ("著者:山田", vec![third_sha]),
            ("著者:\"山田 太郎\"", vec![third_sha]),
            ("author:yamada@example.com", vec![third_sha]),
            ("著者:山田 docs", vec![third_sha]),
            ("著者:山田 feature", vec![]),
            ("author:Author feature", vec![first_sha]),
            ("山田", vec![]),
            // The author narrows a hash prefix search too, rather than either one winning.
            ("著者:山田 0303030", vec![third_sha]),
            ("author:Author 0303030", vec![]),
            ("author：山田", vec![third_sha]),
            ("著者:\"未閉じ", vec![]),
            ("著者:山田太郎", vec![]),
            // A prefix with no value after it is message text, not a filter matching everyone.
            ("著者:", vec![]),
            ("author::", vec![]),
            ("   ", vec![]),
        ] {
            git_graph.update(cx, |graph, cx| {
                graph.search_for_test(query.into(), cx);
            });
            cx.run_until_parked();
            git_graph.read_with(&*cx, |graph, _| {
                assert_eq!(
                    graph.search_matches_for_test(),
                    expected,
                    "unexpected matches for {query:?}"
                );
            });
        }
    }

    fn sorted_search_matches(graph: &GitGraph) -> Vec<Oid> {
        let mut matches = graph.search_matches_for_test();
        matches.sort_by_key(|oid| oid.to_string());
        matches
    }

    fn sorted_oids(mut oids: Vec<Oid>) -> Vec<Oid> {
        oids.sort_by_key(|oid| oid.to_string());
        oids
    }

    /// Three commits with the data the fake repository searches, newest first: a merge by
    /// `Author` on 2026-03-01, `Author`'s "Fix branch loading" on 2026-02-01 and 山田 太郎's
    /// "Update docs" on 2026-01-01.
    fn set_advanced_search_commit_data(fs: &FakeFs, shas: [Oid; 3]) {
        let noon = |month: time::Month| {
            time::PrimitiveDateTime::new(
                time::Date::from_calendar_date(2026, month, 1).unwrap(),
                time::Time::from_hms(12, 0, 0).unwrap(),
            )
            .assume_utc()
            .unix_timestamp()
        };
        let [merge, fix, docs] = shas;
        let commit = |sha: Oid,
                      parents: Vec<Oid>,
                      name: &str,
                      email: &str,
                      month: time::Month,
                      message: &str| {
            (
                CommitData {
                    sha,
                    parents: parents.into_iter().collect(),
                    author_name: SharedString::from(name.to_string()),
                    author_email: SharedString::from(email.to_string()),
                    commit_timestamp: noon(month),
                    subject: SharedString::from(message.to_string()),
                    message: SharedString::from(message.to_string()),
                },
                false,
            )
        };
        fs.set_commit_data(
            Path::new("/project/.git"),
            [
                commit(
                    merge,
                    vec![fix, docs],
                    "Author",
                    "author@example.com",
                    time::Month::March,
                    "Merge feature",
                ),
                commit(
                    fix,
                    vec![docs],
                    "Author",
                    "author@example.com",
                    time::Month::February,
                    "Fix branch loading",
                ),
                commit(
                    docs,
                    Vec::new(),
                    "山田 太郎",
                    "yamada@example.com",
                    time::Month::January,
                    "Update docs",
                ),
            ],
        );
    }

    #[gpui::test]
    async fn test_git_graph_search_conditions(cx: &mut TestAppContext) {
        let commits = generate_random_commit_dag(&mut StdRng::seed_from_u64(21), 3, false);
        let shas = [commits[0].sha, commits[1].sha, commits[2].sha];
        let [merge, fix, docs] = shas;
        let (fs, _repository, _multi_workspace, git_graph, cx) = open_graph(commits, cx).await;
        set_advanced_search_commit_data(&fs, shas);

        for (query, expected) in [
            ("以降:2026-01-15", vec![merge, fix]),
            ("after:2026-01-15 before:2026-02-15", vec![fix]),
            ("以前:2026-01-15 著者:山田", vec![docs]),
            ("以前:2026-01-15 著者:Author", vec![]),
            ("マージ:だけ", vec![merge]),
            ("merge:no author:Author", vec![fix]),
            ("author:Author author:example.com", vec![merge, fix]),
            ("author:Author author:山田", vec![]),
            (&*format!("sha:{}", &fix.to_string()[..5]), vec![fix]),
            ("Fix マージ:いいえ", vec![fix]),
            // Unknown prefixes stay message text.
            ("branch: loading", vec![]),
        ] {
            git_graph.update(cx, |graph, cx| graph.search_for_test(query.into(), cx));
            cx.run_until_parked();
            git_graph.read_with(&*cx, |graph, _| {
                assert_eq!(
                    sorted_search_matches(graph),
                    sorted_oids(expected),
                    "unexpected matches for {query:?}"
                );
                assert!(
                    graph.search_state.outcome.problems.is_empty(),
                    "{query:?}: {:?}",
                    graph.search_state.outcome.problems
                );
            });
        }

        // A condition that cannot be read stops the search, rather than being dropped and letting
        // `Fix` alone find a commit.
        for (query, reason) in [
            (
                "Fix 以降:きのう",
                "Cannot read the date in \"since:きのう\".",
            ),
            ("Fix ブランチ:nope", "There is no branch \"nope\"."),
            ("Fix マージ:たぶん", "Cannot read \"merge:たぶん\"."),
        ] {
            git_graph.update(cx, |graph, cx| graph.search_for_test(query.into(), cx));
            cx.run_until_parked();
            git_graph.read_with(&*cx, |graph, _| {
                assert!(
                    graph.search_matches_for_test().is_empty(),
                    "{query:?} must not search"
                );
                assert!(matches!(graph.search_state.state, QueryState::Empty));
                assert!(
                    graph
                        .search_state
                        .outcome
                        .problems
                        .iter()
                        .any(|problem| problem.starts_with(reason)),
                    "{query:?}: {:?}",
                    graph.search_state.outcome.problems
                );
            });
        }

        // The backend's own refusal reaches the line under the search box too.
        git_graph.update(cx, |graph, cx| {
            graph.search_for_test("変更:needle".into(), cx)
        });
        cx.run_until_parked();
        git_graph.read_with(&*cx, |graph, _| {
            let outcome = &graph.search_state.outcome;
            assert!(outcome.finished);
            assert_eq!(outcome.problems.len(), 1, "{:?}", outcome.problems);
            assert_eq!(graph_search::search_count_text(outcome, 0), None);
        });

        git_graph.update(cx, |graph, cx| graph.search_for_test("Fix".into(), cx));
        cx.run_until_parked();
        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(
                graph_search::search_count_text(&graph.search_state.outcome, 1).as_deref(),
                Some("1 commit")
            );
        });
        git_graph.update(cx, |graph, cx| graph.search_for_test("nothing".into(), cx));
        cx.run_until_parked();
        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(
                graph_search::search_count_text(&graph.search_state.outcome, 0).as_deref(),
                Some("No commits match. Try fewer conditions.")
            );
        });

        // 中止 drops the search before it gets to report anything.
        git_graph.update(cx, |graph, cx| {
            graph.search_for_test("Fix".into(), cx);
            assert_eq!(
                graph_search::search_count_text(&graph.search_state.outcome, 0).as_deref(),
                Some("Searching… 0 commits")
            );
            graph.stop_search(cx);
        });
        cx.run_until_parked();
        git_graph.read_with(&*cx, |graph, _| {
            assert!(graph.search_matches_for_test().is_empty());
            assert_eq!(
                graph_search::search_count_text(&graph.search_state.outcome, 0).as_deref(),
                Some("Stopped at 0 commits.")
            );
        });

        // The graph changing (HEAD moved, a commit made) does not start a stopped search again;
        // only confirming the box does.
        git_graph.update(cx, |graph, cx| graph.invalidate_state(cx));
        cx.run_until_parked();
        git_graph.read_with(&*cx, |graph, _| {
            assert!(!matches!(graph.search_state.state, QueryState::Pending(_)));
            assert!(graph.search_state.outcome.stopped);
            assert!(graph.search_matches_for_test().is_empty());
        });
        git_graph.update(cx, |graph, cx| graph.search_for_test("Fix".into(), cx));
        cx.run_until_parked();
        git_graph.read_with(&*cx, |graph, _| {
            assert!(!graph.search_state.outcome.stopped);
            assert_eq!(graph.search_matches_for_test(), [fix]);
        });
    }

    #[gpui::test]
    async fn test_git_graph_search_chip_removes_its_condition(cx: &mut TestAppContext) {
        let commits = generate_random_commit_dag(&mut StdRng::seed_from_u64(22), 3, false);
        let shas = [commits[0].sha, commits[1].sha, commits[2].sha];
        let fix = shas[1];
        let (fs, _repository, _multi_workspace, git_graph, cx) = open_graph(commits, cx).await;
        set_advanced_search_commit_data(&fs, shas);

        git_graph.update_in(cx, |graph, window, cx| {
            graph.search_state.editor.update(cx, |editor, cx| {
                editor.set_text("著者:山田 Fix マージ:いいえ", window, cx)
            });
            graph.run_search_from_box(cx);
        });
        cx.run_until_parked();
        git_graph.read_with(&*cx, |graph, _| {
            assert!(graph.search_matches_for_test().is_empty());
            assert_eq!(
                graph
                    .search_state
                    .outcome
                    .conditions
                    .iter()
                    .map(|condition| condition.kind)
                    .collect::<Vec<_>>(),
                [
                    CommitSearchConditionKind::Author,
                    CommitSearchConditionKind::Merge
                ]
            );
        });

        git_graph.update_in(cx, |graph, window, cx| {
            graph.remove_search_condition(0, window, cx)
        });
        cx.run_until_parked();
        git_graph.read_with(&*cx, |graph, cx| {
            assert_eq!(
                graph.search_state.editor.read(cx).text(cx),
                "Fix マージ:いいえ"
            );
            assert_eq!(graph.search_matches_for_test(), [fix]);
        });

        git_graph.update_in(cx, |graph, window, cx| {
            graph.remove_search_condition(0, window, cx)
        });
        cx.run_until_parked();
        git_graph.read_with(&*cx, |graph, cx| {
            assert_eq!(graph.search_state.editor.read(cx).text(cx), "Fix");
            assert_eq!(graph.search_matches_for_test(), [fix]);
            assert!(graph.search_state.outcome.conditions.is_empty());
        });

        // Words typed after the search ran stay when a chip's × takes its condition out.
        git_graph.update_in(cx, |graph, window, cx| {
            graph.search_state.editor.update(cx, |editor, cx| {
                editor.set_text("マージ:いいえ Fix", window, cx)
            });
            graph.run_search_from_box(cx);
        });
        cx.run_until_parked();
        git_graph.update_in(cx, |graph, window, cx| {
            graph.search_state.editor.update(cx, |editor, cx| {
                editor.set_text("マージ:いいえ Fix branch", window, cx)
            });
            graph.remove_search_condition(0, window, cx)
        });
        cx.run_until_parked();
        git_graph.read_with(&*cx, |graph, cx| {
            assert_eq!(graph.search_state.editor.read(cx).text(cx), "Fix branch");
            assert_eq!(graph.search_matches_for_test(), [fix]);
        });
    }

    #[gpui::test]
    async fn test_git_graph_search_form_writes_into_the_search_box(cx: &mut TestAppContext) {
        use graph_search::GraphSearchForm;

        let commits = generate_random_commit_dag(&mut StdRng::seed_from_u64(23), 3, false);
        let (_fs, repository, _multi_workspace, git_graph, cx) = open_graph(commits, cx).await;
        repository.update(cx, |repository, cx| {
            repository.set_branch_list_for_test(
                [
                    "refs/heads/main",
                    "refs/heads/feature/login",
                    "refs/remotes/origin/main",
                ]
                .into_iter()
                .map(|ref_name| Branch {
                    is_head: false,
                    ref_name: ref_name.into(),
                    upstream: None,
                    most_recent_commit: None,
                })
                .collect(),
                cx,
            )
        });
        git_graph.update_in(cx, |graph, window, cx| {
            graph.tags = Some(Arc::from(
                ["v1.0", "v1.1", "release"]
                    .into_iter()
                    .map(|name| TagInfo {
                        name: name.to_string(),
                        commit_sha: Oid::from_bytes(&[1; 20]).unwrap().to_string(),
                        created_at_unix: None,
                        subject: None,
                        commit_subject: None,
                        annotated: false,
                    })
                    .collect::<Vec<_>>(),
            ));
            graph.search_state.editor.update(cx, |editor, cx| {
                editor.set_text("直した tag:v1", window, cx)
            });
        });

        let form =
            cx.new_window_entity(|window, cx| GraphSearchForm::new(git_graph.clone(), window, cx));
        cx.run_until_parked();
        let search_text = |cx: &mut VisualTestContext| {
            git_graph.read_with(&*cx, |graph, cx| {
                graph.search_state.editor.read(cx).text(cx)
            })
        };
        assert_eq!(
            search_text(cx),
            "直した tag:v1",
            "opening the form leaves the box as it was"
        );
        form.read_with(&*cx, |form, cx| {
            assert_eq!(
                form.field_text_for_test(CommitSearchConditionKind::Tag, cx),
                "v1",
                "the form starts from the conditions already in the box"
            );
            assert_eq!(
                form.suggestions_for_test(CommitSearchConditionKind::Tag, cx),
                ["v1.0", "v1.1"]
            );
            assert_eq!(
                form.suggestions_for_test(CommitSearchConditionKind::Branch, cx),
                ["main", "feature/login", "origin/main"]
            );
        });

        form.update_in(cx, |form, window, cx| {
            form.set_field_for_test(CommitSearchConditionKind::Author, "山田 太郎", window, cx)
        });
        cx.run_until_parked();
        assert_eq!(search_text(cx), "直した tag:v1 author:\"山田 太郎\"");

        form.update_in(cx, |form, window, cx| {
            form.set_field_for_test(CommitSearchConditionKind::Tag, "v1.1", window, cx);
            form.set_field_for_test(CommitSearchConditionKind::Path, "src/", window, cx);
            form.set_field_for_test(CommitSearchConditionKind::Since, "2週間", window, cx);
        });
        cx.run_until_parked();
        form.update_in(cx, |form, window, cx| {
            form.set_merges_for_test(MergeFilter::Only, window, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            search_text(cx),
            "直した author:\"山田 太郎\" tag:v1.1 file:src/ since:2週間 merge:only"
        );

        form.update_in(cx, |form, window, cx| {
            form.set_field_for_test(CommitSearchConditionKind::Author, "", window, cx);
            form.set_merges_for_test(MergeFilter::Include, window, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            search_text(cx),
            "直した tag:v1.1 file:src/ since:2週間",
            "emptying a field takes its condition out"
        );
        // With two authors in the box the field can show only the first; opening the form must
        // not narrow the box down to it.
        git_graph.update_in(cx, |graph, window, cx| {
            graph.search_state.editor.update(cx, |editor, cx| {
                editor.set_text("author:alice 著者:bob 直した", window, cx)
            });
        });
        let second_form =
            cx.new_window_entity(|window, cx| GraphSearchForm::new(git_graph.clone(), window, cx));
        cx.run_until_parked();
        assert_eq!(search_text(cx), "author:alice 著者:bob 直した");
        second_form.read_with(&*cx, |form, cx| {
            assert_eq!(
                form.field_text_for_test(CommitSearchConditionKind::Author, cx),
                "alice"
            );
        });
    }

    #[gpui::test]
    async fn test_git_graph_tag_search_waits_for_the_tag_list(cx: &mut TestAppContext) {
        let commits = generate_random_commit_dag(&mut StdRng::seed_from_u64(24), 5, false);
        let target = commits[3].sha;
        let other = commits[1].sha;
        let (fs, repository, _multi_workspace, git_graph, cx) = open_graph(commits, cx).await;
        set_tag_list(
            &fs,
            &[
                ("v0.1.0", target),
                ("release-0.1", target),
                ("v0.2.0", other),
            ],
        );
        repository.update(cx, |repository, cx| {
            repository.apply_ref_storage_scan_for_test(
                git::repository::RefStorageScan {
                    loose_refs: None,
                    tags_fingerprint: Some(7),
                },
                cx,
            )
        });
        git_graph.update(cx, |graph, _| {
            graph.tags = None;
            graph._tags_task = None;
        });

        git_graph.update(cx, |graph, cx| {
            graph.search_for_test("タグ:v0.1".into(), cx)
        });
        git_graph.read_with(&*cx, |graph, _| {
            assert!(graph.search_state.outcome.waiting_for_tags);
            assert!(graph.search_matches_for_test().is_empty());
        });
        cx.run_until_parked();
        git_graph.read_with(&*cx, |graph, _| {
            assert!(graph.tags.is_some());
            assert!(!graph.search_state.outcome.waiting_for_tags);
            assert_eq!(graph.search_matches_for_test(), [target]);
            let selected = graph
                .selected_entry_idx
                .and_then(|index| graph.graph_data.commits.get(index))
                .map(|commit| commit.data.sha);
            assert_eq!(selected, Some(target));
        });

        git_graph.update(cx, |graph, cx| graph.search_for_test("tag:v0".into(), cx));
        cx.run_until_parked();
        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(
                graph.search_matches_for_test(),
                [other, target],
                "in the graph's order"
            );
        });

        git_graph.update(cx, |graph, cx| {
            graph.search_for_test("tag:v0 tag:release".into(), cx)
        });
        cx.run_until_parked();
        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(graph.search_matches_for_test(), [target]);
        });

        git_graph.update(cx, |graph, cx| graph.search_for_test("tag:v9".into(), cx));
        cx.run_until_parked();
        git_graph.read_with(&*cx, |graph, _| {
            assert!(graph.search_matches_for_test().is_empty());
            assert_eq!(
                graph.search_state.outcome.problems,
                [SharedString::from("No tag has \"v9\" in its name.")]
            );
        });
    }

    #[gpui::test]
    async fn test_graph_data_reloaded_after_stash_change(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;

        let initial_head = Oid::from_bytes(&[1; 20]).unwrap();
        let initial_stash = Oid::from_bytes(&[2; 20]).unwrap();
        let updated_head = Oid::from_bytes(&[3; 20]).unwrap();
        let updated_stash = Oid::from_bytes(&[4; 20]).unwrap();

        fs.set_graph_commits(
            Path::new("/project/.git"),
            vec![
                Arc::new(InitialGraphCommitData {
                    sha: initial_head,
                    parents: smallvec![initial_stash],
                    ref_names: vec!["HEAD".into(), "refs/heads/main".into()],
                }),
                Arc::new(InitialGraphCommitData {
                    sha: initial_stash,
                    parents: smallvec![],
                    ref_names: vec!["refs/stash".into()],
                }),
            ],
        );
        fs.with_git_state(Path::new("/project/.git"), true, |state| {
            state.stash_entries = git::stash::GitStash {
                entries: vec![git::stash::StashEntry {
                    index: 0,
                    oid: initial_stash,
                    message: "initial stash".to_string(),
                    branch: Some("main".to_string()),
                    timestamp: 1,
                }]
                .into(),
            };
        })
        .unwrap();

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace_weak =
            multi_workspace.read_with(&*cx, |multi, _| multi.workspace().downgrade());
        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace_weak,
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();

        let initial_shas = git_graph.read_with(&*cx, |graph, _| {
            graph
                .graph_data
                .commits
                .iter()
                .map(|commit| commit.data.sha)
                .collect::<Vec<_>>()
        });
        assert_eq!(initial_shas, vec![initial_head, initial_stash]);

        fs.set_graph_commits(
            Path::new("/project/.git"),
            vec![
                Arc::new(InitialGraphCommitData {
                    sha: updated_head,
                    parents: smallvec![updated_stash],
                    ref_names: vec!["HEAD".into(), "refs/heads/main".into()],
                }),
                Arc::new(InitialGraphCommitData {
                    sha: updated_stash,
                    parents: smallvec![],
                    ref_names: vec!["refs/stash".into()],
                }),
            ],
        );
        fs.with_git_state(Path::new("/project/.git"), true, |state| {
            state.stash_entries = git::stash::GitStash {
                entries: vec![git::stash::StashEntry {
                    index: 0,
                    oid: updated_stash,
                    message: "updated stash".to_string(),
                    branch: Some("main".to_string()),
                    timestamp: 1,
                }]
                .into(),
            };
        })
        .unwrap();

        project
            .update(cx, |project, cx| project.git_scans_complete(cx))
            .await;
        cx.run_until_parked();

        cx.draw(
            point(px(0.), px(0.)),
            gpui::size(px(1200.), px(800.)),
            |_, _| git_graph.clone().into_any_element(),
        );
        cx.run_until_parked();

        let reloaded_shas = git_graph.read_with(&*cx, |graph, _| {
            graph
                .graph_data
                .commits
                .iter()
                .map(|commit| commit.data.sha)
                .collect::<Vec<_>>()
        });
        assert_eq!(reloaded_shas, vec![updated_head, updated_stash]);
    }

    #[gpui::test]
    async fn test_graph_data_reloaded_after_tag_created(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;

        let head = Oid::from_bytes(&[1; 20]).unwrap();
        fs.set_graph_commits(
            Path::new("/project/.git"),
            vec![Arc::new(InitialGraphCommitData {
                sha: head,
                parents: smallvec![],
                ref_names: vec!["HEAD -> main".into()],
            })],
        );

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace_weak =
            multi_workspace.read_with(&*cx, |multi, _| multi.workspace().downgrade());
        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace_weak,
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();

        let ref_names_of_head = |cx: &mut gpui::VisualTestContext| {
            git_graph.read_with(&*cx, |graph, _| {
                graph
                    .graph_data
                    .commits
                    .iter()
                    .map(|commit| commit.data.ref_names.clone())
                    .collect::<Vec<_>>()
            })
        };
        assert_eq!(
            ref_names_of_head(cx),
            vec![vec![SharedString::from("HEAD -> main")]]
        );

        fs.set_graph_commits(
            Path::new("/project/.git"),
            vec![Arc::new(InitialGraphCommitData {
                sha: head,
                parents: smallvec![],
                ref_names: vec!["HEAD -> main".into(), "tag: v1.0.0".into()],
            })],
        );
        repository
            .update(cx, |repository, _| {
                repository.create_tag("v1.0.0".to_string(), head.to_string())
            })
            .await
            .unwrap()
            .unwrap();
        cx.run_until_parked();

        cx.draw(
            point(px(0.), px(0.)),
            gpui::size(px(1200.), px(800.)),
            |_, _| git_graph.clone().into_any_element(),
        );
        cx.run_until_parked();

        assert_eq!(
            ref_names_of_head(cx),
            vec![vec![
                SharedString::from("HEAD -> main"),
                SharedString::from("tag: v1.0.0"),
            ]]
        );
    }

    #[gpui::test]
    async fn test_git_graph_row_at_position_rounding(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            serde_json::json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;

        let mut rng = StdRng::seed_from_u64(42);
        let commits = generate_random_commit_dag(&mut rng, 10, false);
        fs.set_graph_commits(Path::new("/project/.git"), commits.clone());

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });

        let workspace_weak =
            multi_workspace.read_with(&*cx, |multi, _| multi.workspace().downgrade());

        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace_weak,
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();

        git_graph.update_in(cx, |graph, window, cx| {
            assert!(
                graph.graph_data.commits.len() >= 10,
                "graph should load dummy commits"
            );

            let row_height = GitGraph::row_height(window, cx);
            let origin_y = px(100.0);
            graph.graph_canvas_bounds.set(Some(Bounds {
                origin: point(px(0.0), origin_y),
                size: gpui::size(px(100.0), row_height * 50.0),
            }));

            // Scroll down by half a row so the row under a position near the
            // top of the canvas is row 1 rather than row 0.
            let scroll_offset = row_height * 0.75;
            graph.table_interaction_state.update(cx, |state, _| {
                state.set_scroll_offset(point(px(0.0), -scroll_offset))
            });
            let pos_y = origin_y + row_height * 0.5;
            let absolute_calc_row = graph.row_at_position(pos_y, window, cx);

            assert_eq!(
                absolute_calc_row,
                Some(1),
                "Row calculation should yield absolute row exactly"
            );
        });
    }

    #[gpui::test]
    async fn test_focus_handle_focuses_search_editor(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            serde_json::json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace_weak =
            multi_workspace.read_with(&*cx, |multi, _| multi.workspace().downgrade());

        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace_weak,
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();

        git_graph.update_in(cx, |graph, window, cx| {
            window.focus(&graph.focus_handle(cx), cx);
            assert!(
                graph
                    .search_state
                    .editor
                    .read(cx)
                    .focus_handle(cx)
                    .is_focused(window),
                "focusing the git graph item should focus the search editor"
            );
        });
    }

    #[gpui::test]
    async fn test_row_height_matches_uniform_list_item_height(cx: &mut TestAppContext) {
        init_test(cx);

        cx.update(|cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    *settings.theme = ThemeSettingsContent {
                        ui_font_size: Some(12.7.into()),
                        ..Default::default()
                    }
                });
            })
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            serde_json::json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;

        let mut rng = StdRng::seed_from_u64(99);
        let commits = generate_random_commit_dag(&mut rng, 20, false);
        fs.set_graph_commits(Path::new("/project/.git"), commits);

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });

        let workspace_weak =
            multi_workspace.read_with(&*cx, |multi, _| multi.workspace().downgrade());

        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace_weak,
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();

        cx.draw(
            point(px(0.), px(0.)),
            gpui::size(px(1200.), px(800.)),
            |_, _| git_graph.clone().into_any_element(),
        );
        cx.run_until_parked();

        git_graph.update_in(cx, |graph, window, cx| {
            let commit_count = graph.graph_data.commits.len();
            assert!(
                commit_count > 0,
                "need at least one commit to measure item height"
            );

            let table_state = graph.table_interaction_state.read(cx);
            let item_size = table_state.scroll_handle.0.borrow().last_item_size.expect(
                "uniform_list should have populated last_item_size after draw(); \
                     the table has not been laid out",
            );

            let measured_item_height = item_size.contents.height / commit_count as f32;
            let computed_row_height = GitGraph::row_height(window, cx);

            assert_eq!(
                computed_row_height, measured_item_height,
                "GitGraph::row_height ({}) must exactly match the height that \
                 uniform_list measured for each table row ({}). \
                 A mismatch means the canvas and table rows will drift when scrolling.",
                computed_row_height, measured_item_height,
            );
        });
    }

    #[gpui::test]
    async fn test_copy_selected_commit_tag_with_one_tag_copies_to_clipboard(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            serde_json::json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;

        let commit_sha = Oid::from_bytes(&[1; 20]).unwrap();
        let commits = vec![Arc::new(InitialGraphCommitData {
            sha: commit_sha,
            parents: smallvec![],
            ref_names: vec![
                SharedString::from("HEAD -> main"),
                SharedString::from("origin/main"),
                SharedString::from("tag: v1.0.0"),
            ],
        })];
        fs.set_graph_commits(Path::new("/project/.git"), commits);

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = multi_workspace.read_with(&*cx, |multi, _| multi.workspace().clone());
        let workspace_weak = workspace.downgrade();

        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace_weak,
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();

        git_graph.update_in(cx, |graph, window, cx| {
            assert_eq!(graph.graph_data.commits.len(), 1);
            graph.selected_entry_idx = Some(0);
            graph.copy_selected_commit_tag(&CopyCommitTag, window, cx);
        });

        assert_eq!(
            cx.read_from_clipboard().and_then(|item| item.text()),
            Some("v1.0.0".to_string())
        );
    }

    #[gpui::test]
    async fn test_copy_selected_commit_tag_with_multiple_tags_opens_picker_and_copies_selected_tag(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            serde_json::json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;

        let commit_sha = Oid::from_bytes(&[1; 20]).unwrap();
        let commits = vec![Arc::new(InitialGraphCommitData {
            sha: commit_sha,
            parents: smallvec![],
            ref_names: vec![
                SharedString::from("HEAD -> main"),
                SharedString::from("origin/main"),
                SharedString::from("tag: v1.0.0"),
                SharedString::from("tag: v1.1.0"),
            ],
        })];
        fs.set_graph_commits(Path::new("/project/.git"), commits);

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = multi_workspace.read_with(&*cx, |multi, _| multi.workspace().clone());
        let workspace_weak = workspace.downgrade();

        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace_weak,
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();

        git_graph.update_in(cx, |graph, window, cx| {
            assert_eq!(graph.graph_data.commits.len(), 1);
            graph.selected_entry_idx = Some(0);
            graph.copy_selected_commit_tag(&CopyCommitTag, window, cx);
        });

        // Ensure that nothing has been copied at this point
        assert_eq!(cx.read_from_clipboard().and_then(|item| item.text()), None);

        let picker = workspace.update(cx, |workspace, cx| {
            workspace
                .active_modal::<CommitTagPicker>(cx)
                .expect("commit tag picker is not open")
                .read(cx)
                .picker
                .clone()
        });

        picker.read_with(cx, |picker, _| {
            assert_eq!(picker.delegate.selected_index, 0);
            assert_eq!(
                picker.delegate.tag_names,
                [SharedString::from("v1.0.0"), SharedString::from("v1.1.0")]
            );
        });

        cx.dispatch_action(menu::Confirm);
        cx.run_until_parked();

        assert_eq!(
            cx.read_from_clipboard().and_then(|item| item.text()),
            Some("v1.0.0".to_string())
        );
    }

    #[gpui::test]
    async fn test_open_at_commit_reuses_loaded_graph(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({ ".git": {}, "file.txt": "content" }),
        )
        .await;

        let first_sha = Oid::from_bytes(&[1; 20]).expect("valid commit SHA");
        let second_sha = Oid::from_bytes(&[2; 20]).expect("valid commit SHA");
        fs.set_graph_commits(
            Path::new("/project/.git"),
            vec![
                Arc::new(InitialGraphCommitData {
                    sha: second_sha,
                    parents: smallvec![first_sha],
                    ref_names: vec!["HEAD -> main".into()],
                }),
                Arc::new(InitialGraphCommitData {
                    sha: first_sha,
                    parents: smallvec![],
                    ref_names: Vec::new(),
                }),
            ],
        );
        fs.set_commit_data(
            Path::new("/project/.git"),
            [first_sha, second_sha].map(|sha| {
                (
                    CommitData {
                        sha,
                        parents: smallvec![],
                        author_name: "Author".into(),
                        author_email: "author@example.com".into(),
                        commit_timestamp: 1_700_000_000,
                        subject: "Commit subject".into(),
                        message: "Commit message".into(),
                    },
                    false,
                )
            }),
        );

        let project = Project::test(fs, [Path::new("/project")], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });
        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = multi_workspace.read_with(&*cx, |multi, _| multi.workspace().clone());
        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace.downgrade(),
                None,
                window,
                cx,
            )
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(Box::new(git_graph.clone()), None, true, window, cx);
        });
        cx.run_until_parked();

        git_graph.update(cx, |graph, cx| {
            graph.select_commit_by_sha(first_sha, cx);
        });
        cx.run_until_parked();
        git_graph.update(cx, |graph, cx| {
            graph.select_commit_by_sha(second_sha, cx);
        });
        cx.run_until_parked();

        workspace.update_in(cx, |workspace, window, cx| {
            open_or_reuse_graph(
                workspace,
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                LogSource::All,
                Some(first_sha.to_string()),
                window,
                cx,
            );
        });
        cx.run_until_parked();

        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(graph.selected_entry_idx, Some(1));
        });
    }

    #[gpui::test]
    async fn test_go_back_from_commit_view_returns_to_git_graph(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({ ".git": {}, "file.txt": "content" }),
        )
        .await;

        let first_sha = Oid::from_bytes(&[1; 20]).expect("valid commit SHA");
        fs.set_graph_commits(
            Path::new("/project/.git"),
            vec![Arc::new(InitialGraphCommitData {
                sha: first_sha,
                parents: smallvec![],
                ref_names: vec!["HEAD -> main".into()],
            })],
        );
        fs.set_commit_data(
            Path::new("/project/.git"),
            [(
                CommitData {
                    sha: first_sha,
                    parents: smallvec![],
                    author_name: "Author".into(),
                    author_email: "author@example.com".into(),
                    commit_timestamp: 1_700_000_000,
                    subject: "Commit subject".into(),
                    message: "Commit message".into(),
                },
                false,
            )],
        );

        let project = Project::test(fs, [Path::new("/project")], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = multi_workspace.read_with(&*cx, |multi, _| multi.workspace().clone());

        // Open a file first, so there's something in nav history before the Git Graph tab.
        let buffer = project
            .update(cx, |project, cx| {
                project.open_local_buffer(Path::new("/project/file.txt"), cx)
            })
            .await
            .expect("file should open");
        let buffer_editor = cx.new_window_entity(|window, cx| {
            Editor::for_buffer(buffer, Some(project.clone()), window, cx)
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(
                Box::new(buffer_editor.clone()),
                None,
                true,
                window,
                cx,
            );
        });
        cx.run_until_parked();

        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace.downgrade(),
                None,
                window,
                cx,
            )
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(Box::new(git_graph.clone()), None, true, window, cx);
        });
        cx.run_until_parked();

        git_graph.update(cx, |graph, cx| {
            graph.select_commit_by_sha(first_sha, cx);
        });
        cx.run_until_parked();

        git_graph.update_in(cx, |graph, window, cx| {
            graph.open_selected_commit_view(window, cx);
        });
        cx.run_until_parked();

        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());

        pane.read_with(cx, |pane, _cx| {
            assert!(
                pane.active_item()
                    .and_then(|item| item.downcast::<CommitView>())
                    .is_some(),
                "expected the commit diff view to be active after opening a commit"
            );
        });

        pane.update_in(cx, |pane, window, cx| {
            pane.navigate_backward(&Default::default(), window, cx);
        });
        cx.run_until_parked();

        pane.read_with(cx, |pane, _cx| {
            let active_git_graph = pane
                .active_item()
                .and_then(|item| item.downcast::<GitGraph>());
            assert_eq!(
                active_git_graph,
                Some(git_graph.clone()),
                "Go Back from the commit diff view should return to the Git Graph view"
            );
        });
    }

    #[gpui::test]
    async fn test_git_graph_navigation(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            serde_json::json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;

        let mut rng = StdRng::seed_from_u64(42);
        let commits = generate_random_commit_dag(&mut rng, 10, false);
        fs.set_graph_commits(Path::new("/project/.git"), commits);

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });

        let workspace = multi_workspace.read_with(&*cx, |multi, _| multi.workspace().clone());
        let workspace_weak = workspace.downgrade();

        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace_weak,
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(Box::new(git_graph.clone()), None, true, window, cx);
        });
        cx.run_until_parked();

        git_graph.update_in(cx, |graph, window, cx| {
            graph.focus_handle(cx).focus(window, cx);
        });
        cx.run_until_parked();

        cx.draw(
            point(px(0.), px(0.)),
            gpui::size(px(1200.), px(800.)),
            |_, _| multi_workspace.clone().into_any_element(),
        );
        cx.run_until_parked();

        git_graph.update_in(cx, |graph, window, cx| {
            graph.focus_handle(cx).focus(window, cx);
        });
        cx.run_until_parked();

        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(graph.graph_data.commits.len(), 10);
        });
        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(graph.selected_entry_idx, None);
        });

        git_graph.update_in(cx, |graph, window, cx| {
            graph.select_first(&menu::SelectFirst, window, cx);
        });
        cx.run_until_parked();
        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(graph.selected_entry_idx, Some(0));
        });

        let scroll_step = git_graph.update_in(cx, |graph, window, cx| {
            (graph.visible_row_count(window, cx) / 2).max(1)
        });

        cx.dispatch_action(ScrollDown);
        cx.run_until_parked();
        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(graph.selected_entry_idx, Some(scroll_step));
        });

        cx.dispatch_action(ScrollUp);
        cx.run_until_parked();
        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(graph.selected_entry_idx, Some(0));
        });

        git_graph.update_in(cx, |graph, window, cx| {
            graph.select_next(&menu::SelectNext, window, cx);
        });
        cx.run_until_parked();
        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(graph.selected_entry_idx, Some(1));
        });

        git_graph.update_in(cx, |graph, window, cx| {
            graph.select_prev(&menu::SelectPrevious, window, cx);
        });
        cx.run_until_parked();
        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(graph.selected_entry_idx, Some(0));
        });

        git_graph.update_in(cx, |graph, window, cx| {
            graph.select_last(&menu::SelectLast, window, cx);
        });
        cx.run_until_parked();
        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(graph.selected_entry_idx, Some(9));
        });

        cx.dispatch_action(ScrollDown);
        cx.run_until_parked();
        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(graph.selected_entry_idx, Some(9));
        });

        git_graph.update_in(cx, |graph, window, cx| {
            graph.select_next(&menu::SelectNext, window, cx);
        });
        cx.run_until_parked();
        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(graph.selected_entry_idx, Some(9));
        });

        git_graph.update_in(cx, |graph, window, cx| {
            graph.select_prev(&menu::SelectPrevious, window, cx);
        });
        cx.run_until_parked();
        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(graph.selected_entry_idx, Some(8));
        });

        git_graph.update(cx, |graph, cx| {
            graph.selected_entry_idx = None;
            cx.notify();
        });
        cx.run_until_parked();
        git_graph.update_in(cx, |graph, window, cx| {
            graph.select_prev(&menu::SelectPrevious, window, cx);
        });
        cx.run_until_parked();
        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(graph.selected_entry_idx, Some(0));
        });

        git_graph.update(cx, |graph, cx| {
            graph.selected_entry_idx = None;
            cx.notify();
        });
        cx.run_until_parked();
        git_graph.update_in(cx, |graph, window, cx| {
            graph.select_next(&menu::SelectNext, window, cx);
        });
        cx.run_until_parked();
        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(graph.selected_entry_idx, Some(0));
        });
    }

    #[gpui::test]
    async fn test_global_git_command_task_runs_from_context_menu(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;

        let commit_sha = Oid::try_from("abcdef1234567890abcdef1234567890abcdef12")
            .expect("commit SHA should be valid");
        fs.set_graph_commits(
            Path::new("/project/.git"),
            vec![Arc::new(InitialGraphCommitData {
                sha: commit_sha,
                parents: SmallVec::new(),
                ref_names: Vec::new(),
            })],
        );

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("project should have an active repository")
        });
        let task_inventory = project.read_with(cx, |project, cx| {
            project
                .task_store()
                .read(cx)
                .task_inventory()
                .cloned()
                .expect("project should have a task inventory")
        });

        task_inventory.update(cx, |inventory, _| {
            inventory
                .update_file_based_tasks(
                    TaskSettingsLocation::Global(Path::new("/tasks.json")),
                    Some(
                        &serde_json::to_string(&json!([
                            // Tagged global task that should be scheduled from the Git graph context menu.
                            {
                                "label": "Git Show $ZED_GIT_SHA_SHORT",
                                "command": "git",
                                "args": ["show", "$ZED_GIT_SHA"],
                                "cwd": "$ZED_GIT_REPOSITORY_PATH",
                                "env": {
                                    "REPOSITORY": "$ZED_GIT_REPOSITORY_NAME",
                                },
                                "tags": [GIT_COMMAND_TASK_TAG],
                            },
                            // Untagged task that should not appear in the Git graph context menu.
                            {
                                "label": "Git Status",
                                "command": "git",
                                "args": ["status"],
                            },
                            // Tagged task that still should not appear because Git graph task contexts
                            // do not provide editor-specific variables.
                            {
                                "label": "Print File $ZED_FILE",
                                "command": "echo",
                                "args": ["$ZED_FILE"],
                                "tags": [GIT_COMMAND_TASK_TAG],
                            },
                        ]))
                        .expect("tasks JSON should serialize"),
                    ),
                )
                .expect("tasks should parse");
        });

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = multi_workspace.read_with(&*cx, |multi_workspace, _| {
            multi_workspace.workspace().clone()
        });
        let workspace_weak = workspace.downgrade();

        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace_weak,
                None,
                window,
                cx,
            )
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(Box::new(git_graph.clone()), None, true, window, cx);
        });
        cx.run_until_parked();

        git_graph.update_in(cx, |git_graph, window, cx| {
            assert_eq!(git_graph.graph_data.commits.len(), 1);
            git_graph.deploy_entry_context_menu(point(px(20.), px(20.)), 0, None, window, cx);
        });
        cx.run_until_parked();

        let context_menu = git_graph.read_with(&*cx, |git_graph, _| {
            git_graph
                .context_menu
                .as_ref()
                .expect("context menu should be open")
                .menu
                .clone()
        });
        context_menu.update_in(cx, |context_menu, window, cx| {
            context_menu
                .select_last(window, cx)
                .expect("custom Git task should be selectable");
            context_menu.confirm(&menu::Confirm, window, cx);
        });
        cx.run_until_parked();

        let (task_source_kind, resolved_task) = task_inventory.read_with(&*cx, |inventory, _| {
            inventory
                .last_scheduled_task(None)
                .expect("custom Git task should be scheduled")
        });

        assert!(
            matches!(task_source_kind, TaskSourceKind::AbsPath { .. }),
            "scheduled task should come from global tasks"
        );
        assert_eq!(resolved_task.resolved_label, "Git Show abcdef1");
        assert_eq!(resolved_task.resolved.command, Some("git".to_string()));
        assert_eq!(
            resolved_task.resolved.args,
            vec![
                "show".to_string(),
                "abcdef1234567890abcdef1234567890abcdef12".to_string(),
            ]
        );
        assert_eq!(
            resolved_task.resolved.cwd,
            Some(Path::new("/project").to_path_buf())
        );
        assert_eq!(
            resolved_task.resolved.env.get("REPOSITORY"),
            Some(&"project".to_string())
        );
    }

    #[gpui::test]
    async fn test_global_git_command_task_runs_from_ref_context_menu(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;

        let commit_sha = Oid::try_from("abcdef1234567890abcdef1234567890abcdef12")
            .expect("commit SHA should be valid");
        fs.set_graph_commits(
            Path::new("/project/.git"),
            vec![Arc::new(InitialGraphCommitData {
                sha: commit_sha,
                parents: SmallVec::new(),
                ref_names: vec!["HEAD -> feature-x".into()],
            })],
        );

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("project should have an active repository")
        });
        let task_inventory = project.read_with(cx, |project, cx| {
            project
                .task_store()
                .read(cx)
                .task_inventory()
                .cloned()
                .expect("project should have a task inventory")
        });

        task_inventory.update(cx, |inventory, _| {
            inventory
                .update_file_based_tasks(
                    TaskSettingsLocation::Global(Path::new("/tasks.json")),
                    Some(
                        &serde_json::to_string(&json!([
                            {
                                "label": "Check out $ZED_GIT_REF",
                                "command": "git",
                                "args": ["checkout", "$ZED_GIT_REF"],
                                "cwd": "$ZED_GIT_REPOSITORY_PATH",
                                "tags": [GIT_COMMAND_TASK_TAG],
                            },
                        ]))
                        .expect("tasks JSON should serialize"),
                    ),
                )
                .expect("tasks should parse");
        });

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = multi_workspace.read_with(&*cx, |multi_workspace, _| {
            multi_workspace.workspace().clone()
        });
        let workspace_weak = workspace.downgrade();

        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace_weak,
                None,
                window,
                cx,
            )
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(Box::new(git_graph.clone()), None, true, window, cx);
        });
        cx.run_until_parked();

        git_graph.update_in(cx, |git_graph, window, cx| {
            assert_eq!(git_graph.graph_data.commits.len(), 1);
            git_graph.deploy_entry_context_menu(
                point(px(20.), px(20.)),
                0,
                Some("feature-x".into()),
                window,
                cx,
            );
        });
        cx.run_until_parked();

        let context_menu = git_graph.read_with(&*cx, |git_graph, _| {
            git_graph
                .context_menu
                .as_ref()
                .expect("context menu should be open")
                .menu
                .clone()
        });
        context_menu.update_in(cx, |context_menu, window, cx| {
            context_menu
                .select_last(window, cx)
                .expect("custom Git task should be selectable");
            context_menu.confirm(&menu::Confirm, window, cx);
        });
        cx.run_until_parked();

        let (_task_source_kind, resolved_task) = task_inventory.read_with(&*cx, |inventory, _| {
            inventory
                .last_scheduled_task(None)
                .expect("custom Git task should be scheduled")
        });

        assert_eq!(resolved_task.resolved_label, "Check out feature-x");
        assert_eq!(
            resolved_task.resolved.args,
            vec!["checkout".to_string(), "feature-x".to_string()]
        );
    }

    fn commit_entry(sha: u8, parents: &[u8]) -> Rc<CommitEntry> {
        Rc::new(CommitEntry {
            data: Arc::new(InitialGraphCommitData {
                sha: Oid::from_bytes(&[sha; 20]).unwrap(),
                parents: parents
                    .iter()
                    .map(|parent| Oid::from_bytes(&[*parent; 20]).unwrap())
                    .collect(),
                ref_names: Vec::new(),
            }),
            lane: 0,
            color_idx: 0,
        })
    }

    fn rows(indices: &[usize]) -> BTreeSet<usize> {
        indices.iter().copied().collect()
    }

    #[test]
    fn test_apply_selection_gesture() {
        // A plain click drops whatever else was picked.
        let (selection, anchor) =
            apply_selection_gesture(&rows(&[1, 2, 3]), Some(1), 7, SelectionGesture::Replace);
        assert_eq!(selection, rows(&[7]));
        assert_eq!(anchor, 7);

        // Ctrl adds, then removes, the clicked row.
        let (selection, anchor) =
            apply_selection_gesture(&rows(&[1]), Some(1), 4, SelectionGesture::Toggle);
        assert_eq!(selection, rows(&[1, 4]));
        assert_eq!(anchor, 4);
        let (selection, _) =
            apply_selection_gesture(&selection, Some(4), 4, SelectionGesture::Toggle);
        assert_eq!(selection, rows(&[1]));

        // The last picked row cannot be toggled away: the detail panel always
        // has a commit to show.
        let (selection, _) =
            apply_selection_gesture(&rows(&[2]), Some(2), 2, SelectionGesture::Toggle);
        assert_eq!(selection, rows(&[2]));

        // Shift spans the anchor, in either direction, and keeps the anchor.
        let (selection, anchor) =
            apply_selection_gesture(&rows(&[2]), Some(2), 5, SelectionGesture::Range);
        assert_eq!(selection, rows(&[2, 3, 4, 5]));
        assert_eq!(anchor, 2);
        let (selection, anchor) =
            apply_selection_gesture(&rows(&[5]), Some(5), 3, SelectionGesture::Range);
        assert_eq!(selection, rows(&[3, 4, 5]));
        assert_eq!(anchor, 5);

        // Shift without an anchor behaves like a plain click.
        let (selection, anchor) =
            apply_selection_gesture(&rows(&[1, 2]), None, 6, SelectionGesture::Range);
        assert_eq!(selection, rows(&[6]));
        assert_eq!(anchor, 6);
    }

    #[test]
    fn test_is_contiguous_commit_range() {
        // Rows run newest first: 0 <- 1 <- 2 <- 3, and 2 is a merge of 3 and 4.
        let commits = vec![
            commit_entry(0, &[1]),
            commit_entry(1, &[2]),
            commit_entry(2, &[3, 4]),
            commit_entry(3, &[4]),
            commit_entry(4, &[]),
        ];

        assert!(is_contiguous_commit_range(&commits, &rows(&[0, 1])));
        assert!(is_contiguous_commit_range(&commits, &rows(&[0, 1, 2])));
        // A single row is a range of one.
        assert!(is_contiguous_commit_range(&commits, &rows(&[3])));

        // A gap breaks the chain.
        assert!(!is_contiguous_commit_range(&commits, &rows(&[0, 2])));
        // A merge commit cannot be anywhere but the oldest end, because the
        // squash has to know which parent the new commit hangs off.
        assert!(!is_contiguous_commit_range(&commits, &rows(&[1, 2, 3])));
        // Nothing picked is not a range.
        assert!(!is_contiguous_commit_range(&commits, &rows(&[])));
        // Out of bounds rows (a graph that shrank under the selection).
        assert!(!is_contiguous_commit_range(&commits, &rows(&[0, 99])));

        // The chain is read child-to-parent, never the other way round: the
        // same commits listed oldest first are not a range, because squashing
        // them would take the wrong commit's tree.
        let oldest_first = vec![
            commit_entry(4, &[]),
            commit_entry(3, &[4]),
            commit_entry(2, &[3, 4]),
            commit_entry(1, &[2]),
            commit_entry(0, &[1]),
        ];
        assert!(!is_contiguous_commit_range(&oldest_first, &rows(&[3, 4])));
        assert!(!is_contiguous_commit_range(&oldest_first, &rows(&[0, 1])));
    }

    #[test]
    fn test_worktree_holding_branch() {
        let worktree = |path: &str, ref_name: Option<&str>| GitWorktree {
            path: std::path::PathBuf::from(path),
            ref_name: ref_name.map(|ref_name| SharedString::from(ref_name.to_string())),
            sha: "0".into(),
            is_main: false,
            is_bare: false,
        };
        let worktrees = vec![
            worktree("/work/release", Some("refs/heads/release")),
            // A worktree sitting on a detached HEAD holds no branch.
            worktree("/work/detached", None),
            worktree("/work/nested", Some("refs/heads/feature/login")),
        ];

        assert_eq!(
            worktree_holding_branch(&worktrees, "release").map(|worktree| worktree.path.as_path()),
            Some(std::path::Path::new("/work/release"))
        );
        assert_eq!(
            worktree_holding_branch(&worktrees, "feature/login")
                .map(|worktree| worktree.path.as_path()),
            Some(std::path::Path::new("/work/nested"))
        );
        // A branch nobody else has checked out can be moved.
        assert!(worktree_holding_branch(&worktrees, "main").is_none());
        // Only the whole branch name counts, never a prefix or a suffix of one:
        // a wrong answer here either blocks a move that is fine or lets through
        // one that corrupts another worktree.
        assert!(worktree_holding_branch(&worktrees, "feature").is_none());
        assert!(worktree_holding_branch(&worktrees, "rele").is_none());
        assert!(worktree_holding_branch(&worktrees, "login").is_none());
        assert!(worktree_holding_branch(&worktrees, "ease").is_none());
        assert!(worktree_holding_branch(&worktrees, "heads/release").is_none());
        // The ref is matched under its full name too, not just the short one.
        assert!(worktree_holding_branch(&worktrees, "refs/heads/release").is_some());
        assert!(worktree_holding_branch(&[], "release").is_none());
    }

    #[test]
    fn test_commit_reaches() {
        // 0 <- 1 <- 2 is the current branch; 5 <- 6 is a branch beside it.
        let commits = vec![
            commit_entry(0, &[1]),
            commit_entry(1, &[2]),
            commit_entry(2, &[]),
            commit_entry(5, &[6]),
            commit_entry(6, &[2]),
        ];
        let oid = |sha: u8| Oid::from_bytes(&[sha; 20]).unwrap();

        assert!(commit_reaches(&commits, oid(0), oid(0)));
        assert!(commit_reaches(&commits, oid(0), oid(2)));
        // A commit on the branch beside it is not reachable from that tip.
        assert!(!commit_reaches(&commits, oid(0), oid(5)));
        // …but the commit they share is.
        assert!(commit_reaches(&commits, oid(5), oid(2)));
        // A commit the graph has not loaded ends the walk instead of guessing.
        assert!(!commit_reaches(&commits, oid(9), oid(2)));
        assert!(!commit_reaches(&commits, oid(0), oid(9)));
    }

    #[test]
    fn test_graph_drop_action() {
        let sha = Oid::from_bytes(&[7; 20]).unwrap();
        let other = Oid::from_bytes(&[8; 20]).unwrap();
        let named = |name: &str, kind: RefKind| GraphDragEnd::Ref {
            name: name.to_string().into(),
            kind,
        };
        let picked: Arc<[Oid]> = Arc::from([other, sha]);

        // A commit dropped on the checked-out branch is picked straight onto it.
        assert_eq!(
            graph_drop_action(&GraphDragEnd::Commit(sha), &named("main", RefKind::Head)),
            Some(GraphDropAction::CherryPickOntoHead {
                shas: Arc::from([sha]),
                branch: "main".into()
            })
        );
        // Another local branch has to be checked out first.
        assert_eq!(
            graph_drop_action(
                &GraphDragEnd::Commit(sha),
                &named("dev", RefKind::LocalBranch)
            ),
            Some(GraphDropAction::CherryPickAfterSwitch {
                shas: Arc::from([sha]),
                branch: "dev".into()
            })
        );
        // Part of a multi-selection brings the whole selection along, oldest
        // first as it was dragged.
        assert_eq!(
            graph_drop_action(
                &GraphDragEnd::Commits(picked.clone()),
                &named("main", RefKind::Head)
            ),
            Some(GraphDropAction::CherryPickOntoHead {
                shas: picked.clone(),
                branch: "main".into()
            })
        );
        assert_eq!(
            graph_drop_action(
                &GraphDragEnd::Commits(picked.clone()),
                &named("dev", RefKind::LocalBranch)
            ),
            Some(GraphDropAction::CherryPickAfterSwitch {
                shas: picked.clone(),
                branch: "dev".into()
            })
        );
        // Tags and remote branches are not places to land a commit.
        assert_eq!(
            graph_drop_action(&GraphDragEnd::Commit(sha), &named("v1.0", RefKind::Tag)),
            None
        );
        assert_eq!(
            graph_drop_action(
                &GraphDragEnd::Commit(sha),
                &named("origin/main", RefKind::RemoteBranch)
            ),
            None
        );
        assert_eq!(
            graph_drop_action(
                &GraphDragEnd::Commits(picked.clone()),
                &named("v1.0", RefKind::Tag)
            ),
            None
        );

        // Dragging the checked-out branch onto a commit is a reset; another
        // branch only has its ref moved.
        assert_eq!(
            graph_drop_action(&named("main", RefKind::Head), &GraphDragEnd::Commit(sha)),
            Some(GraphDropAction::ResetHeadBranch {
                branch: "main".into(),
                sha
            })
        );
        assert_eq!(
            graph_drop_action(
                &named("dev", RefKind::LocalBranch),
                &GraphDragEnd::Commit(sha)
            ),
            Some(GraphDropAction::MoveBranchRef {
                branch: "dev".into(),
                sha
            })
        );
        assert_eq!(
            graph_drop_action(&named("v1.0", RefKind::Tag), &GraphDragEnd::Commit(sha)),
            None
        );

        // A commit on another commit is a reorder; on itself it is nothing.
        assert_eq!(
            graph_drop_action(&GraphDragEnd::Commit(sha), &GraphDragEnd::Commit(other)),
            Some(GraphDropAction::MoveCommit { sha, target: other })
        );
        assert_eq!(
            graph_drop_action(&GraphDragEnd::Commit(sha), &GraphDragEnd::Commit(sha)),
            None
        );
        // Reordering goes one commit at a time, and says so rather than
        // quietly moving just the row under the pointer.
        let outside = Oid::from_bytes(&[9; 20]).unwrap();
        assert!(matches!(
            graph_drop_action(&GraphDragEnd::Commits(picked.clone()), &GraphDragEnd::Commit(outside)),
            Some(GraphDropAction::Refused(reason)) if reason.contains("one at a time")
        ));
        // Let go over the selection itself, the drag simply went nowhere.
        assert_eq!(
            graph_drop_action(
                &GraphDragEnd::Commits(picked.clone()),
                &GraphDragEnd::Commit(sha)
            ),
            None
        );

        // Files from the git panel go into the commit they land on.
        let files = vec![RepoPath::new("src/lib.rs").unwrap()];
        assert_eq!(
            graph_drop_action(
                &GraphDragEnd::Files(files.clone()),
                &GraphDragEnd::Commit(sha)
            ),
            Some(GraphDropAction::Fixup {
                sha,
                repo_paths: files.clone()
            })
        );
        assert_eq!(
            graph_drop_action(&GraphDragEnd::Files(Vec::new()), &GraphDragEnd::Commit(sha)),
            None
        );
        assert_eq!(
            graph_drop_action(
                &GraphDragEnd::Files(files.clone()),
                &named("main", RefKind::Head)
            ),
            None
        );

        // Like on like means nothing, and neither files nor a selection are
        // ever something to drop onto.
        assert_eq!(
            graph_drop_action(
                &named("dev", RefKind::LocalBranch),
                &named("main", RefKind::Head)
            ),
            None
        );
        assert_eq!(
            graph_drop_action(&GraphDragEnd::Commit(sha), &GraphDragEnd::Files(files)),
            None
        );
        assert_eq!(
            graph_drop_action(&GraphDragEnd::Commit(sha), &GraphDragEnd::Commits(picked)),
            None
        );
    }

    /// Dragging a row that is part of a multi-selection carries the
    /// selection; any other row goes on its own.
    #[test]
    fn test_dragged_commit_carries_the_selection() {
        let sha = Oid::from_bytes(&[1; 20]).unwrap();
        let other = Oid::from_bytes(&[2; 20]).unwrap();
        let dragged = |selection: &[Oid]| DraggedCommit {
            sha,
            subject: "subject".into(),
            selection: Arc::from(selection),
        };
        assert_eq!(
            GraphDragEnd::dragged_commit(&dragged(&[sha])),
            GraphDragEnd::Commit(sha)
        );
        assert_eq!(
            GraphDragEnd::dragged_commit(&dragged(&[other, sha])),
            GraphDragEnd::Commits(Arc::from([other, sha]))
        );
    }

    #[test]
    fn test_dropped_files_refusal() {
        assert_eq!(dropped_files_refusal(Some(1), Some(1)), None);
        assert!(
            dropped_files_refusal(Some(1), Some(2))
                .is_some_and(|reason| reason.contains("another repository")),
            "files of another repository must not go into this graph's commits"
        );
        assert!(dropped_files_refusal(None, Some(1)).is_some());
        assert!(dropped_files_refusal(Some(1), None::<i32>).is_some());
        assert!(dropped_files_refusal(None::<i32>, None).is_some());
    }

    #[test]
    fn test_rewritable_history() {
        let oid = |sha: u8| Oid::from_bytes(&[sha; 20]).unwrap();

        // 0 <- 1 <- 2 <- 3 (root), with 9 on a branch beside it.
        let linear = vec![
            commit_entry(0, &[1]),
            commit_entry(9, &[2]),
            commit_entry(1, &[2]),
            commit_entry(2, &[3]),
            commit_entry(3, &[]),
        ];
        let history = rewritable_history(&linear, oid(0));
        assert_eq!(history.commits, vec![oid(0), oid(1), oid(2)]);
        assert_eq!(
            history.rows,
            [(0, oid(0)), (2, oid(1)), (3, oid(2))]
                .into_iter()
                .collect()
        );
        // The root commit is not something to move, but files can still go
        // into it.
        assert_eq!(history.root_row, Some(4));

        // A merge commit ends the stretch, and so does everything below it:
        // 0 <- 1 (merge of 2 and 4) <- 2 <- 3.
        let merged = vec![
            commit_entry(0, &[1]),
            commit_entry(1, &[2, 4]),
            commit_entry(4, &[2]),
            commit_entry(2, &[3]),
            commit_entry(3, &[]),
        ];
        let history = rewritable_history(&merged, oid(0));
        assert_eq!(history.commits, vec![oid(0)]);
        assert_eq!(history.root_row, None);
        // With HEAD on the merge itself there is nothing to rewrite.
        assert!(rewritable_history(&merged, oid(1)).commits.is_empty());

        // A parent the graph has not loaded ends the walk instead of guessing.
        let partial = vec![commit_entry(0, &[1]), commit_entry(1, &[2])];
        let history = rewritable_history(&partial, oid(0));
        assert_eq!(history.commits, vec![oid(0), oid(1)]);
        assert_eq!(history.root_row, None);
        // So does a HEAD it has not loaded.
        assert!(rewritable_history(&partial, oid(7)).commits.is_empty());
    }

    #[test]
    fn test_plan_commit_move() {
        let oid = |sha: u8| Oid::from_bytes(&[sha; 20]).unwrap();
        // Newest first, the way the graph lists them: 1 is HEAD.
        let history = [oid(1), oid(2), oid(3), oid(4), oid(5)];
        let on_branch = |_: Oid| true;
        let moved = |dragged: u8, target: u8| {
            let plan = plan_commit_move(&history, oid(dragged), oid(target), on_branch).unwrap();
            // What the modal opens with, newest first like the graph, so that
            // the expectations read the way the rows look.
            let mut rows = plan
                .initial_order
                .iter()
                .map(|index| plan.commits_oldest_first[*index])
                .collect::<Vec<_>>();
            rows.reverse();
            (plan.commits_oldest_first, rows)
        };

        // Down: the dragged commit takes the target's place and the rows in
        // between move up into the gap it left.
        let (range, rows) = moved(1, 4);
        assert_eq!(range, vec![oid(4), oid(3), oid(2), oid(1)]);
        assert_eq!(rows, vec![oid(2), oid(3), oid(4), oid(1)]);
        // Up: the same the other way.
        let (range, rows) = moved(5, 2);
        assert_eq!(range, vec![oid(5), oid(4), oid(3), oid(2)]);
        assert_eq!(rows, vec![oid(5), oid(2), oid(3), oid(4)]);
        // Next to each other is a swap of the two.
        let (range, rows) = moved(3, 4);
        assert_eq!(range, vec![oid(4), oid(3)]);
        assert_eq!(rows, vec![oid(4), oid(3)]);
        let (_, rows) = moved(4, 3);
        assert_eq!(rows, vec![oid(4), oid(3)]);

        // Onto itself nothing moves.
        assert!(plan_commit_move(&history, oid(2), oid(2), on_branch).is_err());

        // A commit off the stretch is refused, and the reason says whether it
        // is on another branch or only below a merge.
        let elsewhere = plan_commit_move(&history, oid(2), oid(9), |_| false).unwrap_err();
        assert!(
            elsewhere.contains("not in the current branch's history"),
            "{elsewhere}"
        );
        let below_merge = plan_commit_move(&history, oid(9), oid(2), |_| true).unwrap_err();
        assert!(below_merge.contains("merge commit"), "{below_merge}");
    }

    #[test]
    fn test_row_drop_context_hint() {
        let oid = |sha: u8| Oid::from_bytes(&[sha; 20]).unwrap();
        // Rows 0, 2 and 3 are the rewritable stretch, row 4 its root, row 1 a
        // commit of another branch.
        let history = Rc::new(rewritable_history(
            &[
                commit_entry(0, &[1]),
                commit_entry(9, &[2]),
                commit_entry(1, &[2]),
                commit_entry(2, &[3]),
                commit_entry(3, &[]),
            ],
            oid(0),
        ));

        let moving = RowDropContext::MoveCommit {
            dragged: oid(1),
            history: history.clone(),
        };
        let takes_commit = (0..6)
            .filter(|row| moving.accepts_commit(*row))
            .collect::<Vec<_>>();
        // Not its own row, not another branch's, not the root, and nothing
        // past the end of the list.
        assert_eq!(takes_commit, vec![0, 3]);
        assert!(moving.hint(0).is_some_and(|hint| hint.contains("reorder")));
        assert!((0..6).all(|row| !moving.accepts_files(row)));

        let files = RowDropContext::Files { history };
        let takes_files = (0..6)
            .filter(|row| files.accepts_files(*row))
            .collect::<Vec<_>>();
        assert_eq!(takes_files, vec![0, 2, 3, 4]);
        assert!(files.hint(4).is_some_and(|hint| hint.contains("fixup")));
        assert!((0..6).all(|row| !files.accepts_commit(row)));

        let reset = RowDropContext::Ref {
            name: "main".into(),
            kind: RefKind::Head,
        };
        assert!(reset.hint(1).is_some_and(|hint| hint.contains("reset")));
        assert!(RowDropContext::Nothing.hint(0).is_none());
    }

    /// Before letting go, the chip says how many commits it is about to take.
    #[test]
    fn test_cherry_pick_drop_wording() {
        let sha = Oid::from_bytes(&[1; 20]).unwrap();
        let other = Oid::from_bytes(&[2; 20]).unwrap();

        assert_eq!(
            chip_drop_hint(1, "main", RefKind::Head).as_deref(),
            Some("Drop here: cherry-pick")
        );
        assert_eq!(
            chip_drop_hint(3, "main", RefKind::Head).as_deref(),
            Some("Drop here: cherry-pick 3 commits")
        );
        assert_eq!(
            chip_drop_hint(2, "dev", RefKind::LocalBranch).as_deref(),
            Some("Drop here: switch to dev and cherry-pick 2 commits")
        );
        assert_eq!(chip_drop_hint(1, "v1", RefKind::Tag), None);

        let (title, detail) = cherry_pick_drop_prompt(&[sha], "main", false);
        assert_eq!(title, "Cherry-pick this commit onto the current branch?");
        assert!(detail.contains(&sha.display_short()), "{detail}");
        let (title, detail) = cherry_pick_drop_prompt(&[other, sha], "main", false);
        assert!(title.contains("2 commits"), "{title}");
        assert!(detail.contains("Cherry-picks 2 commits"), "{detail}");
        let (title, detail) = cherry_pick_drop_prompt(&[other, sha], "dev", true);
        assert!(title.contains("Switch to dev"), "{title}");
        assert!(detail.contains("cherry-picks 2 commits"), "{detail}");
    }

    #[gpui::test]
    async fn test_multi_selection_click_gestures(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            serde_json::json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;

        let shas = (1..=4u8)
            .map(|n| Oid::from_bytes(&[n; 20]).unwrap())
            .collect::<Vec<_>>();
        let commits = (0..4)
            .map(|idx| {
                Arc::new(InitialGraphCommitData {
                    sha: shas[idx],
                    parents: shas.get(idx + 1).copied().into_iter().collect(),
                    ref_names: Vec::new(),
                })
            })
            .collect::<Vec<_>>();
        fs.set_graph_commits(Path::new("/project/.git"), commits);

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = multi_workspace.read_with(&*cx, |multi, _| multi.workspace().clone());
        let workspace_weak = workspace.downgrade();

        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace_weak,
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();

        let click = |modifiers: Modifiers| {
            ClickEvent::Mouse(gpui::MouseClickEvent {
                down: gpui::MouseDownEvent {
                    button: MouseButton::Left,
                    position: point(px(0.), px(0.)),
                    modifiers,
                    click_count: 1,
                    first_mouse: false,
                },
                up: gpui::MouseUpEvent {
                    button: MouseButton::Left,
                    position: point(px(0.), px(0.)),
                    modifiers,
                    click_count: 1,
                },
            })
        };

        git_graph.update_in(cx, |graph, window, cx| {
            assert_eq!(graph.graph_data.commits.len(), 4);

            graph.handle_entry_click(
                0,
                &click(Modifiers::none()),
                ScrollStrategy::Nearest,
                None,
                window,
                cx,
            );
            assert_eq!(graph.selected_entry_idx, Some(0));
            assert!(graph.multi_selection.is_empty());
            assert_eq!(graph.selected_rows(), vec![0]);

            graph.handle_entry_click(
                2,
                &click(Modifiers::shift()),
                ScrollStrategy::Nearest,
                None,
                window,
                cx,
            );
            assert_eq!(graph.selected_rows(), vec![0, 1, 2]);
            assert_eq!(graph.selected_entry_idx, Some(2));

            graph.handle_entry_click(
                1,
                &click(Modifiers::secondary_key()),
                ScrollStrategy::Nearest,
                None,
                window,
                cx,
            );
            assert_eq!(graph.selected_rows(), vec![0, 2]);

            // Oldest first is what cherry-pick wants, so the shas come back
            // reversed from the row order.
            assert_eq!(
                graph.selected_shas_oldest_first(),
                vec![shas[2].to_string(), shas[0].to_string()]
            );

            // A plain click collapses back to one row.
            graph.handle_entry_click(
                3,
                &click(Modifiers::none()),
                ScrollStrategy::Nearest,
                None,
                window,
                cx,
            );
            assert_eq!(graph.selected_rows(), vec![3]);
            assert!(graph.multi_selection.is_empty());

            // Picked rows are row numbers, so they cannot outlive the rows
            // they point at: refetching the graph has to drop them, or an
            // operation would run against whatever commit took that row.
            graph.handle_entry_click(
                1,
                &click(Modifiers::none()),
                ScrollStrategy::Nearest,
                None,
                window,
                cx,
            );
            graph.handle_entry_click(
                3,
                &click(Modifiers::shift()),
                ScrollStrategy::Nearest,
                None,
                window,
                cx,
            );
            assert_eq!(graph.selected_rows(), vec![1, 2, 3]);
            graph.invalidate_state(cx);
            assert!(graph.multi_selection.is_empty());
            assert_eq!(graph.selection_anchor_idx, None);
            assert!(graph.graph_data.commits.is_empty());
        });
    }

    /// A commit dropped on another commit row opens the todo editor for the
    /// commits between them, already reordered; nothing is rewritten until it
    /// is confirmed there. A drop the graph cannot carry out leaves it closed.
    #[gpui::test]
    async fn test_commit_dropped_on_a_row_opens_the_reordered_todo_list(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            serde_json::json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;

        // Newest first: rows 0, 2, 3 and 4 are the current branch down to its
        // root commit, and row 1 is a commit of another branch beside it.
        let shas = (1..=5u8)
            .map(|n| Oid::from_bytes(&[n; 20]).unwrap())
            .collect::<Vec<_>>();
        let rows: [(usize, &[usize]); 5] = [(0, &[1]), (4, &[2]), (1, &[2]), (2, &[3]), (3, &[])];
        let commits = rows
            .iter()
            .map(|(sha, parents)| {
                Arc::new(InitialGraphCommitData {
                    sha: shas[*sha],
                    parents: parents.iter().map(|parent| shas[*parent]).collect(),
                    ref_names: Vec::new(),
                })
            })
            .collect::<Vec<_>>();
        fs.set_graph_commits(Path::new("/project/.git"), commits);
        fs.set_branch_name(Path::new("/project/.git"), Some("main"));
        fs.set_head_for_repo(Path::new("/project/.git"), &[], shas[0].to_string());

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = multi_workspace.read_with(&*cx, |multi, _| multi.workspace().clone());
        let workspace_weak = workspace.downgrade();

        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace_weak,
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();

        let dragged = |sha: Oid, selection: &[Oid]| DraggedCommit {
            sha,
            subject: "subject".into(),
            selection: Arc::from(selection),
        };
        let open_modal_rows = |cx: &mut gpui::VisualTestContext| {
            workspace.read_with(cx, |workspace, cx| {
                workspace
                    .active_modal::<RewriteRangeModal>(cx)
                    .map(|modal| modal.read(cx).row_shas())
            })
        };

        git_graph.update_in(cx, |graph, window, cx| {
            assert_eq!(graph.graph_data.commits.len(), 5);
            assert_eq!(graph.head_commit_sha(cx), Some(shas[0]));
            // A commit of another branch cannot be moved into this one.
            graph.handle_commit_drop_on_row(&dragged(shas[4], &[shas[4]]), 0, window, cx);
            // Reordering goes one commit at a time.
            graph.handle_commit_drop_on_row(&dragged(shas[0], &[shas[1], shas[0]]), 3, window, cx);
        });
        cx.run_until_parked();
        assert_eq!(open_modal_rows(cx), None);

        // HEAD dragged onto the third commit: the range is the three of them,
        // and HEAD's commit takes the oldest place.
        git_graph.update_in(cx, |graph, window, cx| {
            graph.handle_commit_drop_on_row(&dragged(shas[0], &[shas[0]]), 3, window, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            open_modal_rows(cx),
            Some(vec![
                shas[0].to_string(),
                shas[2].to_string(),
                shas[1].to_string(),
            ])
        );
        workspace.read_with(cx, |workspace, cx| {
            let modal = workspace
                .active_modal::<RewriteRangeModal>(cx)
                .expect("the todo list is open");
            assert!(modal.read(cx).opened_reordered());
        });
    }

    #[test]
    fn test_classify_ref_decoration() {
        let remotes: Vec<SharedString> = vec!["origin".into(), "fork".into()];

        assert_eq!(
            classify_ref_decoration("HEAD -> main", Some("main"), &remotes),
            Some((RefKind::Head, "main".into()))
        );
        assert_eq!(
            classify_ref_decoration("main", Some("main"), &remotes),
            Some((RefKind::Head, "main".into()))
        );
        assert_eq!(
            classify_ref_decoration("main", Some("other"), &remotes),
            Some((RefKind::LocalBranch, "main".into()))
        );
        assert_eq!(
            classify_ref_decoration("origin/main", Some("main"), &remotes),
            Some((RefKind::RemoteBranch, "origin/main".into()))
        );
        assert_eq!(
            classify_ref_decoration("fork/feature/x", None, &remotes),
            Some((RefKind::RemoteBranch, "fork/feature/x".into()))
        );
        assert_eq!(
            classify_ref_decoration("tag: v1.0", None, &remotes),
            Some((RefKind::Tag, "v1.0".into()))
        );
        assert_eq!(classify_ref_decoration("HEAD", None, &remotes), None);
        assert_eq!(classify_ref_decoration("", None, &remotes), None);

        // A local branch whose name contains a slash must not be taken for a
        // remote branch just because it has one.
        assert_eq!(
            classify_ref_decoration("feature/login", Some("main"), &remotes),
            Some((RefKind::LocalBranch, "feature/login".into()))
        );

        // Before the branch list has loaded there are no remote names to match
        // against, and only `origin/` is assumed to be a remote.
        assert_eq!(
            classify_ref_decoration("origin/main", None, &[]),
            Some((RefKind::RemoteBranch, "origin/main".into()))
        );
        assert_eq!(
            classify_ref_decoration("fork/main", None, &[]),
            Some((RefKind::LocalBranch, "fork/main".into()))
        );
    }

    #[test]
    fn test_remote_names_from_branches() {
        let branch = |ref_name: &str| Branch {
            is_head: false,
            ref_name: ref_name.to_string().into(),
            upstream: None,
            most_recent_commit: None,
        };
        let branches = vec![
            branch("refs/heads/main"),
            branch("refs/remotes/origin/main"),
            branch("refs/remotes/origin/dev"),
            branch("refs/remotes/fork/main"),
        ];
        assert_eq!(
            remote_names_from_branches(&branches),
            vec![SharedString::from("origin"), SharedString::from("fork")]
        );
    }

    #[gpui::test]
    async fn test_commit_message_rendered_as_markdown(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({ ".git": {}, "file.txt": "content" }),
        )
        .await;

        let commit_sha = Oid::from_bytes(&[1; 20]).unwrap();
        let commits = vec![Arc::new(InitialGraphCommitData {
            sha: commit_sha,
            parents: smallvec![],
            ref_names: vec!["HEAD -> main".into()],
        })];
        fs.set_graph_commits(Path::new("/project/.git"), commits);
        fs.set_commit_data(
            Path::new("/project/.git"),
            [(
                CommitData {
                    sha: commit_sha,
                    parents: smallvec![],
                    author_name: "Author".into(),
                    author_email: "author@example.com".into(),
                    commit_timestamp: 1_700_000_000,
                    subject: "Fix crash".into(),
                    message: "Fix crash\n\nThis fixes a crash that occurred when...".into(),
                },
                false,
            )],
        );

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace_weak =
            multi_workspace.read_with(&*cx, |multi, _| multi.workspace().downgrade());

        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace_weak,
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();

        // Select the commit to trigger loading the commit message
        git_graph.update_in(cx, |graph, window, cx| {
            graph.select_first(&menu::SelectFirst, window, cx);
        });
        cx.run_until_parked();

        // Verify the commit message was loaded as markdown
        git_graph.read_with(&*cx, |graph, app| {
            let message = graph
                .selected_commit_message
                .as_ref()
                .expect("selected_commit_message should be Some");
            assert_eq!(message.sha, commit_sha);
            let source = message.message.read_with(app, |m, _| m.source().to_owned());
            assert!(source.contains("Fix crash"));
            assert!(source.contains("This fixes a crash"));
        });
    }

    #[gpui::test]
    async fn test_long_commit_message_is_constrained_to_scroll_viewport(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({ ".git": {}, "file.txt": "content" }),
        )
        .await;

        let commit_sha = Oid::from_bytes(&[1; 20]).expect("commit SHA should be valid");
        let commits = vec![Arc::new(InitialGraphCommitData {
            sha: commit_sha,
            parents: smallvec![],
            ref_names: vec!["HEAD -> main".into()],
        })];
        fs.set_graph_commits(Path::new("/project/.git"), commits);

        let message = (0..40)
            .map(|line_number| {
                format!(
                    "Line {line_number}: This commit message is long enough to require scrolling."
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        fs.set_commit_data(
            Path::new("/project/.git"),
            [(
                CommitData {
                    sha: commit_sha,
                    parents: smallvec![],
                    author_name: "Author".into(),
                    author_email: "author@example.com".into(),
                    commit_timestamp: 1_700_000_000,
                    subject: "Long commit message".into(),
                    message: message.into(),
                },
                false,
            )],
        );

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });
        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace_weak =
            multi_workspace.read_with(&*cx, |multi, _| multi.workspace().downgrade());
        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace_weak,
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();

        git_graph.update_in(cx, |graph, window, cx| {
            graph.select_first(&menu::SelectFirst, window, cx);
        });
        cx.run_until_parked();

        git_graph.update(cx, |graph, cx| {
            graph.selected_commit_diff = Some(CommitDiff {
                files: vec![CommitFile {
                    path: RepoPath::new("file.txt").expect("repository path should be valid"),
                    old_text: Some("content".into()),
                    new_text: Some("updated content".into()),
                    is_binary: false,
                }],
                is_shallow_boundary: false,
            });
            graph.selected_commit_diff_stats = Some((1, 1));
            cx.notify();
        });

        cx.draw(
            point(px(0.), px(0.)),
            gpui::size(px(1200.), px(800.)),
            |_, _| git_graph.clone().into_any_element(),
        );
        cx.run_until_parked();

        let (message_scroll_handle, changed_files_scroll_handle) =
            git_graph.read_with(&*cx, |graph, _| {
                (
                    graph
                        .selected_commit_message
                        .as_ref()
                        .expect("selected commit message should be loaded")
                        .scroll_handle
                        .clone(),
                    graph.changed_files_scroll_handle.clone(),
                )
            });
        let maximum_message_height = git_graph.update_in(cx, |_, window, cx| {
            editor::hover_markdown_style(window, cx)
                .base_text_style
                .line_height_in_pixels(window.rem_size())
                * 12.
        });
        let message_bounds = message_scroll_handle.bounds();
        let changed_files_bounds = changed_files_scroll_handle.0.borrow().base_handle.bounds();

        assert!(
            message_bounds.size.height <= maximum_message_height,
            "commit message viewport height ({}) should not exceed its maximum ({})",
            message_bounds.size.height,
            maximum_message_height,
        );
        assert!(
            message_scroll_handle.max_offset().y > px(0.),
            "long commit message should be scrollable"
        );
        assert!(
            message_bounds.bottom() <= changed_files_bounds.top(),
            "commit message viewport {message_bounds:?} should not overlap changed files {changed_files_bounds:?}"
        );
    }

    #[gpui::test]
    async fn test_commit_message_not_reloaded_for_same_sha(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({ ".git": {}, "file.txt": "content" }),
        )
        .await;

        let commit_sha = Oid::from_bytes(&[1; 20]).unwrap();
        let commits = vec![Arc::new(InitialGraphCommitData {
            sha: commit_sha,
            parents: smallvec![],
            ref_names: vec!["HEAD -> main".into()],
        })];
        fs.set_graph_commits(Path::new("/project/.git"), commits);
        fs.set_commit_data(
            Path::new("/project/.git"),
            [(
                CommitData {
                    sha: commit_sha,
                    parents: smallvec![],
                    author_name: "Author".into(),
                    author_email: "author@example.com".into(),
                    commit_timestamp: 1_700_000_000,
                    subject: "Fix crash".into(),
                    message: "Fix crash\n\nBody text.".into(),
                },
                false,
            )],
        );

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace_weak =
            multi_workspace.read_with(&*cx, |multi, _| multi.workspace().downgrade());

        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace_weak,
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();

        // Select the commit to load the message
        git_graph.update_in(cx, |graph, window, cx| {
            graph.select_first(&menu::SelectFirst, window, cx);
        });
        cx.run_until_parked();

        // Verify message is loaded
        let message_entity_id = git_graph.read_with(&*cx, |graph, _| {
            graph
                .selected_commit_message
                .as_ref()
                .map(|m| m.message.entity_id())
        });
        assert!(message_entity_id.is_some());

        // Select the same commit again to trigger the early-return logic
        git_graph.update_in(cx, |graph, window, cx| {
            graph.select_first(&menu::SelectFirst, window, cx);
        });
        cx.run_until_parked();

        // Verify the message entity is the same (not replaced)
        git_graph.read_with(&*cx, |graph, _| {
            let new_entity_id = graph
                .selected_commit_message
                .as_ref()
                .map(|m| m.message.entity_id());
            assert_eq!(message_entity_id, new_entity_id);
        });
    }

    #[test]
    fn test_ref_chips_are_classified_when_commits_arrive() {
        let mut graph = GraphData::new(4);
        graph.set_ref_chip_context(RefChipContext {
            head_branch_name: Some("main".into()),
            remote_names: vec!["origin".into()],
        });
        let sha = Oid::from_bytes(&[3; 20]).unwrap();
        let bare = Oid::from_bytes(&[4; 20]).unwrap();
        let mut ref_names: Vec<SharedString> = vec![
            "HEAD -> main".into(),
            "origin/main".into(),
            "tag: v1".into(),
            "fork/topic".into(),
        ];
        ref_names.extend((0..21).map(|number| SharedString::from(format!("release/{number}"))));
        graph.add_commits(&[
            Arc::new(InitialGraphCommitData {
                sha,
                parents: smallvec![bare],
                ref_names,
            }),
            Arc::new(InitialGraphCommitData {
                sha: bare,
                parents: smallvec![],
                ref_names: Vec::new(),
            }),
        ]);

        let chips = graph
            .ref_chips
            .get(&sha)
            .cloned()
            .expect("chips of the tip");
        assert!(!graph.ref_chips.contains_key(&bare));
        let kinds = |chips: &[RefChip]| {
            chips
                .iter()
                .take(4)
                .map(|chip| chip.classified.as_ref().map(|(kind, _)| *kind))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            kinds(&chips),
            [
                Some(RefKind::Head),
                Some(RefKind::RemoteBranch),
                Some(RefKind::Tag),
                Some(RefKind::LocalBranch),
            ]
        );

        let (shown, hidden_count) = visible_ref_chips(&chips, ROW_REF_CHIP_LIMIT);
        assert_eq!(shown.len(), ROW_REF_CHIP_LIMIT);
        assert_eq!(hidden_count, chips.len() - ROW_REF_CHIP_LIMIT);
        let tooltip = hidden_ref_chips_tooltip(&chips[ROW_REF_CHIP_LIMIT..]);
        let lines = tooltip.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), HIDDEN_REF_TOOLTIP_LIMIT + 1);
        assert_eq!(lines.first(), Some(&"fork/topic"));
        assert_eq!(lines.last(), Some(&"and 2 more"));
        assert_eq!(
            hidden_ref_chips_tooltip(&chips[..2]),
            "HEAD -> main\norigin/main"
        );
        assert_eq!(visible_ref_chips(&chips, DETAIL_REF_CHIP_LIMIT).1, 0);
        assert_eq!(visible_ref_chips(&chips, 30).0.len(), chips.len());

        // The branch list learning about another remote classifies the loaded chips again.
        graph.set_ref_chip_context(RefChipContext {
            head_branch_name: Some("main".into()),
            remote_names: vec!["origin".into(), "fork".into()],
        });
        assert_eq!(
            kinds(&graph.ref_chips[&sha]),
            [
                Some(RefKind::Head),
                Some(RefKind::RemoteBranch),
                Some(RefKind::Tag),
                Some(RefKind::RemoteBranch),
            ]
        );

        graph.clear();
        assert!(graph.ref_chips.is_empty());
    }

    fn tag_list_command() -> String {
        format!(
            "for-each-ref --sort=-creatordate --format={} refs/tags/",
            git::repository::TAG_LIST_FORMAT
        )
    }

    fn set_tag_list(fs: &FakeFs, tags: &[(&str, Oid)]) {
        let stdout = tags
            .iter()
            .map(|(name, sha)| {
                format!("refs/tags/{name}\0commit\0{sha}\0\01700000000 +0000\0subject\n")
            })
            .collect::<String>();
        fs.with_git_state(Path::new("/project/.git"), false, |state| {
            state.command_outputs.insert(
                tag_list_command(),
                git::repository::GitCommandOutput {
                    stdout,
                    stderr: String::new(),
                    exit_code: Some(0),
                },
            );
        })
        .unwrap();
    }

    pub(super) fn git_invocations(fs: &FakeFs, subcommand: &str) -> Vec<Vec<String>> {
        fs.with_git_state(Path::new("/project/.git"), false, |state| {
            state
                .run_command_log
                .iter()
                .filter(|(args, _)| args.first().map(String::as_str) == Some(subcommand))
                .map(|(args, _)| args.clone())
                .collect()
        })
        .unwrap()
    }

    fn tag_names(graph: &Entity<GitGraph>, cx: &mut VisualTestContext) -> Option<Vec<String>> {
        graph.read_with(&*cx, |graph, _| {
            graph
                .tags
                .as_ref()
                .map(|tags| tags.iter().map(|tag| tag.name.clone()).collect())
        })
    }

    pub(super) fn draw_workspace(
        multi_workspace: &Entity<workspace::MultiWorkspace>,
        cx: &mut VisualTestContext,
    ) {
        let multi_workspace = multi_workspace.clone();
        cx.draw(
            point(px(0.), px(0.)),
            gpui::size(px(1200.), px(800.)),
            |_, _| multi_workspace.into_any_element(),
        );
        cx.run_until_parked();
    }

    /// Sets up a project whose fake repository has `commits`, and a graph tab showing them.
    pub(super) async fn open_graph(
        commits: Vec<Arc<InitialGraphCommitData>>,
        cx: &mut TestAppContext,
    ) -> (
        Arc<FakeFs>,
        Entity<Repository>,
        Entity<workspace::MultiWorkspace>,
        Entity<GitGraph>,
        &mut VisualTestContext,
    ) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            serde_json::json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;
        fs.set_graph_commits(Path::new("/project/.git"), commits);
        set_tag_list(&fs, &[]);

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.run_until_parked();
        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = multi_workspace.read_with(&*cx, |multi, _| multi.workspace().clone());
        let workspace_weak = workspace.downgrade();
        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace_weak,
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(Box::new(git_graph.clone()), None, true, window, cx);
        });
        cx.run_until_parked();
        draw_workspace(&multi_workspace, cx);
        (fs, repository, multi_workspace, git_graph, cx)
    }

    #[gpui::test]
    async fn test_tags_survive_head_changes_and_reload_when_tags_change(cx: &mut TestAppContext) {
        let commits = generate_random_commit_dag(&mut StdRng::seed_from_u64(7), 5, false);
        let tagged = commits[0].sha;
        let (fs, repository, multi_workspace, git_graph, cx) = open_graph(commits, cx).await;
        let tag_list_runs = |fs: &FakeFs| {
            git_invocations(fs, "for-each-ref")
                .iter()
                .filter(|args| args.last().map(String::as_str) == Some("refs/tags/"))
                .count()
        };

        // A host that can tell when its tags change.
        set_tag_list(&fs, &[("v1", tagged)]);
        repository.update(cx, |repository, cx| {
            repository.apply_ref_storage_scan_for_test(
                git::repository::RefStorageScan {
                    loose_refs: None,
                    tags_fingerprint: Some(1),
                },
                cx,
            )
        });
        cx.run_until_parked();
        draw_workspace(&multi_workspace, cx);
        assert_eq!(tag_names(&git_graph, cx), Some(vec!["v1".to_string()]));
        let runs = tag_list_runs(&fs);

        // Moving HEAD (a commit, a checkout) leaves the tags as they were.
        repository.update(cx, |_, cx| cx.emit(RepositoryEvent::HeadChanged));
        cx.run_until_parked();
        draw_workspace(&multi_workspace, cx);
        git_graph.update(cx, |graph, cx| graph.invalidate_state(cx));
        draw_workspace(&multi_workspace, cx);
        assert_eq!(tag_names(&git_graph, cx), Some(vec!["v1".to_string()]));
        assert_eq!(tag_list_runs(&fs), runs, "no reload for a HEAD change");

        // A tag made outside Zed: the list is read again, and the old one stays up meanwhile.
        set_tag_list(&fs, &[("v2", tagged), ("v1", tagged)]);
        repository.update(cx, |repository, cx| {
            repository.apply_ref_storage_scan_for_test(
                git::repository::RefStorageScan {
                    loose_refs: None,
                    tags_fingerprint: Some(2),
                },
                cx,
            )
        });
        assert_eq!(tag_names(&git_graph, cx), Some(vec!["v1".to_string()]));
        cx.run_until_parked();
        assert_eq!(
            tag_names(&git_graph, cx),
            Some(vec!["v2".to_string(), "v1".to_string()])
        );
        assert_eq!(tag_list_runs(&fs), runs + 1);

        // A host that cannot tell gets its tags reloaded with every HEAD change instead.
        repository.update(cx, |repository, cx| {
            repository.apply_ref_storage_scan_for_test(Default::default(), cx)
        });
        repository.update(cx, |_, cx| cx.emit(RepositoryEvent::HeadChanged));
        cx.run_until_parked();
        assert_eq!(tag_list_runs(&fs), runs + 2);
    }

    #[gpui::test]
    async fn test_describe_waits_for_the_selection_to_settle(cx: &mut TestAppContext) {
        let commits = generate_random_commit_dag(&mut StdRng::seed_from_u64(11), 10, false);
        let (fs, _repository, _multi_workspace, git_graph, cx) = open_graph(commits, cx).await;
        let described = |fs: &FakeFs| {
            git_invocations(fs, "describe")
                .into_iter()
                .filter_map(|args| args.last().cloned())
                .collect::<Vec<_>>()
        };

        git_graph.update_in(cx, |graph, window, cx| {
            graph.select_first(&menu::SelectFirst, window, cx);
        });
        cx.run_until_parked();
        // Walking down the rows faster than the debounce.
        for _ in 0..4 {
            cx.executor().advance_clock(TAG_DISTANCE_DEBOUNCE / 3);
            cx.run_until_parked();
            git_graph.update_in(cx, |graph, window, cx| {
                graph.select_next(&menu::SelectNext, window, cx);
            });
        }
        cx.run_until_parked();
        assert_eq!(described(&fs), Vec::<String>::new(), "nothing while moving");

        cx.executor().advance_clock(TAG_DISTANCE_DEBOUNCE);
        cx.run_until_parked();
        let settled_on = git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(graph.selected_entry_idx, Some(4));
            graph.graph_data.commits[4].data.sha.to_string()
        });
        assert_eq!(described(&fs), [settled_on]);
    }

    #[gpui::test]
    async fn test_ref_chips_fold_in_rows_and_expand_in_the_detail_panel(cx: &mut TestAppContext) {
        let sha = Oid::from_bytes(&[5; 20]).unwrap();
        let ref_names = (0..40)
            .map(|number| SharedString::from(format!("release/{number}")))
            .collect();
        let commits = vec![Arc::new(InitialGraphCommitData {
            sha,
            parents: smallvec![],
            ref_names,
        })];
        let (_fs, _repository, multi_workspace, git_graph, cx) = open_graph(commits, cx).await;

        assert!(
            cx.debug_bounds("hidden-ref-chips-0").is_some(),
            "the row folds all but the first chips into one"
        );
        assert!(cx.debug_bounds("git-graph-show-all-ref-chips").is_none());

        git_graph.update_in(cx, |graph, window, cx| {
            graph.select_first(&menu::SelectFirst, window, cx);
        });
        cx.run_until_parked();
        draw_workspace(&multi_workspace, cx);
        let show_all = cx
            .debug_bounds("git-graph-show-all-ref-chips")
            .expect("the detail panel offers the chips past its limit");
        cx.simulate_click(show_all.center(), Modifiers::none());
        cx.run_until_parked();
        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(graph.detail_ref_chips_expanded_for, Some(sha));
        });
        draw_workspace(&multi_workspace, cx);
        assert!(cx.debug_bounds("git-graph-show-all-ref-chips").is_none());
    }

    #[gpui::test]
    async fn test_loose_refs_banner_offers_to_pack_them(cx: &mut TestAppContext) {
        let commits = generate_random_commit_dag(&mut StdRng::seed_from_u64(3), 3, false);
        let (fs, repository, multi_workspace, _git_graph, cx) = open_graph(commits, cx).await;
        let set_loose_refs = |count: Option<usize>, cx: &mut VisualTestContext| {
            repository.update(cx, |repository, cx| {
                repository.apply_ref_storage_scan_for_test(
                    git::repository::RefStorageScan {
                        loose_refs: count.map(|count| git::repository::LooseRefCount {
                            count,
                            complete: true,
                        }),
                        tags_fingerprint: None,
                    },
                    cx,
                )
            });
            draw_workspace(&multi_workspace, cx);
        };

        assert!(cx.debug_bounds("git-graph-loose-refs-banner").is_none());
        set_loose_refs(
            Some(crate::many_refs::LOOSE_REFS_SUGGESTION_THRESHOLD - 1),
            cx,
        );
        assert!(cx.debug_bounds("git-graph-loose-refs-banner").is_none());
        set_loose_refs(None, cx);
        assert!(cx.debug_bounds("git-graph-loose-refs-banner").is_none());
        set_loose_refs(Some(crate::many_refs::LOOSE_REFS_SUGGESTION_THRESHOLD), cx);
        assert!(cx.debug_bounds("git-graph-loose-refs-banner").is_some());

        fs.with_git_state(Path::new("/project/.git"), false, |state| {
            state.command_outputs.insert(
                "pack-refs --all".into(),
                git::repository::GitCommandOutput {
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: Some(0),
                },
            );
        })
        .unwrap();
        let stage = |cx: &mut VisualTestContext| {
            cx.update(|_, cx| crate::many_refs::pack_refs_stage(&repository, cx))
        };
        let click_pack = |cx: &mut VisualTestContext| {
            let pack = cx
                .debug_bounds("git-graph-pack-refs")
                .expect("the banner's button");
            cx.simulate_click(pack.center(), Modifiers::none());
            cx.run_until_parked();
            draw_workspace(&multi_workspace, cx);
        };

        // Packing holds up every other git operation for a while, so it is asked about first,
        // and cancelling runs nothing.
        click_pack(cx);
        assert!(cx.has_pending_prompt());
        assert_eq!(stage(cx), Some(crate::many_refs::PackRefsStage::Confirming));
        assert!(git_invocations(&fs, "pack-refs").is_empty());
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
        assert!(!cx.has_pending_prompt());
        assert_eq!(stage(cx), None);
        assert!(git_invocations(&fs, "pack-refs").is_empty());

        // A second click, or the palette, while the question is open asks nothing more, and the
        // one answer runs `git pack-refs` once.
        click_pack(cx);
        click_pack(cx);
        cx.dispatch_action(git::PackRefs);
        cx.run_until_parked();
        cx.simulate_prompt_answer("Pack");
        cx.run_until_parked();
        assert!(!cx.has_pending_prompt(), "only one question was asked");
        assert_eq!(
            git_invocations(&fs, "pack-refs"),
            [vec!["pack-refs".to_string(), "--all".to_string()]]
        );
        assert_eq!(stage(cx), None, "done packing");
        // The ref scan queued behind the packing counts again (the fake repository reads no ref
        // files, so its count is unknown and the banner goes).
        draw_workspace(&multi_workspace, cx);
        assert!(cx.debug_bounds("git-graph-loose-refs-banner").is_none());
        set_loose_refs(Some(crate::many_refs::LOOSE_REFS_SUGGESTION_THRESHOLD), cx);

        // While packing runs, the button cannot start another one.
        cx.update(|_, cx| {
            crate::many_refs::set_pack_refs_stage(
                repository.entity_id(),
                Some(crate::many_refs::PackRefsStage::Packing),
                cx,
            )
        });
        draw_workspace(&multi_workspace, cx);
        click_pack(cx);
        assert!(!cx.has_pending_prompt());
        cx.update(|_, cx| crate::many_refs::set_pack_refs_stage(repository.entity_id(), None, cx));

        // `git::PackRefs` from the palette goes through the same question.
        cx.dispatch_action(git::PackRefs);
        cx.run_until_parked();
        assert!(cx.has_pending_prompt());
        cx.simulate_prompt_answer("Pack");
        cx.run_until_parked();
        assert_eq!(git_invocations(&fs, "pack-refs").len(), 2);
    }

    /// Right after packing, the count in hand is the one from before (over SSH the host's
    /// updates can keep carrying it for a while), and offering to pack again would be wrong.
    #[gpui::test]
    async fn test_loose_refs_banner_waits_for_a_count_taken_after_packing(cx: &mut TestAppContext) {
        let commits = generate_random_commit_dag(&mut StdRng::seed_from_u64(4), 3, false);
        let (_fs, repository, multi_workspace, _git_graph, cx) = open_graph(commits, cx).await;
        let count = |count: usize| git::repository::LooseRefCount {
            count,
            complete: true,
        };
        let set_loose_refs = |count: git::repository::LooseRefCount, cx: &mut VisualTestContext| {
            repository.update(cx, |repository, cx| {
                repository.apply_ref_storage_scan_for_test(
                    git::repository::RefStorageScan {
                        loose_refs: Some(count),
                        tags_fingerprint: None,
                    },
                    cx,
                )
            });
            draw_workspace(&multi_workspace, cx);
        };
        let banner_shown =
            |cx: &mut VisualTestContext| cx.debug_bounds("git-graph-loose-refs-banner").is_some();

        set_loose_refs(count(5_000), cx);
        assert!(banner_shown(cx));
        cx.update(|_, cx| {
            crate::many_refs::remember_count_before_packing(
                repository.entity_id(),
                count(5_000),
                cx,
            )
        });
        draw_workspace(&multi_workspace, cx);
        assert!(!banner_shown(cx), "packed; the count is from before");
        set_loose_refs(count(5_000), cx);
        assert!(!banner_shown(cx), "an update still carrying the old count");

        // A count taken after packing: shown again when it is still over the threshold.
        set_loose_refs(count(1_200), cx);
        assert!(banner_shown(cx));
        draw_workspace(&multi_workspace, cx);
        assert_eq!(
            cx.update(|_, cx| crate::many_refs::awaiting_recount(&repository, cx)),
            None,
            "the record goes once a new count is in"
        );
        set_loose_refs(count(5_000), cx);
        assert!(
            banner_shown(cx),
            "the old number coming back is a real count"
        );
    }

    #[gpui::test]
    async fn test_tag_picker_searches_every_tag_and_jumps_to_its_commit(cx: &mut TestAppContext) {
        let commits = generate_random_commit_dag(&mut StdRng::seed_from_u64(5), 8, false);
        let newest = commits[0].sha;
        let target = commits[6].sha;
        let (fs, repository, multi_workspace, git_graph, cx) = open_graph(commits, cx).await;
        let workspace = multi_workspace.read_with(&*cx, |multi, _| multi.workspace().clone());

        // More tags than the old menu's 50, with the one looked for the oldest of them.
        let mut tags = (0..59)
            .map(|number| (format!("build-{number:03}"), newest))
            .collect::<Vec<_>>();
        tags.push(("v0.1.0".to_string(), target));
        let tags = tags
            .iter()
            .map(|(name, sha)| (name.as_str(), *sha))
            .collect::<Vec<_>>();
        set_tag_list(&fs, &tags);
        repository.update(cx, |repository, cx| {
            repository.apply_ref_storage_scan_for_test(
                git::repository::RefStorageScan {
                    loose_refs: None,
                    tags_fingerprint: Some(1),
                },
                cx,
            )
        });
        cx.run_until_parked();

        git_graph.update_in(cx, |graph, window, cx| graph.open_tag_picker(window, cx));
        cx.run_until_parked();
        let picker = workspace.update(cx, |workspace, cx| {
            workspace
                .active_modal::<crate::many_refs::GraphTagPicker>(cx)
                .expect("the tag picker is open")
                .read(cx)
                .picker
                .clone()
        });
        picker.read_with(&*cx, |picker, _| {
            assert_eq!(picker.delegate.match_count(), 60, "every tag is listed");
        });

        picker.update_in(cx, |picker, window, cx| {
            picker.set_query("v0.1", window, cx)
        });
        cx.run_until_parked();
        picker.read_with(&*cx, |picker, _| {
            assert!(picker.delegate.match_count() >= 1);
        });
        cx.dispatch_action(menu::Confirm);
        cx.run_until_parked();

        git_graph.read_with(&*cx, |graph, _| {
            let selected = graph
                .selected_entry_idx
                .and_then(|index| graph.graph_data.commits.get(index))
                .map(|commit| commit.data.sha);
            assert_eq!(selected, Some(target));
        });
        workspace.update(cx, |workspace, cx| {
            assert!(
                workspace
                    .active_modal::<crate::many_refs::GraphTagPicker>(cx)
                    .is_none()
            );
        });
    }

    #[gpui::test]
    async fn test_comparison_offers_tags_by_their_full_ref_name(cx: &mut TestAppContext) {
        let commits = generate_random_commit_dag(&mut StdRng::seed_from_u64(9), 3, false);
        let tagged = commits[1].sha;
        let (fs, repository, _multi_workspace, git_graph, cx) = open_graph(commits, cx).await;
        // A branch and a tag of the same name, which the tag list no longer tells apart by name.
        repository.update(cx, |repository, cx| {
            repository.set_branch_list_for_test(
                vec![Branch {
                    is_head: true,
                    ref_name: "refs/heads/v1".into(),
                    upstream: None,
                    most_recent_commit: None,
                }],
                cx,
            )
        });
        set_tag_list(&fs, &[("v1", tagged)]);
        repository.update(cx, |repository, cx| {
            repository.apply_ref_storage_scan_for_test(
                git::repository::RefStorageScan {
                    loose_refs: None,
                    tags_fingerprint: Some(1),
                },
                cx,
            )
        });
        cx.run_until_parked();

        let refs = git_graph.read_with(&*cx, |graph, cx| graph.comparable_refs(cx));
        assert_eq!(
            refs,
            [SharedString::from("v1"), SharedString::from("refs/tags/v1")]
        );
    }

    #[test]
    fn test_commit_dates_follow_the_display_language() {
        let _japanese = i18n::test_support::LocaleOverride::new(Some(
            i18n_catalog::test_support::bundled_catalog("ja"),
        ));
        let datetime =
            OffsetDateTime::from_unix_timestamp(1_790_682_120).expect("a valid timestamp");
        assert_eq!(
            format_local_datetime(datetime, UtcOffset::UTC),
            "2026年9月29日 11:42"
        );
    }

    #[gpui::test]
    async fn test_date_column_is_widened_to_fit_a_date(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;
        let mut rng = StdRng::seed_from_u64(7);
        fs.set_graph_commits(
            Path::new("/project/.git"),
            generate_random_commit_dag(&mut rng, 5, false),
        );

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.run_until_parked();
        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });
        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace_weak =
            multi_workspace.read_with(&*cx, |multi, _| multi.workspace().downgrade());
        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace_weak,
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();

        // The first frame learns the table's width; the next one fits the column to it.
        for _ in 0..2 {
            cx.draw(
                point(px(0.), px(0.)),
                gpui::size(px(600.), px(800.)),
                |_, _| git_graph.clone().into_any_element(),
            );
            cx.run_until_parked();
        }

        let (date_width, min_width) = cx.update(|window, cx| {
            let date_width = git_graph
                .read(cx)
                .column_widths
                .read(cx)
                .preview_column_width(2, window);
            (date_width, date_column_min_width(window, cx))
        });
        let date_width = date_width.expect("the table's width is known after drawing");
        let min_width = min_width.expect("a date can be measured");
        // The default share of a 600px table is far too narrow for a date.
        assert!(min_width > px(600.) * 0.1032, "{min_width:?}");
        assert!(
            date_width + px(0.5) >= min_width,
            "the date column is {date_width:?} wide but a date needs {min_width:?}"
        );
    }
}
