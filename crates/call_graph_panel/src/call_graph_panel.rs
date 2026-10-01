mod call_graph_panel_settings;
mod call_source;
mod call_tree;

#[cfg(test)]
mod call_graph_panel_tests;

use std::{
    collections::VecDeque,
    ops::Range,
    time::{Duration, Instant},
};

use anyhow::Context as _;
use collections::HashMap;
use db::kvp::KeyValueStore;
use editor::{Editor, EditorEvent, SelectionEffects, scroll::Autoscroll};
use futures::FutureExt as _;
use gpui::{
    Action, App, AsyncWindowContext, ClickEvent, ClipboardItem, Context, Entity, EventEmitter,
    FocusHandle, Focusable, KeyContext, ScrollHandle, ScrollStrategy, Subscription, Task,
    UniformListScrollHandle, WeakEntity, Window, actions, px, uniform_list,
};
use language::{Anchor, Buffer, ToPoint as _};
use project::{
    LspStoreEvent, Project, SymbolIndexStoreEvent, SymbolIndexUnavailable, WorktreeId,
    symbol_index_store::SymbolIndexStatus,
};
use serde::{Deserialize, Serialize};
use settings::{DockSide, Settings};
use ui::{
    CommonAnimationExt, ContextMenu, ListItem, PopoverMenu, TintColor, Tooltip, WithScrollbar,
    prelude::*,
};
use util::{ResultExt as _, TryFutureExt as _};
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

pub use call_graph_panel_settings::CallGraphPanelSettings;
pub use call_source::{
    CallChildren, CallDirection, CallOrigin, CallSymbol, ChildrenFetch, FetchOptions, IndexAnswer,
    LanguageServerAnswer, LocationPoints, RootOutcome, SourceFilter, SymbolLocation, SymbolRole,
    combine, fetch_children, prepare_root, resolve_location, start_fetch,
};
use call_tree::{CallTree, ChildrenState, NodeId, Row};
pub use zed_actions::call_graph_panel::{ShowCallees, ShowCallers, ToggleFocus};

actions!(
    call_graph_panel,
    [
        /// Toggles the call graph panel.
        Toggle,
        /// Switches between showing callers and callees of the root.
        ToggleDirection,
        /// Asks again for everything shown, keeping the root.
        Refresh,
        /// Expands the selected function, or moves into its first child.
        ExpandSelected,
        /// Collapses the selected function, or moves to its parent.
        CollapseSelected,
        /// Collapses every function under the root.
        CollapseAll,
        /// Opens the selected call in the editor. Repeating it on a row with
        /// several calls goes to the next one.
        OpenSelected,
        /// Shows the selected call in the editor, keeping focus in the panel.
        PreviewSelected,
        /// Copies the name of the selected function.
        CopySymbolName,
        /// Makes the function under the editor's cursor the root.
        InspectCursor,
        /// Goes back to the previous root.
        PreviousRoot,
        /// Goes forward to the next root.
        NextRoot,
        /// Toggles whether the root follows the editor's cursor.
        ToggleFollowCursor,
    ]
);

const CALL_GRAPH_PANEL_KEY: &str = "CallGraphPanel";
const HISTORY_LIMIT: usize = 50;
const FOLLOW_CURSOR_DEBOUNCE: Duration = Duration::from_millis(400);
/// clangd reports finished work many times a second while it indexes, so empty
/// answers are asked for again at most this often, and each one waits twice as
/// long as before, up to 2^MAX_RETRY_DOUBLINGS intervals, before it is asked
/// for again.
const INDEXING_RETRY_INTERVAL: Duration = Duration::from_secs(1);
const MAX_RETRY_DOUBLINGS: u32 = 5;
const INDENT_STEP: f32 = 12.;
/// Files opened again to ask a language server are kept open this long, and
/// at most this many, so that expanding rows of the same file does not open
/// and close it each time, while a deep tree still does not keep its files.
const RECENT_BUFFER_LIMIT: usize = 8;
const RECENT_BUFFER_LIFETIME: Duration = Duration::from_secs(30);
const MAX_ROOT_CANDIDATES: usize = 200;

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<CallGraphPanel>(window, cx);
        });
        workspace.register_action(|workspace, _: &Toggle, window, cx| {
            if !workspace.toggle_panel_focus::<CallGraphPanel>(window, cx) {
                workspace.close_panel::<CallGraphPanel>(window, cx);
            }
        });
        workspace.register_action(|workspace, _: &ShowCallers, window, cx| {
            show_calls_from_workspace(workspace, CallDirection::Incoming, window, cx);
        });
        workspace.register_action(|workspace, _: &ShowCallees, window, cx| {
            show_calls_from_workspace(workspace, CallDirection::Outgoing, window, cx);
        });
    })
    .detach();
}

fn show_calls_from_workspace(
    workspace: &mut Workspace,
    direction: CallDirection,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let editor = workspace.active_item_as::<Editor>(cx);
    let Some(panel) = workspace.focus_panel::<CallGraphPanel>(window, cx) else {
        return;
    };
    // The panel reads the workspace while it looks for the root, and the
    // workspace is being updated right now.
    window.defer(cx, move |window, cx| {
        panel.update(cx, |panel, cx| {
            panel.direction = direction;
            panel.serialize(cx);
            match editor {
                Some(editor) => panel.inspect_editor(editor, true, window, cx),
                None => {
                    panel.state = PanelState::NoEditor;
                    cx.notify();
                }
            }
        });
    });
}

#[derive(Serialize, Deserialize)]
struct SerializedCallGraphPanel {
    direction: Option<CallDirection>,
}

#[derive(Clone, Debug, PartialEq)]
enum PanelState {
    /// No root has been asked for yet.
    Idle,
    LoadingRoot,
    NoEditor,
    NoSymbol,
    NoLanguageServer,
    Unsupported,
    RootFailed(SharedString),
    /// The symbol index found several functions by the name at the cursor;
    /// they are in `root_candidates`.
    ChooseRoot,
    Ready,
}

/// A request for a root that is kept so that it can be retried when a
/// language server starts or finishes indexing.
struct PendingRoot {
    buffer: WeakEntity<Buffer>,
    position: Anchor,
}

/// Buffers opened again to ask a language server, kept open for a while.
#[derive(Default)]
struct RecentBuffers {
    /// Oldest first, each with when it is released.
    entries: VecDeque<(Entity<Buffer>, Instant)>,
}

impl RecentBuffers {
    fn keep(&mut self, buffer: Entity<Buffer>, now: Instant) {
        self.entries
            .retain(|(kept, _)| kept.entity_id() != buffer.entity_id());
        self.entries
            .push_back((buffer, now + RECENT_BUFFER_LIFETIME));
        while self.entries.len() > RECENT_BUFFER_LIMIT {
            self.entries.pop_front();
        }
    }

    /// Releases the files of a folder removed from the project, which would
    /// otherwise keep its worktree alive until they are due.
    fn release_worktree(&mut self, worktree_id: WorktreeId, cx: &App) {
        self.entries.retain(|(buffer, _)| {
            buffer
                .read(cx)
                .file()
                .is_none_or(|file| file.worktree_id(cx) != worktree_id)
        });
    }

    /// Releases what is due and returns when the next one is.
    fn release_due(&mut self, now: Instant) -> Option<Instant> {
        self.entries.retain(|(_, release_at)| *release_at > now);
        self.entries.iter().map(|(_, release_at)| *release_at).min()
    }
}

/// What the status line says about the symbol index.
#[derive(Clone, Debug, PartialEq)]
enum IndexSummary {
    Building { done: usize, total: usize },
    Preparing,
    Unavailable(SymbolIndexUnavailable),
    Disabled,
    Ready { files: u64, definitions: u64 },
}

/// Sums up the statuses of the project's worktrees: one still being built
/// matters most, then one that cannot be used.
fn index_summary(statuses: &[SymbolIndexStatus]) -> Option<IndexSummary> {
    let mut building = None;
    let mut preparing = false;
    let mut unavailable = None;
    let mut ready: Option<(u64, u64)> = None;
    for status in statuses {
        match status {
            SymbolIndexStatus::Indexing { done, total } => {
                let (all_done, all_total) = building.unwrap_or((0, 0));
                building = Some((all_done + done, all_total + total));
            }
            SymbolIndexStatus::Scanning => preparing = true,
            SymbolIndexStatus::Unavailable { reason } => {
                unavailable.get_or_insert_with(|| reason.clone());
            }
            SymbolIndexStatus::Ready { stats } => {
                let (files, definitions) = ready.unwrap_or((0, 0));
                ready = Some((files + stats.files, definitions + stats.definitions));
            }
            SymbolIndexStatus::Disabled => {}
        }
    }
    if let Some((done, total)) = building {
        Some(IndexSummary::Building { done, total })
    } else if preparing {
        Some(IndexSummary::Preparing)
    } else if let Some(reason) = unavailable {
        Some(IndexSummary::Unavailable(reason))
    } else if let Some((files, definitions)) = ready {
        Some(IndexSummary::Ready { files, definitions })
    } else if statuses.is_empty() {
        None
    } else {
        Some(IndexSummary::Disabled)
    }
}

/// `12345` as `12,345`.
fn group_digits(number: impl ToString) -> String {
    let digits = number.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

pub struct CallGraphPanel {
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    filter_editor: Entity<Editor>,
    scroll_handle: UniformListScrollHandle,
    direction: CallDirection,
    follow_cursor: bool,
    sources: SourceFilter,
    tree: CallTree,
    rows: Vec<Row>,
    selected: Option<Row>,
    state: PanelState,
    history: Vec<CallSymbol>,
    history_index: usize,
    fetch_tasks: HashMap<NodeId, Task<()>>,
    root_task: Option<Task<()>>,
    pending_root: Option<PendingRoot>,
    follow_task: Option<Task<()>>,
    indexing_retry_task: Option<Task<()>>,
    /// The node the last "open" went to and the call site the next one goes to.
    site_cycle: Option<(NodeId, usize)>,
    open_task: Option<Task<()>>,
    root_candidates: Vec<CallSymbol>,
    /// The index, in the listed candidates, of the one Enter makes the root.
    selected_root_candidate: usize,
    root_candidates_scroll: ScrollHandle,
    recent_buffers: RecentBuffers,
    buffer_release_task: Option<Task<()>>,
    /// Whether the symbol index was being built when its status last changed,
    /// so that finishing asks again for what it answered in the meantime.
    index_was_building: bool,
    active_editor_subscription: Option<Subscription>,
    pending_serialization: Task<Option<()>>,
    _subscriptions: Vec<Subscription>,
}

impl CallGraphPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> anyhow::Result<Entity<Self>> {
        let serialized_panel = match workspace
            .read_with(&cx, |workspace, _| Self::serialization_key(workspace))
            .ok()
            .flatten()
        {
            Some(serialization_key) => {
                let kvp = cx.update(|_, cx| KeyValueStore::global(cx))?;
                cx.background_spawn(async move { kvp.read_kvp(&serialization_key) })
                    .await
                    .context("loading call graph panel")
                    .log_err()
                    .flatten()
                    .map(|panel| serde_json::from_str::<SerializedCallGraphPanel>(&panel))
                    .transpose()
                    .log_err()
                    .flatten()
            }
            None => None,
        };

        workspace.update_in(&mut cx, |workspace, window, cx| {
            let panel = Self::new(workspace, window, cx);
            if let Some(direction) = serialized_panel.and_then(|panel| panel.direction) {
                panel.update(cx, |panel, _| panel.direction = direction);
            }
            panel
        })
    }

    pub fn new(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        let project = workspace.project().clone();
        let workspace_handle = cx.entity().downgrade();
        let workspace_entity = cx.entity();
        cx.new(|cx| {
            let filter_editor = cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_placeholder_text(i18n::t!("Filter shown functions…"), window, cx);
                editor
            });
            let filter_subscription =
                cx.subscribe(&filter_editor, |panel: &mut Self, _, event, cx| {
                    if let EditorEvent::BufferEdited = event {
                        panel.update_rows(cx);
                    }
                });
            let project_subscription =
                cx.subscribe_in(&project, window, |panel, _, event, window, cx| {
                    if let project::Event::WorktreeRemoved(worktree_id) = event {
                        panel.recent_buffers.release_worktree(*worktree_id, cx);
                    }
                    if let project::Event::LanguageServerBufferRegistered { buffer_id, .. } = event
                        && panel.pending_root.as_ref().is_some_and(|pending| {
                            pending
                                .buffer
                                .upgrade()
                                .is_some_and(|buffer| buffer.read(cx).remote_id() == *buffer_id)
                        })
                    {
                        panel.retry_pending_root(true, window, cx);
                    }
                });
            let lsp_store_subscription = cx.subscribe_in(
                &project.read(cx).lsp_store(),
                window,
                |panel, _, event, window, cx| {
                    if let LspStoreEvent::LanguageServerUpdate {
                        message: proto::update_language_server::Variant::WorkEnd(_),
                        ..
                    } = event
                    {
                        panel.note_work_ended(window, cx);
                    }
                },
            );
            let workspace_subscription = cx.subscribe_in(
                &workspace_entity,
                window,
                |panel, workspace, event, window, cx| {
                    if let workspace::Event::ActiveItemChanged = event {
                        let editor = workspace.read(cx).active_item_as::<Editor>(cx);
                        panel.watch_active_editor(editor, window, cx);
                    }
                },
            );
            let index_store = project.read(cx).symbol_index_store().clone();
            let index_subscription =
                cx.subscribe_in(&index_store, window, |panel, _, event, window, cx| {
                    let SymbolIndexStoreEvent::StatusChanged(_) = event;
                    panel.index_status_changed(window, cx);
                });
            let mut follow_cursor_setting = CallGraphPanelSettings::get_global(cx).follow_cursor;
            let mut sources_setting = CallGraphPanelSettings::get_global(cx).sources;
            let mut excluded_setting = CallGraphPanelSettings::get_global(cx)
                .excluded_callees
                .clone();
            let settings_subscription = cx.observe_global_in::<settings::SettingsStore>(
                window,
                move |panel: &mut Self, window, cx| {
                    let settings = CallGraphPanelSettings::get_global(cx);
                    let new_follow_cursor = settings.follow_cursor;
                    let new_sources = settings.sources;
                    let excluded_changed = settings.excluded_callees != excluded_setting;
                    if excluded_changed {
                        excluded_setting = settings.excluded_callees.clone();
                    }
                    if new_follow_cursor != follow_cursor_setting {
                        follow_cursor_setting = new_follow_cursor;
                        panel.follow_cursor = new_follow_cursor;
                    }
                    if new_sources != sources_setting {
                        sources_setting = new_sources;
                        panel.set_sources(new_sources.into(), window, cx);
                    } else if excluded_changed && panel.direction == CallDirection::Outgoing {
                        panel.rebuild_from_root(window, cx);
                    }
                    cx.notify();
                },
            );

            let mut panel = Self {
                project,
                workspace: workspace_handle,
                focus_handle: cx.focus_handle(),
                filter_editor,
                scroll_handle: UniformListScrollHandle::new(),
                direction: CallDirection::default(),
                follow_cursor: follow_cursor_setting,
                sources: sources_setting.into(),
                tree: CallTree::default(),
                rows: Vec::new(),
                selected: None,
                state: PanelState::Idle,
                history: Vec::new(),
                history_index: 0,
                fetch_tasks: HashMap::default(),
                root_task: None,
                pending_root: None,
                follow_task: None,
                indexing_retry_task: None,
                site_cycle: None,
                open_task: None,
                root_candidates: Vec::new(),
                selected_root_candidate: 0,
                root_candidates_scroll: ScrollHandle::new(),
                recent_buffers: RecentBuffers::default(),
                buffer_release_task: None,
                index_was_building: false,
                active_editor_subscription: None,
                pending_serialization: Task::ready(None),
                _subscriptions: vec![
                    filter_subscription,
                    project_subscription,
                    lsp_store_subscription,
                    workspace_subscription,
                    index_subscription,
                    settings_subscription,
                ],
            };
            panel.index_was_building = panel.index_is_building(cx);
            panel.watch_active_editor(workspace.active_item_as::<Editor>(cx), window, cx);
            panel
        })
    }

    fn serialization_key(workspace: &Workspace) -> Option<String> {
        workspace
            .database_id()
            .map(|id| i64::from(id).to_string())
            .or(workspace.session_id())
            .map(|id| format!("{}-{:?}", CALL_GRAPH_PANEL_KEY, id))
    }

    fn serialize(&mut self, cx: &mut Context<Self>) {
        let Some(serialization_key) = self
            .workspace
            .read_with(cx, |workspace, _| Self::serialization_key(workspace))
            .ok()
            .flatten()
        else {
            return;
        };
        let direction = Some(self.direction);
        let kvp = KeyValueStore::global(cx);
        self.pending_serialization = cx.background_spawn(
            async move {
                kvp.write_kvp(
                    serialization_key,
                    serde_json::to_string(&SerializedCallGraphPanel { direction })?,
                )
                .await?;
                anyhow::Ok(())
            }
            .log_err(),
        );
    }

    pub fn direction(&self) -> CallDirection {
        self.direction
    }

    pub fn root_symbol(&self) -> Option<&CallSymbol> {
        let root = self.tree.root()?;
        Some(&self.tree.node(root)?.symbol)
    }

    fn watch_active_editor(
        &mut self,
        editor: Option<Entity<Editor>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.active_editor_subscription = editor.map(|editor| {
            cx.subscribe_in(&editor, window, |panel, editor, event, window, cx| {
                if let EditorEvent::SelectionsChanged { local: true } = event
                    && panel.follow_cursor
                {
                    panel.schedule_follow(editor.downgrade(), window, cx);
                }
            })
        });
    }

    fn schedule_follow(
        &mut self,
        editor: WeakEntity<Editor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.follow_task = Some(cx.spawn_in(window, async move |panel, cx| {
            cx.background_executor().timer(FOLLOW_CURSOR_DEBOUNCE).await;
            let Some(editor) = editor.upgrade() else {
                return;
            };
            panel
                .update_in(cx, |panel, window, cx| {
                    if panel.follow_cursor {
                        panel.inspect_editor(editor, false, window, cx);
                    }
                })
                .ok();
        }));
    }

    /// Makes the function under `editor`'s cursor the root. An explicit request
    /// reports why nothing was found; following the cursor quietly keeps the
    /// current tree instead.
    pub fn inspect_editor(
        &mut self,
        editor: Entity<Editor>,
        explicit: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let editor = editor.read(cx);
        let head = editor.selections.newest_anchor().head();
        let position = editor.buffer().read(cx).text_anchor_for_position(head, cx);
        let Some((buffer, position)) = position else {
            if explicit {
                self.state = PanelState::NoSymbol;
                cx.notify();
            }
            return;
        };
        if !explicit
            && (!self.is_visible(cx)
                || self
                    .root_symbol()
                    .is_some_and(|root| root.encloses(&buffer, position, cx)))
        {
            return;
        }
        self.request_root(buffer, position, explicit, window, cx);
    }

    fn is_visible(&self, cx: &Context<Self>) -> bool {
        let Some(workspace) = self.workspace.upgrade() else {
            return false;
        };
        let panel_id = cx.entity_id();
        workspace.read(cx).all_docks().iter().any(|dock| {
            dock.read(cx)
                .visible_panels()
                .any(|panel| panel.panel_id() == panel_id)
        })
    }

    fn request_root(
        &mut self,
        buffer: Entity<Buffer>,
        position: Anchor,
        explicit: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pending_root = Some(PendingRoot {
            buffer: buffer.downgrade(),
            position,
        });
        if explicit {
            self.state = PanelState::LoadingRoot;
            cx.notify();
        }
        let task = call_source::prepare_root(&self.project, buffer, position, self.sources, cx);
        self.root_task = Some(cx.spawn_in(window, async move |panel, cx| {
            let outcome = task.await;
            panel
                .update_in(cx, |panel, window, cx| {
                    panel.apply_root_outcome(outcome, explicit, window, cx);
                })
                .ok();
        }));
    }

    fn apply_root_outcome(
        &mut self,
        outcome: anyhow::Result<RootOutcome>,
        explicit: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let state = match outcome {
            Ok(RootOutcome::Found(symbol)) => {
                self.pending_root = None;
                let same_root = self
                    .root_symbol()
                    .is_some_and(|root| root.same_symbol(&symbol));
                if !(same_root && !explicit) {
                    self.push_history(symbol.clone());
                    self.set_root(symbol, window, cx);
                }
                return;
            }
            Ok(RootOutcome::Candidates(candidates)) => {
                self.pending_root = None;
                if !explicit && self.tree.root().is_some() {
                    return;
                }
                self.clear_tree();
                self.root_candidates = candidates;
                self.state = PanelState::ChooseRoot;
                self.select_root_candidate(0, cx);
                return;
            }
            Ok(RootOutcome::NoSymbol) => PanelState::NoSymbol,
            Ok(RootOutcome::NoLanguageServer) => PanelState::NoLanguageServer,
            Ok(RootOutcome::Unsupported) => PanelState::Unsupported,
            Err(error) => {
                log::error!("call graph: failed to find the function under the cursor: {error:#}");
                PanelState::RootFailed(format!("{error:#}").into())
            }
        };
        if explicit || self.tree.root().is_none() {
            self.clear_tree();
            self.state = state;
            cx.notify();
        }
    }

    fn retry_pending_root(&mut self, explicit: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !matches!(
            self.state,
            PanelState::NoSymbol | PanelState::NoLanguageServer | PanelState::Unsupported
        ) {
            return;
        }
        let Some(pending) = self.pending_root.as_ref() else {
            return;
        };
        let Some(buffer) = pending.buffer.upgrade() else {
            return;
        };
        let position = pending.position;
        self.request_root(buffer, position, explicit, window, cx);
    }

    /// A language server finished some work; the rows it had answered empty
    /// may have answers now.
    fn note_work_ended(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.owe_retry(|node| node.language_server_empty);
        self.schedule_indexing_retry(INDEXING_RETRY_INTERVAL, window, cx);
    }

    fn owe_retry(&mut self, needs_retry: impl Fn(&call_tree::CallNode) -> bool) {
        for id in self.tree.node_ids() {
            if let Some(node) = self.tree.node_mut(id)
                && node.expanded
                && matches!(node.children, ChildrenState::Loaded { .. })
                && needs_retry(node)
            {
                node.retry_owed = true;
            }
        }
    }

    fn index_statuses(&self, cx: &App) -> Vec<SymbolIndexStatus> {
        self.project
            .read(cx)
            .symbol_index_store()
            .read(cx)
            .statuses(cx)
            .into_iter()
            .map(|(_, status)| status)
            .collect()
    }

    fn index_is_building(&self, cx: &App) -> bool {
        self.index_statuses(cx).iter().any(|status| {
            matches!(
                status,
                SymbolIndexStatus::Indexing { .. } | SymbolIndexStatus::Scanning
            )
        })
    }

    fn index_status_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let building = self.index_is_building(cx);
        if self.index_was_building && !building {
            self.note_index_finished(window, cx);
        }
        self.index_was_building = building;
        cx.notify();
    }

    /// The symbol index finished building; what it answered while it was
    /// being built may have been incomplete.
    fn note_index_finished(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(
            self.state,
            PanelState::NoLanguageServer | PanelState::Unsupported
        ) {
            self.retry_pending_root(false, window, cx);
        }
        self.owe_retry(|node| node.index_partial);
        self.schedule_indexing_retry(INDEXING_RETRY_INTERVAL, window, cx);
    }

    fn schedule_indexing_retry(
        &mut self,
        delay: Duration,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.indexing_retry_task.is_some() {
            return;
        }
        self.indexing_retry_task = Some(cx.spawn_in(window, async move |panel, cx| {
            cx.background_executor().timer(delay).await;
            panel
                .update_in(cx, |panel, window, cx| {
                    panel.indexing_retry_task = None;
                    panel.retry_after_indexing(window, cx);
                })
                .ok();
        }));
    }

    /// A source that was still indexing may have answered with nothing or
    /// with part of the answer; ask again once it reports that work ended.
    fn retry_after_indexing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.state == PanelState::NoSymbol {
            // Quietly, so that the message does not flicker to "looking" and back
            // every time the server reports progress.
            self.retry_pending_root(false, window, cx);
            return;
        }
        let now: Instant = cx.background_executor().now();
        let mut next_due: Option<Instant> = None;
        let mut due = Vec::new();
        for id in self.tree.node_ids() {
            let Some(node) = self.tree.node_mut(id) else {
                continue;
            };
            if !node.retry_owed
                || !node.expanded
                || !matches!(node.children, ChildrenState::Loaded { .. })
            {
                continue;
            }
            if let Some(not_before) = node.retry_not_before
                && not_before > now
            {
                next_due = Some(next_due.map_or(not_before, |due| due.min(not_before)));
                continue;
            }
            node.retry_owed = false;
            node.empty_retries = node.empty_retries.saturating_add(1);
            let doublings = u32::from(node.empty_retries).min(MAX_RETRY_DOUBLINGS);
            node.retry_not_before = Some(now + INDEXING_RETRY_INTERVAL * 2u32.pow(doublings));
            due.push(id);
        }
        for id in due {
            self.fetch_children_of(id, true, window, cx);
        }
        // The work that ended is still owed to the rows that had to wait.
        if let Some(due) = next_due {
            self.schedule_indexing_retry(due.saturating_duration_since(now), window, cx);
        }
    }

    fn push_history(&mut self, symbol: CallSymbol) {
        if let Some(current) = self.history.get_mut(self.history_index)
            && current.same_symbol(&symbol)
        {
            *current = symbol;
            return;
        }
        if !self.history.is_empty() {
            self.history.truncate(self.history_index + 1);
        }
        self.history.push(symbol);
        if self.history.len() > HISTORY_LIMIT {
            let excess = self.history.len() - HISTORY_LIMIT;
            self.history.drain(..excess);
        }
        self.history_index = self.history.len().saturating_sub(1);
    }

    fn clear_tree(&mut self) {
        self.tree.clear();
        self.fetch_tasks.clear();
        self.rows.clear();
        self.selected = None;
        self.site_cycle = None;
        self.root_candidates.clear();
    }

    fn set_root(&mut self, symbol: CallSymbol, window: &mut Window, cx: &mut Context<Self>) {
        self.fetch_tasks.clear();
        self.site_cycle = None;
        self.root_candidates.clear();
        let root = self.tree.set_root(symbol);
        self.state = PanelState::Ready;
        self.selected = Some(Row::Node { id: root, depth: 0 });
        self.expand(root, window, cx);
    }

    fn expand(&mut self, id: NodeId, window: &mut Window, cx: &mut Context<Self>) {
        self.fetch_children_of(id, false, window, cx);
    }

    /// Asks every source for the node's children. The symbol index usually
    /// answers first; its rows are shown at once and the language server's
    /// answer is merged into them when it comes, keeping the rows that stay.
    fn fetch_children_of(
        &mut self,
        id: NodeId,
        keep_children: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(node) = self.tree.node(id) else {
            return;
        };
        if !node.can_expand() {
            return;
        }
        let symbol = node.symbol.clone();
        let Some(generation) = self.tree.begin_fetch(id, keep_children) else {
            return;
        };
        self.forget_finished_fetches();
        let direction = self.direction;
        let options = FetchOptions {
            sources: self.sources,
            ..FetchOptions::from_settings(cx)
        };
        let ChildrenFetch {
            language_server,
            index,
        } = call_source::start_fetch(&self.project, &symbol, direction, &options, cx);
        let task = cx.spawn_in(window, async move |panel, cx| {
            let language_server_asked = language_server.is_some();
            let language_server = async move {
                match language_server {
                    Some(task) => Some(task.await),
                    None => None,
                }
            }
            .fuse();
            let index = async move {
                match index {
                    Some(task) => Some(task.await),
                    None => None,
                }
            }
            .fuse();
            futures::pin_mut!(language_server, index);
            let mut server_answer = None;
            let mut index_answer = None;
            while server_answer.is_none() || index_answer.is_none() {
                futures::select_biased! {
                    answer = language_server => server_answer = Some(answer),
                    answer = index => {
                        if language_server_asked
                            && server_answer.is_none()
                            && let Some(Ok(early)) = &answer
                            && !early.symbols.is_empty()
                        {
                            let early = call_source::combine(
                                direction,
                                None,
                                Some(Ok(early.clone())),
                                &options,
                            );
                            panel
                                .update(cx, |panel, cx| {
                                    panel.apply_children(id, generation, early, false, cx);
                                })
                                .ok();
                        }
                        index_answer = Some(answer);
                    }
                }
            }
            let children = call_source::combine(
                direction,
                server_answer.flatten(),
                index_answer.flatten(),
                &options,
            );
            panel
                .update(cx, |panel, cx| {
                    panel.fetch_tasks.remove(&id);
                    panel.apply_children(id, generation, children, true, cx);
                })
                .ok();
        });
        self.fetch_tasks.insert(id, task);
        self.update_rows(cx);
    }

    fn apply_children(
        &mut self,
        id: NodeId,
        generation: u64,
        children: anyhow::Result<CallChildren>,
        finished: bool,
        cx: &mut Context<Self>,
    ) {
        if !self.tree.is_current_fetch(id, generation) {
            return;
        }
        match children {
            Ok(mut children) => {
                self.keep_buffers(std::mem::take(&mut children.opened_buffers), cx);
                let limit = CallGraphPanelSettings::get_global(cx).max_children;
                self.tree.set_children(id, children, limit, finished);
            }
            Err(error) => {
                log::warn!("call graph: failed to fetch calls: {error:#}");
                self.tree.set_failed(id, format!("{error:#}").into());
            }
        }
        self.update_rows(cx);
    }

    fn keep_buffers(&mut self, buffers: Vec<Entity<Buffer>>, cx: &mut Context<Self>) {
        if buffers.is_empty() {
            return;
        }
        let now = cx.background_executor().now();
        for buffer in buffers {
            self.recent_buffers.keep(buffer, now);
        }
        self.schedule_buffer_release(cx);
    }

    fn schedule_buffer_release(&mut self, cx: &mut Context<Self>) {
        if self.buffer_release_task.is_some() {
            return;
        }
        let now = cx.background_executor().now();
        let Some(next) = self.recent_buffers.release_due(now) else {
            return;
        };
        self.buffer_release_task = Some(cx.spawn(async move |panel, cx| {
            cx.background_executor()
                .timer(next.saturating_duration_since(now))
                .await;
            panel
                .update(cx, |panel, cx| {
                    panel.buffer_release_task = None;
                    panel.schedule_buffer_release(cx);
                })
                .ok();
        }));
    }

    /// Drops the fetches of nodes that are gone, which cancels them.
    fn forget_finished_fetches(&mut self) {
        let tree = &self.tree;
        self.fetch_tasks.retain(|id, _| tree.node(*id).is_some());
    }

    fn collapse(&mut self, id: NodeId, cx: &mut Context<Self>) {
        self.fetch_tasks.remove(&id);
        self.tree.collapse(id);
        self.forget_finished_fetches();
        self.update_rows(cx);
    }

    fn toggle_node(&mut self, id: NodeId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(node) = self.tree.node(id) else {
            return;
        };
        if node.expanded {
            self.collapse(id, cx);
        } else {
            self.expand(id, window, cx);
        }
    }

    fn update_rows(&mut self, cx: &mut Context<Self>) {
        let filter = self.filter_editor.read(cx).text(cx);
        self.rows = self.tree.rows(&filter);
        self.selected = self
            .selected
            .and_then(|selected| self.shown_row_for(selected))
            .or_else(|| self.rows.first().copied());
        cx.notify();
    }

    /// The row to select in place of `selected`. A row that went away (a
    /// message row replaced by a spinner, a node hidden by the filter) hands
    /// the selection to its nearest shown ancestor rather than to the top of
    /// the tree.
    fn shown_row_for(&self, selected: Row) -> Option<Row> {
        if let Some(row) = self.rows.iter().find(|row| same_row(row, &selected)) {
            return Some(*row);
        }
        let mut ancestor = Some(selected.node());
        while let Some(id) = ancestor {
            let shown = self
                .rows
                .iter()
                .find(|row| matches!(row, Row::Node { id: row_id, .. } if *row_id == id));
            if let Some(row) = shown {
                return Some(*row);
            }
            ancestor = self.tree.node(id).and_then(|node| node.parent);
        }
        None
    }

    fn selected_index(&self) -> Option<usize> {
        let selected = self.selected?;
        self.rows.iter().position(|row| same_row(row, &selected))
    }

    fn select_index(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(row) = self.rows.get(index) {
            if self
                .site_cycle
                .is_some_and(|(cycled_node, _)| cycled_node != row.node())
            {
                self.site_cycle = None;
            }
            self.selected = Some(*row);
            self.scroll_handle
                .scroll_to_item(index, ScrollStrategy::Nearest);
            cx.notify();
        }
    }

    fn select_node(&mut self, id: NodeId, cx: &mut Context<Self>) {
        if let Some(index) = self
            .rows
            .iter()
            .position(|row| matches!(row, Row::Node { id: row_id, .. } if *row_id == id))
        {
            self.select_index(index, cx);
        }
    }

    fn choosing_root(&self) -> bool {
        self.state == PanelState::ChooseRoot
    }

    /// The candidates the list shows, and how many more it does not list.
    fn listed_root_candidates(&self) -> (&[CallSymbol], usize) {
        let shown = self.root_candidates.len().min(MAX_ROOT_CANDIDATES);
        let listed = self.root_candidates.get(..shown).unwrap_or_default();
        (listed, self.root_candidates.len() - listed.len())
    }

    fn select_root_candidate(&mut self, index: usize, cx: &mut Context<Self>) {
        let (listed, _) = self.listed_root_candidates();
        let Some(last) = listed.len().checked_sub(1) else {
            self.selected_root_candidate = 0;
            cx.notify();
            return;
        };
        self.selected_root_candidate = index.min(last);
        // The list's first child is its heading.
        self.root_candidates_scroll
            .scroll_to_item(self.selected_root_candidate + 1);
        cx.notify();
    }

    /// Closes the candidates and goes back to the root shown before them.
    fn dismiss_root_candidates(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.root_candidates.clear();
        self.selected_root_candidate = 0;
        if self.history.get(self.history_index).is_some() {
            self.go_to_history(self.history_index, window, cx);
        } else {
            self.clear_tree();
            self.state = PanelState::Idle;
            cx.notify();
        }
    }

    fn select_next(&mut self, _: &menu::SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        if self.choosing_root() {
            self.select_root_candidate(self.selected_root_candidate + 1, cx);
            return;
        }
        let next = self.selected_index().map_or(0, |index| index + 1);
        if next < self.rows.len() {
            self.select_index(next, cx);
        }
    }

    fn select_previous(
        &mut self,
        _: &menu::SelectPrevious,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.choosing_root() {
            self.select_root_candidate(self.selected_root_candidate.saturating_sub(1), cx);
            return;
        }
        if let Some(index) = self.selected_index()
            && index > 0
        {
            self.select_index(index - 1, cx);
        }
    }

    fn select_first(&mut self, _: &menu::SelectFirst, _: &mut Window, cx: &mut Context<Self>) {
        if self.choosing_root() {
            self.select_root_candidate(0, cx);
            return;
        }
        self.select_index(0, cx);
    }

    fn select_last(&mut self, _: &menu::SelectLast, _: &mut Window, cx: &mut Context<Self>) {
        if self.choosing_root() {
            self.select_root_candidate(usize::MAX, cx);
            return;
        }
        if let Some(last) = self.rows.len().checked_sub(1) {
            self.select_index(last, cx);
        }
    }

    fn expand_selected(&mut self, _: &ExpandSelected, window: &mut Window, cx: &mut Context<Self>) {
        match self.selected {
            Some(Row::Node { id, .. }) => {
                let Some(node) = self.tree.node(id) else {
                    return;
                };
                if node.expanded {
                    if let Some(first_child) = self.tree.children(id).first().copied() {
                        self.select_node(first_child, cx);
                    }
                } else {
                    self.expand(id, window, cx);
                }
            }
            Some(row) => self.activate_message_row(row, window, cx),
            None => {}
        }
    }

    fn collapse_selected(&mut self, _: &CollapseSelected, _: &mut Window, cx: &mut Context<Self>) {
        let Some(selected) = self.selected else {
            return;
        };
        if let Row::Node { id, .. } = selected
            && self.tree.node(id).is_some_and(|node| node.expanded)
        {
            self.collapse(id, cx);
            return;
        }
        let parent = match selected {
            Row::Node { id, .. } => self.tree.node(id).and_then(|node| node.parent),
            other => Some(other.node()),
        };
        if let Some(parent) = parent {
            self.select_node(parent, cx);
        }
    }

    fn collapse_all(&mut self, _: &CollapseAll, _: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.tree.root() else {
            return;
        };
        for child in self.tree.children(root).to_vec() {
            self.tree.collapse(child);
        }
        self.forget_finished_fetches();
        self.selected = Some(Row::Node { id: root, depth: 0 });
        self.update_rows(cx);
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if self.choosing_root() {
            self.choose_root(self.selected_root_candidate, window, cx);
            return;
        }
        self.open_selected(&OpenSelected, window, cx);
    }

    fn open_selected(&mut self, _: &OpenSelected, window: &mut Window, cx: &mut Context<Self>) {
        match self.selected {
            Some(Row::Node { id, .. }) => self.open_node(id, true, window, cx),
            Some(row) => self.activate_message_row(row, window, cx),
            None => {}
        }
    }

    fn preview_selected(
        &mut self,
        _: &PreviewSelected,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(Row::Node { id, .. }) = self.selected {
            self.open_node(id, false, window, cx);
        }
    }

    fn activate_message_row(&mut self, row: Row, window: &mut Window, cx: &mut Context<Self>) {
        match row {
            Row::ShowMore { parent, .. } => {
                let limit = CallGraphPanelSettings::get_global(cx).max_children;
                self.tree.show_more(parent, limit);
                self.update_rows(cx);
            }
            Row::Excluded { parent, .. } => {
                self.tree.show_excluded(parent);
                self.update_rows(cx);
            }
            Row::Error { parent, .. } => self.expand(parent, window, cx),
            Row::Node { .. } | Row::Empty { .. } | Row::Truncated { .. } => {}
        }
    }

    /// Opens the node's call site, or its definition when there is no call site.
    /// Opening the same row again goes to its next call site.
    fn open_node(&mut self, id: NodeId, focus: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(node) = self.tree.node(id) else {
            return;
        };
        let site_index = match self.site_cycle {
            Some((cycled_node, next_site)) if cycled_node == id => next_site,
            _ => 0,
        };
        let location = node.symbol.jump_target(site_index).clone();
        self.site_cycle = Some((id, (site_index + 1) % node.symbol.site_count()));
        cx.notify();

        let project = self.project.clone();
        let workspace = self.workspace.clone();
        self.open_task = Some(cx.spawn_in(window, async move |_, cx| {
            let location = match call_source::resolve_location(&project, &location, cx).await {
                Ok(location) => location,
                Err(error) => {
                    log::error!("call graph: failed to open the call: {error:#}");
                    workspace
                        .update(cx, |workspace, cx| workspace.show_error(error, cx))
                        .ok();
                    return;
                }
            };
            workspace
                .update_in(cx, |workspace, window, cx| {
                    let position = location
                        .range
                        .start
                        .to_point(&location.buffer.read(cx).snapshot());
                    let editor = workspace.open_project_item::<Editor>(
                        None,
                        location.buffer,
                        true,
                        focus,
                        true,
                        true,
                        window,
                        cx,
                    );
                    editor.update(cx, |editor, cx| {
                        editor.change_selections(
                            SelectionEffects::scroll(Autoscroll::center()),
                            window,
                            cx,
                            |selections| selections.select_ranges([position..position]),
                        );
                    });
                })
                .ok();
        }));
    }

    fn copy_symbol_name(&mut self, _: &CopySymbolName, _: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.selected.map(|row| row.node()) else {
            return;
        };
        if let Some(node) = self.tree.node(id) {
            cx.write_to_clipboard(ClipboardItem::new_string(node.symbol.name.to_string()));
        }
    }

    fn toggle_direction(
        &mut self,
        _: &ToggleDirection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_direction(self.direction.opposite(), window, cx);
    }

    fn set_direction(
        &mut self,
        direction: CallDirection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.direction == direction {
            return;
        }
        self.direction = direction;
        self.serialize(cx);
        self.rebuild_from_root(window, cx);
    }

    fn set_sources(&mut self, sources: SourceFilter, window: &mut Window, cx: &mut Context<Self>) {
        if self.sources == sources {
            return;
        }
        self.sources = sources;
        if self.tree.root().is_some() {
            self.rebuild_from_root(window, cx);
        } else {
            self.retry_pending_root(true, window, cx);
            cx.notify();
        }
    }

    fn rebuild_symbol_index(&mut self, cx: &mut Context<Self>) {
        self.project
            .read(cx)
            .symbol_index_store()
            .clone()
            .update(cx, |store, cx| store.rebuild(cx))
            .detach_and_log_err(cx);
    }

    fn choose_root(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(symbol) = self.root_candidates.get(index).cloned() else {
            return;
        };
        self.push_history(symbol.clone());
        self.set_root(symbol, window, cx);
    }

    fn refresh(&mut self, _: &Refresh, window: &mut Window, cx: &mut Context<Self>) {
        if self.tree.root().is_some() {
            self.rebuild_from_root(window, cx);
        } else {
            self.retry_pending_root(true, window, cx);
        }
    }

    fn rebuild_from_root(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(root) = self.root_symbol().cloned() {
            self.set_root(root, window, cx);
        } else {
            cx.notify();
        }
    }

    fn inspect_cursor(&mut self, _: &InspectCursor, window: &mut Window, cx: &mut Context<Self>) {
        let editor = self
            .workspace
            .upgrade()
            .and_then(|workspace| workspace.read(cx).active_item_as::<Editor>(cx));
        match editor {
            Some(editor) => self.inspect_editor(editor, true, window, cx),
            None => {
                self.state = PanelState::NoEditor;
                self.clear_tree();
                cx.notify();
            }
        }
    }

    fn previous_root(&mut self, _: &PreviousRoot, window: &mut Window, cx: &mut Context<Self>) {
        if self.history_index > 0 {
            self.go_to_history(self.history_index - 1, window, cx);
        }
    }

    fn next_root(&mut self, _: &NextRoot, window: &mut Window, cx: &mut Context<Self>) {
        if self.history_index + 1 < self.history.len() {
            self.go_to_history(self.history_index + 1, window, cx);
        }
    }

    fn go_to_history(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(symbol) = self.history.get(index).cloned() else {
            return;
        };
        self.history_index = index;
        // A root that was still being looked for would replace the one just
        // chosen when it arrives.
        self.root_task = None;
        self.follow_task = None;
        self.set_root(symbol, window, cx);
    }

    fn toggle_follow_cursor(
        &mut self,
        _: &ToggleFollowCursor,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.follow_cursor = !self.follow_cursor;
        if !self.follow_cursor {
            self.follow_task = None;
        }
        cx.notify();
    }

    fn cancel(&mut self, _: &menu::Cancel, window: &mut Window, cx: &mut Context<Self>) {
        if self.filter_editor.focus_handle(cx).is_focused(window) {
            window.focus(&self.focus_handle, cx);
        } else if self.choosing_root() {
            self.dismiss_root_candidates(window, cx);
        }
    }

    fn dispatch_context(&self, window: &Window, cx: &App) -> KeyContext {
        let mut dispatch_context = KeyContext::new_with_defaults();
        dispatch_context.add(CALL_GRAPH_PANEL_KEY);
        dispatch_context.add("menu");
        dispatch_context.add(if self.filter_editor.focus_handle(cx).is_focused(window) {
            "editing"
        } else {
            "not_editing"
        });
        dispatch_context
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let root_name = self
            .root_symbol()
            .map(|symbol| symbol.name.clone())
            .unwrap_or_else(|| SharedString::new_static(i18n::t!("No function selected")));
        let has_root = self.tree.root().is_some();
        let focus_handle = self.focus_handle.clone();
        let options_menu = self.render_options_menu(cx);
        let direction_button = |id: &'static str, label: &'static str, direction: CallDirection| {
            Button::new(id, label)
                .label_size(LabelSize::Small)
                .toggle_state(self.direction == direction)
                .selected_style(ButtonStyle::Tinted(TintColor::Accent))
                .on_click(cx.listener(move |panel, _, window, cx| {
                    panel.set_direction(direction, window, cx);
                }))
        };
        v_flex()
            .w_full()
            .gap_1()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                h_flex()
                    .w_full()
                    .gap_0p5()
                    .child(
                        IconButton::new("call-graph-previous-root", IconName::ChevronLeft)
                            .icon_size(IconSize::Small)
                            .disabled(self.history_index == 0)
                            .tooltip(Tooltip::for_action_title_in(
                                i18n::t!("Previous Root"),
                                &PreviousRoot,
                                &focus_handle,
                            ))
                            .on_click(cx.listener(|panel, _, window, cx| {
                                panel.previous_root(&PreviousRoot, window, cx);
                            })),
                    )
                    .child(
                        IconButton::new("call-graph-next-root", IconName::ChevronRight)
                            .icon_size(IconSize::Small)
                            .disabled(self.history_index + 1 >= self.history.len())
                            .tooltip(Tooltip::for_action_title_in(
                                i18n::t!("Next Root"),
                                &NextRoot,
                                &focus_handle,
                            ))
                            .on_click(cx.listener(|panel, _, window, cx| {
                                panel.next_root(&NextRoot, window, cx);
                            })),
                    )
                    .child(
                        div().flex_1().min_w_0().child(
                            Label::new(root_name)
                                .size(LabelSize::Small)
                                .color(if has_root {
                                    Color::Default
                                } else {
                                    Color::Muted
                                })
                                .truncate(),
                        ),
                    )
                    .child(
                        IconButton::new("call-graph-inspect", IconName::Crosshair)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::for_action_title_in(
                                i18n::t!("Inspect the Function at the Cursor"),
                                &InspectCursor,
                                &focus_handle,
                            ))
                            .on_click(cx.listener(|panel, _, window, cx| {
                                panel.inspect_cursor(&InspectCursor, window, cx);
                            })),
                    )
                    .child(
                        IconButton::new("call-graph-follow-cursor", IconName::Link)
                            .icon_size(IconSize::Small)
                            .toggle_state(self.follow_cursor)
                            .selected_style(ButtonStyle::Tinted(TintColor::Accent))
                            .tooltip(Tooltip::for_action_title_in(
                                i18n::t!("Follow the Cursor"),
                                &ToggleFollowCursor,
                                &focus_handle,
                            ))
                            .on_click(cx.listener(|panel, _, window, cx| {
                                panel.toggle_follow_cursor(&ToggleFollowCursor, window, cx);
                            })),
                    )
                    .child(
                        IconButton::new("call-graph-refresh", IconName::RotateCw)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::for_action_title_in(
                                i18n::tc!("call graph panel", "Refresh"),
                                &Refresh,
                                &focus_handle,
                            ))
                            .on_click(cx.listener(|panel, _, window, cx| {
                                panel.refresh(&Refresh, window, cx);
                            })),
                    )
                    .child(
                        IconButton::new("call-graph-collapse-all", IconName::ListCollapse)
                            .icon_size(IconSize::Small)
                            .disabled(!has_root)
                            .tooltip(Tooltip::for_action_title_in(
                                i18n::t!("Collapse All"),
                                &CollapseAll,
                                &focus_handle,
                            ))
                            .on_click(cx.listener(|panel, _, window, cx| {
                                panel.collapse_all(&CollapseAll, window, cx);
                            })),
                    )
                    .child(options_menu),
            )
            .child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .child(direction_button(
                        "call-graph-callers",
                        direction_label(CallDirection::Incoming),
                        CallDirection::Incoming,
                    ))
                    .child(direction_button(
                        "call-graph-callees",
                        direction_label(CallDirection::Outgoing),
                        CallDirection::Outgoing,
                    ))
                    .child(div().flex_1().min_w_0().child(self.filter_editor.clone())),
            )
    }

    fn render_options_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        let panel = cx.weak_entity();
        let sources = self.sources;
        let tooltip = match index_summary(&self.index_statuses(cx)) {
            Some(IndexSummary::Ready { files, definitions }) => SharedString::from(i18n::t!(
                "Symbol index: {files} files, {definitions} definitions",
                files = group_digits(files),
                definitions = group_digits(definitions)
            )),
            _ => SharedString::new_static(i18n::t!("Call Graph Options")),
        };
        PopoverMenu::new("call-graph-options")
            .trigger_with_tooltip(
                IconButton::new("call-graph-options-trigger", IconName::Ellipsis)
                    .icon_size(IconSize::Small),
                Tooltip::text(tooltip),
            )
            .menu(move |window, cx| {
                let panel = panel.clone();
                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                    menu = menu.header(i18n::tc!("call graph panel", "Sources"));
                    for filter in [
                        SourceFilter::All,
                        SourceFilter::LanguageServer,
                        SourceFilter::NameMatch,
                    ] {
                        let panel = panel.clone();
                        menu = menu.toggleable_entry(
                            source_filter_label(filter),
                            sources == filter,
                            IconPosition::End,
                            None,
                            move |window, cx| {
                                panel
                                    .update(cx, |panel, cx| panel.set_sources(filter, window, cx))
                                    .ok();
                            },
                        );
                    }
                    menu.separator().entry(
                        i18n::t!("Rebuild the Symbol Index"),
                        None,
                        move |_, cx| {
                            panel
                                .update(cx, |panel, cx| panel.rebuild_symbol_index(cx))
                                .ok();
                        },
                    )
                }))
            })
            .into_any_element()
    }

    /// Whether the symbol index can answer for what the panel shows: a C or
    /// C++ root, or a cursor in such a file.
    fn index_relevant(&self, cx: &App) -> bool {
        if !self.sources.asks_index() {
            return false;
        }
        if let Some(root) = self.root_symbol() {
            return root.is_c_family();
        }
        self.state == PanelState::ChooseRoot
            || self.pending_root.as_ref().is_some_and(|pending| {
                pending
                    .buffer
                    .upgrade()
                    .is_some_and(|buffer| call_source::is_c_family(buffer.read(cx)))
            })
    }

    /// What the status row says. An index being built shows whatever the panel shows, so
    /// that a long first build of a large tree is visible before a C file is opened; the
    /// other states only matter where the index answers.
    fn shown_index_summary(&self, cx: &App) -> Option<IndexSummary> {
        if !self.sources.asks_index() {
            return None;
        }
        let summary = index_summary(&self.index_statuses(cx))?;
        let building = matches!(
            summary,
            IndexSummary::Building { .. } | IndexSummary::Preparing
        );
        (building || self.index_relevant(cx)).then_some(summary)
    }

    fn render_index_status(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let summary = self.shown_index_summary(cx)?;
        let (message, hint) = index_summary_text(&summary)?;
        let building = matches!(
            summary,
            IndexSummary::Building { .. } | IndexSummary::Preparing
        );
        Some(
            v_flex()
                .debug_selector(|| "call-graph-index-status".into())
                .flex_none()
                .w_full()
                .px_2()
                .py_1()
                .gap_0p5()
                .border_b_1()
                .border_color(cx.theme().colors().border_variant)
                .child(
                    h_flex()
                        .gap_1()
                        .when(building, |this| {
                            this.child(
                                Icon::new(IconName::ArrowCircle)
                                    .size(IconSize::XSmall)
                                    .color(Color::Muted)
                                    .with_rotate_animation(2),
                            )
                        })
                        .child(
                            Label::new(message)
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        ),
                )
                .children(
                    hint.map(|hint| Label::new(hint).size(LabelSize::XSmall).color(Color::Muted)),
                )
                .into_any_element(),
        )
    }

    fn render_root_candidates(&self, cx: &mut Context<Self>) -> AnyElement {
        let name = self
            .root_candidates
            .first()
            .map(|symbol| symbol.name.clone())
            .unwrap_or_default();
        let (listed, not_listed) = self.listed_root_candidates();
        let mut items = Vec::with_capacity(listed.len());
        for (index, symbol) in listed.iter().enumerate() {
            let location = symbol
                .display_path
                .clone()
                .map(|path| SharedString::from(format!("{path}:{}", symbol.display_row + 1)));
            let badge = origin_badge(symbol.origin, cx);
            items.push(
                ListItem::new(("call-graph-root-candidate", index))
                    .toggle_state(index == self.selected_root_candidate)
                    .start_slot(
                        Icon::new(symbol_kind_icon(symbol.kind))
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        h_flex()
                            .min_w_0()
                            .flex_1()
                            .gap_1()
                            .overflow_hidden()
                            .child(Label::new(symbol.name.clone()).size(LabelSize::Small))
                            .children(location.map(|location| {
                                Label::new(location)
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted)
                                    .truncate()
                            })),
                    )
                    .end_slot(badge)
                    .on_click(cx.listener(move |panel, _, window, cx| {
                        panel.choose_root(index, window, cx);
                    })),
            );
        }
        // The symbol index can find up to a thousand definitions of one name,
        // more than the panel's height shows.
        v_flex()
            .id("call-graph-root-candidates")
            .flex_1()
            .min_h_0()
            .w_full()
            .overflow_y_scroll()
            .track_scroll(&self.root_candidates_scroll)
            .p_2()
            .gap_1()
            .child(
                Label::new(i18n::t!(
                    "Several functions are named {name}. Choose the root:"
                ))
                .size(LabelSize::Small),
            )
            .children(items)
            .when(not_listed > 0, |this| {
                this.child(
                    Label::new(i18n::tn!(
                        not_listed,
                        "{count} more are not listed",
                        "{count} more are not listed"
                    ))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
            })
            .into_any_element()
    }

    fn render_message(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (message, hint): (SharedString, Option<&'static str>) = match &self.state {
            PanelState::Ready => return None,
            PanelState::ChooseRoot => return Some(self.render_root_candidates(cx)),
            PanelState::Idle => (
                i18n::t!("Put the cursor on a function and inspect it to see its calls.").into(),
                Some(i18n::t!(
                    "Nothing is asked for until then, and each level is asked for only when you expand it."
                )),
            ),
            PanelState::LoadingRoot => (
                i18n::t!("Looking for the function at the cursor…").into(),
                None,
            ),
            PanelState::NoEditor => (
                i18n::t!("Open a file and put the cursor on a function.").into(),
                None,
            ),
            PanelState::NoSymbol => (
                i18n::t!("There is no function at the cursor.").into(),
                Some(i18n::t!(
                    "If the language server is still indexing, this is tried again when it finishes."
                )),
            ),
            PanelState::NoLanguageServer => (
                i18n::t!("No language server is running for this file.").into(),
                Some(i18n::t!(
                    "Calls come from the language server. For C and C++, clangd needs a compile_commands.json to find them."
                )),
            ),
            PanelState::Unsupported => (
                i18n::t!("The language server for this file cannot list calls.").into(),
                Some(i18n::t!(
                    "It does not offer call hierarchy. Another language server for this language may."
                )),
            ),
            PanelState::RootFailed(error) => (
                i18n::t!("Could not find the function at the cursor: {error}").into(),
                None,
            ),
        };
        Some(
            v_flex()
                .p_3()
                .gap_2()
                .child(
                    h_flex()
                        .gap_2()
                        .when(self.state == PanelState::LoadingRoot, |this| {
                            this.child(
                                Icon::new(IconName::ArrowCircle)
                                    .size(IconSize::Small)
                                    .color(Color::Muted)
                                    .with_rotate_animation(2),
                            )
                        })
                        .child(Label::new(message).size(LabelSize::Small)),
                )
                .children(
                    hint.map(|hint| Label::new(hint).size(LabelSize::Small).color(Color::Muted)),
                )
                .when(self.state != PanelState::LoadingRoot, |this| {
                    this.child(
                        h_flex().child(
                            Button::new("call-graph-inspect-message", i18n::t!("Inspect Cursor"))
                                .label_size(LabelSize::Small)
                                .style(ButtonStyle::Outlined)
                                .on_click(cx.listener(|panel, _, window, cx| {
                                    panel.inspect_cursor(&InspectCursor, window, cx);
                                })),
                        ),
                    )
                })
                .into_any_element(),
        )
    }

    fn render_rows(
        &mut self,
        range: Range<usize>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let selected_index = self.selected_index();
        range
            .filter_map(|index| {
                let row = *self.rows.get(index)?;
                Some(self.render_row(index, row, selected_index == Some(index), cx))
            })
            .collect()
    }

    fn render_row(
        &self,
        index: usize,
        row: Row,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let item = ListItem::new(index)
            .indent_level(row.depth() + 1)
            .indent_step_size(px(INDENT_STEP))
            .toggle_state(selected)
            .on_click(cx.listener(move |panel, event: &ClickEvent, window, cx| {
                // The rows may have changed since this one was drawn.
                let Some(current_index) = panel.rows.iter().position(|shown| same_row(shown, &row))
                else {
                    return;
                };
                panel.select_index(current_index, cx);
                window.focus(&panel.focus_handle, cx);
                if event.click_count() > 1 {
                    panel.open_selected(&OpenSelected, window, cx);
                } else if !matches!(row, Row::Node { .. }) {
                    panel.activate_message_row(row, window, cx);
                }
            }));
        match row {
            Row::Node { id, .. } => {
                let Some(node) = self.tree.node(id) else {
                    return item.into_any_element();
                };
                self.render_node(item, id, node, cx)
            }
            Row::Empty { .. } => item
                .child(
                    Label::new(empty_children_label(self.direction))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element(),
            Row::Error { parent, .. } => {
                let message = match self.tree.node(parent).map(|node| &node.children) {
                    Some(ChildrenState::Failed(message)) => message.clone(),
                    _ => SharedString::default(),
                };
                item.start_slot(
                    Icon::new(IconName::Warning)
                        .size(IconSize::Small)
                        .color(Color::Warning),
                )
                .child(
                    Label::new(i18n::t!("Could not get the calls: {message}"))
                        .size(LabelSize::Small)
                        .color(Color::Warning)
                        .truncate(),
                )
                .end_slot(
                    Button::new(("call-graph-retry", index), i18n::t!("Retry"))
                        .label_size(LabelSize::Small)
                        .on_click(cx.listener(move |panel, _, window, cx| {
                            panel.expand(parent, window, cx);
                        })),
                )
                .into_any_element()
            }
            Row::ShowMore { remaining, .. } => item
                .child(
                    Label::new(i18n::tn!(
                        remaining,
                        "Show {count} more",
                        "Show {count} more"
                    ))
                    .size(LabelSize::Small)
                    .color(Color::Accent),
                )
                .into_any_element(),
            Row::Excluded { count, .. } => item
                .child(
                    Label::new(excluded_label(count))
                        .size(LabelSize::Small)
                        .color(Color::Accent),
                )
                .tooltip(Tooltip::text(i18n::t!(
                    "Callees in the call_graph_panel.excluded_callees setting. Click to show them."
                )))
                .into_any_element(),
            Row::Truncated { .. } => item
                .start_slot(
                    Icon::new(IconName::Info)
                        .size(IconSize::Small)
                        .color(Color::Muted),
                )
                .child(
                    Label::new(truncated_label())
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element(),
        }
    }

    fn render_node(
        &self,
        item: ListItem,
        id: NodeId,
        node: &call_tree::CallNode,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let symbol = &node.symbol;
        let loading = node.is_loading();
        let role = symbol.role();
        let (name, name_color) = row_name(symbol);
        let note = row_note(role);
        let location = symbol
            .file_name()
            .map(|file_name| SharedString::from(format!("{file_name}:{}", symbol.display_row + 1)));
        let site_count = symbol.site_count();
        let tooltip_text = SharedString::from(match (&symbol.display_path, &symbol.detail) {
            (Some(path), Some(detail)) => {
                format!("{}{detail}\n{path}:{}", symbol.name, symbol.display_row + 1)
            }
            (Some(path), None) => format!("{}\n{path}:{}", symbol.name, symbol.display_row + 1),
            (None, Some(detail)) => format!("{}{detail}", symbol.name),
            (None, None) => symbol.name.to_string(),
        });
        item.toggle(node.can_expand().then_some(node.expanded))
            .always_show_disclosure_icon(true)
            .on_toggle(cx.listener(move |panel, _, window, cx| {
                panel.toggle_node(id, window, cx);
            }))
            .start_slot(
                Icon::new(symbol_kind_icon(symbol.kind))
                    .size(IconSize::Small)
                    .color(Color::Muted),
            )
            .tooltip(Tooltip::text(tooltip_text))
            .child(
                h_flex()
                    .min_w_0()
                    .flex_1()
                    .gap_1()
                    .overflow_hidden()
                    .child(Label::new(name).size(LabelSize::Small).color(name_color))
                    .children(symbol.detail.clone().map(|detail| {
                        Label::new(detail)
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .truncate()
                    }))
                    .children(note.map(|note| {
                        Label::new(note)
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .truncate()
                    })),
            )
            .end_slot(
                h_flex()
                    .gap_1()
                    .flex_none()
                    .when(loading, |this| {
                        this.child(
                            Icon::new(IconName::ArrowCircle)
                                .size(IconSize::XSmall)
                                .color(Color::Muted)
                                .with_rotate_animation(2),
                        )
                    })
                    .when(node.recursive, |this| {
                        this.child(
                            div()
                                .id(("call-graph-recursive", id.index()))
                                .child(
                                    Icon::new(IconName::RotateCcw)
                                        .size(IconSize::XSmall)
                                        .color(Color::Muted),
                                )
                                .tooltip(Tooltip::text(i18n::t!(
                                    "Recursive: this function is already on the path above"
                                ))),
                        )
                    })
                    .when(site_count > 1, |this| {
                        this.child(
                            Label::new(format!("×{site_count}"))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                    })
                    .children(location.map(|location| {
                        Label::new(location)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                    }))
                    .child(origin_badge(symbol.origin, cx)),
            )
            .into_any_element()
    }
}

fn empty_children_label(direction: CallDirection) -> &'static str {
    match direction {
        CallDirection::Incoming => i18n::t!("No callers"),
        CallDirection::Outgoing => i18n::t!("No callees"),
    }
}

fn direction_label(direction: CallDirection) -> &'static str {
    match direction {
        CallDirection::Incoming => i18n::t!("Callers"),
        CallDirection::Outgoing => i18n::t!("Callees"),
    }
}

fn excluded_label(count: usize) -> String {
    i18n::tn!(count, "{count} excluded", "{count} excluded")
}

fn truncated_label() -> String {
    i18n::t!(
        "Only the first {limit} calls are shown",
        limit = group_digits(project::symbol_index_store::MAX_CALLERS_LIMIT)
    )
}

/// The row's label and its color: a callee without a definition is muted.
fn row_name(symbol: &CallSymbol) -> (SharedString, Color) {
    match symbol.role() {
        SymbolRole::IndirectCall => {
            let field = &symbol.name;
            (i18n::t!("Indirect call: .{field}").into(), Color::Default)
        }
        SymbolRole::FileScope => (i18n::t!("(file scope)").into(), Color::Muted),
        SymbolRole::Unresolved => (symbol.name.clone(), Color::Muted),
        SymbolRole::Function | SymbolRole::SameNameDefinitions(_) => {
            (symbol.name.clone(), Color::Default)
        }
    }
}

fn row_note(role: SymbolRole) -> Option<String> {
    match role {
        SymbolRole::SameNameDefinitions(count) => Some(i18n::tn!(
            count,
            "{count} definition with this name",
            "{count} definitions with this name"
        )),
        SymbolRole::Unresolved => Some(i18n::t!("Definition not found").to_string()),
        SymbolRole::Function | SymbolRole::IndirectCall | SymbolRole::FileScope => None,
    }
}

fn source_filter_label(filter: SourceFilter) -> &'static str {
    match filter {
        SourceFilter::All => i18n::t!("All Sources"),
        SourceFilter::LanguageServer => i18n::t!("Language Server Only"),
        SourceFilter::NameMatch => i18n::t!("Name Matches Only"),
    }
}

/// The status line's text and a hint below it; nothing when the index is ready.
fn index_summary_text(summary: &IndexSummary) -> Option<(String, Option<&'static str>)> {
    Some(match summary {
        IndexSummary::Building { done, total } => (
            i18n::t!(
                "Indexing symbols {done} / {total}",
                done = group_digits(done),
                total = group_digits(total)
            ),
            Some(i18n::t!(
                "Name matches found so far may be incomplete; they are asked for again when indexing ends."
            )),
        ),
        IndexSummary::Preparing => (i18n::t!("Preparing the symbol index…").to_string(), None),
        IndexSummary::Unavailable(reason) => (unavailable_text(reason), None),
        IndexSummary::Disabled => (
            i18n::t!("The symbol index is off").to_string(),
            Some(i18n::t!(
                "Turn it on with the symbol_index.enabled setting to find calls by name in C and C++."
            )),
        ),
        IndexSummary::Ready { .. } => return None,
    })
}

fn unavailable_text(reason: &SymbolIndexUnavailable) -> String {
    match reason {
        SymbolIndexUnavailable::InUseByAnotherWindow => {
            i18n::t!("Symbol index: in use by another window").to_string()
        }
        SymbolIndexUnavailable::UnsupportedHost => {
            i18n::t!("Symbol index: this server does not support it").to_string()
        }
        SymbolIndexUnavailable::Collaboration => {
            i18n::t!("Symbol index: not available in a shared project").to_string()
        }
        SymbolIndexUnavailable::Error(error) => i18n::t!("Symbol index: {error}"),
    }
}

fn origin_label(origin: CallOrigin) -> &'static str {
    match origin {
        CallOrigin::LanguageServer => i18n::t!("Language server"),
        CallOrigin::NameMatch => i18n::t!("Name match"),
        CallOrigin::FunctionPointerCandidate => i18n::t!("Function pointer candidate"),
        CallOrigin::Macro => i18n::t!("Macro"),
    }
}

fn origin_badge(origin: CallOrigin, cx: &App) -> impl IntoElement {
    let label = origin_label(origin);
    let tooltip = match origin {
        CallOrigin::LanguageServer => i18n::t!("Exact: answered by the language server"),
        CallOrigin::NameMatch => i18n::t!(
            "Name match: found by the symbol index by name, not resolved by a language server"
        ),
        CallOrigin::FunctionPointerCandidate => i18n::t!(
            "Function pointer candidate: stored into the member this indirect call goes through, so it may be called there"
        ),
        CallOrigin::Macro => i18n::t!("Macro: a macro the symbol index found by name"),
    };
    let exact = origin == CallOrigin::LanguageServer;
    div()
        .id(label)
        .px_1()
        .rounded_sm()
        .when(exact, |this| {
            this.bg(cx.theme().colors().element_background)
        })
        .when(!exact, |this| {
            this.border_1()
                .border_color(cx.theme().colors().border_variant)
        })
        .child(Label::new(label).size(LabelSize::XSmall).color(if exact {
            Color::Default
        } else {
            Color::Muted
        }))
        .tooltip(Tooltip::text(tooltip))
}

fn symbol_kind_icon(kind: lsp::SymbolKind) -> IconName {
    match kind {
        lsp::SymbolKind::FUNCTION | lsp::SymbolKind::METHOD | lsp::SymbolKind::CONSTRUCTOR => {
            IconName::Code
        }
        lsp::SymbolKind::CLASS | lsp::SymbolKind::STRUCT | lsp::SymbolKind::INTERFACE => {
            IconName::Box
        }
        _ => IconName::Hash,
    }
}

/// Whether two rows are the same place in the tree, whatever their counts say.
fn same_row(left: &Row, right: &Row) -> bool {
    std::mem::discriminant(left) == std::mem::discriminant(right) && left.node() == right.node()
}

impl Panel for CallGraphPanel {
    fn persistent_name() -> &'static str {
        "Call Graph Panel"
    }

    fn panel_key() -> &'static str {
        CALL_GRAPH_PANEL_KEY
    }

    fn position(&self, _: &Window, cx: &App) -> DockPosition {
        match CallGraphPanelSettings::get_global(cx).dock {
            DockSide::Left => DockPosition::Left,
            DockSide::Right => DockPosition::Right,
        }
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(&mut self, position: DockPosition, _: &mut Window, cx: &mut Context<Self>) {
        let Some(fs) = self
            .workspace
            .read_with(cx, |workspace, _| workspace.app_state().fs.clone())
            .ok()
        else {
            return;
        };
        settings::update_settings_file(fs, cx, move |settings, _| {
            let dock = match position {
                DockPosition::Left | DockPosition::Bottom => DockSide::Left,
                DockPosition::Right => DockSide::Right,
            };
            settings.call_graph_panel.get_or_insert_default().dock = Some(dock);
        });
    }

    fn default_size(&self, _: &Window, cx: &App) -> Pixels {
        CallGraphPanelSettings::get_global(cx).default_width
    }

    fn icon(&self, _: &Window, cx: &App) -> Option<IconName> {
        CallGraphPanelSettings::get_global(cx)
            .button
            .then_some(IconName::ArrowRightLeft)
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some(i18n::t!("Call Graph Panel"))
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    // 9 is the first priority no other panel uses; duplicates panic in debug builds.
    fn activation_priority(&self) -> u32 {
        9
    }

    fn hide_button_setting(&self, _: &App) -> Option<workspace::HideStatusItem> {
        Some(workspace::HideStatusItem::new(|settings| {
            settings.call_graph_panel.get_or_insert_default().button = Some(false);
        }))
    }
}

impl Focusable for CallGraphPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for CallGraphPanel {}

impl Render for CallGraphPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = match self.render_message(cx) {
            Some(message) => message,
            None => div()
                .flex_1()
                .min_h_0()
                .size_full()
                .child(
                    uniform_list(
                        "call-graph-rows",
                        self.rows.len(),
                        cx.processor(Self::render_rows),
                    )
                    .size_full()
                    .track_scroll(&self.scroll_handle),
                )
                .vertical_scrollbar_for(&self.scroll_handle, window, cx)
                .into_any_element(),
        };
        v_flex()
            .id("call-graph-panel")
            .debug_selector(|| "call-graph-panel".into())
            .size_full()
            .overflow_hidden()
            .key_context(self.dispatch_context(window, cx))
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::expand_selected))
            .on_action(cx.listener(Self::collapse_selected))
            .on_action(cx.listener(Self::collapse_all))
            .on_action(cx.listener(Self::open_selected))
            .on_action(cx.listener(Self::preview_selected))
            .on_action(cx.listener(Self::copy_symbol_name))
            .on_action(cx.listener(Self::toggle_direction))
            .on_action(cx.listener(Self::refresh))
            .on_action(cx.listener(Self::inspect_cursor))
            .on_action(cx.listener(Self::previous_root))
            .on_action(cx.listener(Self::next_root))
            .on_action(cx.listener(Self::toggle_follow_cursor))
            .child(self.render_toolbar(cx))
            .children(self.render_index_status(cx))
            .child(content)
    }
}
