//! clangd's memory: the index mode and worker count it starts with, what its
//! `$/memoryUsage` says it holds, and a restart when it goes over the user's
//! limit.
//!
//! The measuring and the restarting run where clangd runs: locally for a local
//! project, inside `remote_server` for a remote one, whose client only hears
//! about it (`ClangdMemoryLimitExceeded`) because the settings to change are
//! the client's.

use std::{
    ffi::OsString,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, anyhow};
use client::{TypedEnvelope, proto};
use collections::{HashMap, HashSet};
use gpui::{
    App, AppContext as _, AsyncApp, Context, Entity, EntityId, EventEmitter, Global, Task,
    TaskExt as _, WeakEntity,
};
use lsp::{LanguageServerBinary, LanguageServerId, LanguageServerSelector};
use rpc::{AnyProtoClient, proto::REMOTE_SERVER_PROJECT_ID};
use settings::ClangdIndexMode;
use util::ResultExt as _;
use worktree::WorktreeId;

use crate::{
    lsp_store::{LocalLspStore, LspStore, clangd_ext::CLANGD_SERVER_NAME},
    project_settings::ClangdSettings,
};

/// How often the resident memory of a clangd with a limit is measured.
pub const MEMORY_CHECK_INTERVAL: Duration = Duration::from_secs(30);

/// A clangd that grows back over its limit right after a restart would
/// otherwise be restarted every check, losing its work each time.
pub const MIN_RESTART_INTERVAL: Duration = Duration::from_secs(10 * 60);

/// How long the menu waits for clangd, and for a remote host, to report. A
/// remote server of an older build drops the request it does not know
/// without answering, and the general LSP request timeout is minutes.
pub const MEMORY_REPORT_TIMEOUT: Duration = Duration::from_secs(10);

const BACKGROUND_INDEX_OPTION: &str = "background-index";
const WORKERS_OPTION: &str = "j";

/// What of the settings goes into clangd's command line, and so needs a
/// restart when it changes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClangdLaunchOptions {
    pub index_mode: ClangdIndexMode,
    pub max_workers: Option<u32>,
}

impl From<&ClangdSettings> for ClangdLaunchOptions {
    fn from(settings: &ClangdSettings) -> Self {
        Self {
            index_mode: settings.index_mode,
            max_workers: settings.max_workers,
        }
    }
}

/// The arguments to add to what clangd is already started with. An option the
/// user passes in `lsp.clangd.binary.arguments` is theirs and is not added
/// again, whatever its value.
pub fn arguments_to_add(options: ClangdLaunchOptions, existing: &[OsString]) -> Vec<OsString> {
    let mut arguments = Vec::new();
    if options.index_mode == ClangdIndexMode::Light
        && !has_option(existing, BACKGROUND_INDEX_OPTION)
    {
        arguments.push(OsString::from("--background-index=false"));
    }
    if let Some(workers) = options.max_workers
        && !has_option(existing, WORKERS_OPTION)
    {
        arguments.push(OsString::from(format!("-j={workers}")));
    }
    arguments
}

pub fn apply_launch_options(binary: &mut LanguageServerBinary, options: ClangdLaunchOptions) {
    let arguments = arguments_to_add(options, &binary.arguments);
    binary.arguments.extend(arguments);
}

/// Whether `name` is among the arguments, in any of the spellings clangd's
/// command line parser takes: one or two dashes, with or without `=value`.
/// `-j` also counts written as `-j4`, and `--background-index-priority` is
/// another option than `--background-index`.
fn has_option(arguments: &[OsString], name: &str) -> bool {
    arguments.iter().any(|argument| {
        let Some(argument) = argument.to_str() else {
            return false;
        };
        let Some(option) = argument
            .strip_prefix("--")
            .or_else(|| argument.strip_prefix('-'))
        else {
            return false;
        };
        let Some(rest) = option.strip_prefix(name) else {
            return false;
        };
        rest.is_empty()
            || rest.starts_with('=')
            || (name == WORKERS_OPTION && rest.bytes().all(|byte| byte.is_ascii_digit()))
    })
}

/// clangd's own account of its memory (`$/memoryUsage`, clangd 12 and later),
/// summed into the parts that the index mode and the open files decide.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClangdMemoryBreakdown {
    pub total_bytes: u64,
    /// The index of the whole project, which `light` leaves out.
    pub background_index_bytes: u64,
    /// The index of the open files and their headers.
    pub dynamic_index_bytes: u64,
    /// The preambles and ASTs of the open files.
    pub open_files_bytes: u64,
}

impl ClangdMemoryBreakdown {
    /// Reads the tree clangd answers with, where every node has `_self` and
    /// `_total` and names its children by what they are:
    /// `clangd_server` → `background_index`, `dynamic_index` and
    /// `tuscheduler` (one child per open file, each with `ast` and
    /// `preamble`). A part clangd leaves out, such as the background index
    /// when it is off, counts as nothing.
    pub fn from_memory_tree(tree: &serde_json::Value) -> Option<Self> {
        let total_bytes = total_of(tree)?;
        let server = tree.get("clangd_server");
        let part = |name: &str| {
            server
                .and_then(|server| server.get(name))
                .and_then(total_of)
                .unwrap_or(0)
        };
        Some(Self {
            total_bytes,
            background_index_bytes: part("background_index"),
            dynamic_index_bytes: part("dynamic_index"),
            open_files_bytes: part("tuscheduler"),
        })
    }

    pub fn from_proto(breakdown: &proto::ClangdMemoryBreakdown) -> Self {
        Self {
            total_bytes: breakdown.total_bytes,
            background_index_bytes: breakdown.background_index_bytes,
            dynamic_index_bytes: breakdown.dynamic_index_bytes,
            open_files_bytes: breakdown.open_files_bytes,
        }
    }

    pub fn to_proto(self) -> proto::ClangdMemoryBreakdown {
        proto::ClangdMemoryBreakdown {
            total_bytes: self.total_bytes,
            background_index_bytes: self.background_index_bytes,
            dynamic_index_bytes: self.dynamic_index_bytes,
            open_files_bytes: self.open_files_bytes,
        }
    }
}

/// The protocol only promises a JSON number, which another serializer may
/// write with a fraction or an exponent.
fn total_of(node: &serde_json::Value) -> Option<u64> {
    let total = node.get("_total")?;
    total.as_u64().or_else(|| {
        total
            .as_f64()
            .filter(|total| total.is_finite() && *total >= 0.0)
            .map(|total| total as u64)
    })
}

/// clangd's `$/memoryUsage`. Its answer is a tree of named nodes, so it is
/// kept as JSON and read by [`ClangdMemoryBreakdown::from_memory_tree`].
pub enum MemoryUsage {}

impl lsp::request::Request for MemoryUsage {
    type Params = ();
    type Result = serde_json::Value;
    const METHOD: &'static str = "$/memoryUsage";
}

/// What is known of one clangd's memory.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClangdMemoryReport {
    /// The resident memory of the process, as the operating system counts it.
    pub resident_bytes: Option<u64>,
    pub breakdown: Option<ClangdMemoryBreakdown>,
}

impl ClangdMemoryReport {
    /// The figure to show as clangd's memory: what the operating system
    /// counts when it could be asked, clangd's own sum otherwise.
    pub fn used_bytes(&self) -> Option<u64> {
        self.resident_bytes
            .or_else(|| self.breakdown.map(|breakdown| breakdown.total_bytes))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClangdMemoryEvent {
    /// A clangd of the store `lsp_store` went over `clangd.memory_limit_mb`.
    /// `restarted` is false when it was restarted too recently to be
    /// restarted again.
    LimitExceeded {
        lsp_store: EntityId,
        server_id: LanguageServerId,
        resident_bytes: u64,
        limit_bytes: u64,
        restarted: bool,
    },
}

/// Where [`ClangdMemoryEvent`]s are emitted. Not the store itself: a second
/// event type on it would leave every untyped subscription to it ambiguous.
pub struct ClangdMemoryEvents;

impl EventEmitter<ClangdMemoryEvent> for ClangdMemoryEvents {}

struct GlobalClangdMemoryEvents(Entity<ClangdMemoryEvents>);

impl Global for GlobalClangdMemoryEvents {}

impl ClangdMemoryEvents {
    pub fn global(cx: &mut App) -> Entity<Self> {
        if let Some(events) = cx.try_global::<GlobalClangdMemoryEvents>() {
            return events.0.clone();
        }
        let events = cx.new(|_| ClangdMemoryEvents);
        cx.set_global(GlobalClangdMemoryEvents(events.clone()));
        events
    }

    fn emit(event: ClangdMemoryEvent, cx: &mut App) {
        Self::global(cx).update(cx, |_, cx| cx.emit(event));
    }
}

/// What to do about a clangd found over its limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OverLimitAction {
    Restart,
    /// Restarted less than [`MIN_RESTART_INTERVAL`] ago: say so, once.
    NotifyOnly,
    /// Already said so since the last restart.
    Nothing,
}

/// Keyed by worktree, since each restart gives clangd a new server id.
#[derive(Default)]
pub struct RestartThrottle {
    last_restart: HashMap<WorktreeId, Instant>,
    notified_without_restart: HashSet<WorktreeId>,
}

impl RestartThrottle {
    pub fn over_limit(&mut self, worktree_id: WorktreeId, now: Instant) -> OverLimitAction {
        match self.last_restart.get(&worktree_id) {
            Some(last_restart)
                if now.saturating_duration_since(*last_restart) < MIN_RESTART_INTERVAL =>
            {
                if self.notified_without_restart.insert(worktree_id) {
                    OverLimitAction::NotifyOnly
                } else {
                    OverLimitAction::Nothing
                }
            }
            _ => {
                self.last_restart.insert(worktree_id, now);
                self.notified_without_restart.remove(&worktree_id);
                OverLimitAction::Restart
            }
        }
    }
}

/// Measures a process's memory in tests, where the fake servers have none.
#[cfg(any(test, feature = "test-support"))]
pub type ResidentMemoryForTests =
    std::sync::Arc<dyn Fn(LanguageServerId) -> Option<u64> + Send + Sync>;

/// The part of the local store that watches clangd's memory.
#[derive(Default)]
pub(super) struct ClangdMemoryState {
    /// What each clangd was started with, and in which worktree.
    launched: HashMap<LanguageServerId, (WorktreeId, ClangdLaunchOptions)>,
    throttle: RestartThrottle,
    /// Servers that answered `$/memoryUsage` with an error: clangd before 12.
    without_memory_usage: HashSet<LanguageServerId>,
    monitoring: bool,
    _monitor: Option<Task<()>>,
    #[cfg(any(test, feature = "test-support"))]
    resident_memory_for_tests: Option<ResidentMemoryForTests>,
}

/// The launch options of a server about to start in `worktree_id`, when it is
/// clangd.
pub(super) fn launch_options_for(
    server_name: &lsp::LanguageServerName,
    worktree_id: WorktreeId,
    cx: &App,
) -> Option<ClangdLaunchOptions> {
    (*server_name == CLANGD_SERVER_NAME)
        .then(|| ClangdLaunchOptions::from(&clangd_settings(worktree_id, cx)))
}

fn clangd_settings(worktree_id: WorktreeId, cx: &App) -> ClangdSettings {
    ClangdSettings::for_worktree(worktree_id, cx)
}

impl LocalLspStore {
    pub(super) fn note_clangd_launch(
        &mut self,
        server_id: LanguageServerId,
        worktree_id: WorktreeId,
        options: ClangdLaunchOptions,
        cx: &mut App,
    ) {
        self.clangd_memory
            .launched
            .insert(server_id, (worktree_id, options));
        // The server is not among `language_server_ids` yet.
        if clangd_settings(worktree_id, cx).memory_limit_mb.is_some() {
            self.start_clangd_memory_monitor(cx);
        }
    }

    /// Starts the periodic check unless it is running already. It stops by
    /// itself once no clangd has a limit.
    fn start_clangd_memory_monitor(&mut self, cx: &mut App) {
        if self.clangd_memory.monitoring {
            return;
        }
        self.clangd_memory.monitoring = true;
        let lsp_store = self.weak.clone();
        self.clangd_memory._monitor = Some(cx.spawn(async move |cx| {
            monitor_clangd_memory(lsp_store, cx).await;
        }));
    }

    /// Whether some clangd, running or starting, has a limit.
    fn clangd_memory_limit_set(&self, cx: &App) -> bool {
        self.language_server_ids.keys().any(|seed| {
            seed.name == CLANGD_SERVER_NAME
                && clangd_settings(seed.worktree_id, cx)
                    .memory_limit_mb
                    .is_some()
        })
    }

    /// The running clangd servers whose worktree sets a limit, with the limit
    /// in bytes.
    fn clangd_memory_targets(&self, cx: &App) -> Vec<MemoryTarget> {
        self.language_server_ids
            .iter()
            .filter(|(seed, _)| seed.name == CLANGD_SERVER_NAME)
            .filter_map(|(seed, state)| {
                let server = self.running_language_server_for_id(state.id)?;
                let limit_mb = clangd_settings(seed.worktree_id, cx).memory_limit_mb?;
                Some(MemoryTarget {
                    server_id: state.id,
                    worktree_id: seed.worktree_id,
                    process_id: server.process_id(),
                    limit_bytes: limit_mb.saturating_mul(1024 * 1024),
                })
            })
            .collect()
    }

    fn resident_memory_measurer(
        &self,
    ) -> impl Fn(&MemoryTarget) -> Option<u64> + Send + 'static + use<> {
        #[cfg(any(test, feature = "test-support"))]
        let for_tests = self.clangd_memory.resident_memory_for_tests.clone();
        move |target: &MemoryTarget| {
            #[cfg(any(test, feature = "test-support"))]
            if let Some(for_tests) = &for_tests {
                return for_tests(target.server_id);
            }
            resident_bytes(target.process_id?)
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct MemoryTarget {
    server_id: LanguageServerId,
    worktree_id: WorktreeId,
    process_id: Option<u32>,
    limit_bytes: u64,
}

async fn monitor_clangd_memory(lsp_store: WeakEntity<LspStore>, cx: &mut AsyncApp) {
    loop {
        cx.background_executor().timer(MEMORY_CHECK_INTERVAL).await;
        let Ok(Some((targets, measure))) = lsp_store.update(cx, |lsp_store, cx| {
            let local = lsp_store.as_local_mut()?;
            if !local.clangd_memory_limit_set(cx) {
                local.clangd_memory.monitoring = false;
                return None;
            }
            Some((
                local.clangd_memory_targets(cx),
                local.resident_memory_measurer(),
            ))
        }) else {
            return;
        };
        let measured = cx
            .background_spawn(async move {
                targets
                    .into_iter()
                    .filter_map(|target| Some((target, measure(&target)?)))
                    .collect::<Vec<_>>()
            })
            .await;
        let still_there = lsp_store.update(cx, |lsp_store, cx| {
            for (target, resident_bytes) in measured {
                if resident_bytes > target.limit_bytes {
                    lsp_store.clangd_over_limit(target, resident_bytes, cx);
                }
            }
        });
        if still_there.is_err() {
            return;
        }
    }
}

/// One process as the operating system's process table showed it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ProcessRecord {
    process_id: u32,
    parent_id: Option<u32>,
    /// Seconds since the Unix epoch.
    start_time: u64,
    /// The creation time in 100-nanosecond units, read on Windows where whole
    /// seconds cannot tell which of two processes started first.
    precise_start_time: Option<u64>,
    resident_bytes: u64,
}

fn process_table() -> Vec<ProcessRecord> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};
    let mut system = System::new();
    // On Linux every thread is listed as a process of its own, a child of its
    // process holding the same resident memory: counted, they would multiply
    // clangd's memory by its thread count.
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().with_memory().without_tasks(),
    );
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut table = system
        .processes()
        .values()
        .filter(|process| process.thread_kind().is_none())
        .map(|process| ProcessRecord {
            process_id: process.pid().as_u32(),
            parent_id: process.parent().map(|parent| parent.as_u32()),
            start_time: process.start_time(),
            precise_start_time: None,
            resident_bytes: process.memory(),
        })
        .collect::<Vec<_>>();
    #[cfg(windows)]
    {
        let start_times = table
            .iter()
            .map(|record| (record.process_id, record.start_time))
            .collect::<HashMap<_, _>>();
        let tied = table
            .iter()
            .filter_map(|record| {
                let parent_id = record.parent_id?;
                (start_times.get(&parent_id) == Some(&record.start_time))
                    .then_some([record.process_id, parent_id])
            })
            .flatten()
            .collect::<HashSet<_>>();
        for record in &mut table {
            if tied.contains(&record.process_id) {
                record.precise_start_time = precise_start_time(record);
            }
        }
    }
    table
}

/// The creation time of the process `record` describes, or `None` when it
/// cannot be read or the id has since gone to another process.
#[cfg(windows)]
fn precise_start_time(record: &ProcessRecord) -> Option<u64> {
    use windows::Win32::{
        Foundation::{CloseHandle, FILETIME},
        System::Threading::{GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
    };
    const TICKS_PER_SECOND: u64 = 10_000_000;
    const SECONDS_FROM_1601_TO_1970: u64 = 11_644_473_600;
    let handle =
        unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, record.process_id) }.ok()?;
    let mut creation_time = FILETIME::default();
    let mut exit_time = FILETIME::default();
    let mut kernel_time = FILETIME::default();
    let mut user_time = FILETIME::default();
    let times = unsafe {
        GetProcessTimes(
            handle,
            &mut creation_time,
            &mut exit_time,
            &mut kernel_time,
            &mut user_time,
        )
    };
    unsafe { CloseHandle(handle) }.log_err();
    times.ok()?;
    let ticks =
        (u64::from(creation_time.dwHighDateTime) << 32) | u64::from(creation_time.dwLowDateTime);
    let start_time = (ticks / TICKS_PER_SECOND).checked_sub(SECONDS_FROM_1601_TO_1970);
    (start_time == Some(record.start_time)).then_some(ticks)
}

/// Whether `child`, which names `parent`'s id as its parent, was started by
/// `parent`. A process outlives its parent with the parent's id still recorded
/// on Windows, and that id can be reused: one that started before the process
/// now holding the id is not its child. A child started right away shares its
/// parent's second, and so may an older process given the id in that second,
/// which only the finer creation times tell apart. Without them, a tie is
/// taken as a child only where an orphan's parent id is replaced.
fn started_by(child: &ProcessRecord, parent: &ProcessRecord) -> bool {
    if child.start_time != parent.start_time {
        return child.start_time > parent.start_time;
    }
    match (child.precise_start_time, parent.precise_start_time) {
        (Some(child_start), Some(parent_start)) => child_start >= parent_start,
        _ => !cfg!(windows),
    }
}

/// `root` and all its descendants in `table`, `root` first, or nothing when
/// `root` has gone.
fn process_tree(root: u32, table: &[ProcessRecord]) -> Vec<ProcessRecord> {
    let Some(root) = table.iter().find(|record| record.process_id == root) else {
        return Vec::new();
    };
    let mut tree = vec![*root];
    let mut next = 0;
    while let Some(parent) = tree.get(next).copied() {
        next += 1;
        for record in table {
            let is_child = record.parent_id == Some(parent.process_id)
                && record.process_id != parent.process_id
                && started_by(record, &parent);
            if is_child
                && !tree
                    .iter()
                    .any(|member| member.process_id == record.process_id)
            {
                tree.push(*record);
            }
        }
    }
    tree
}

/// The resident memory of a process and of all its descendants, or `None`
/// when it has gone. A wrapper script that does not `exec` clangd leaves
/// clangd as its child, holding all of the memory.
fn resident_bytes(process_id: u32) -> Option<u64> {
    tree_resident_bytes(process_id, &process_table())
}

fn tree_resident_bytes(root: u32, table: &[ProcessRecord]) -> Option<u64> {
    let tree = process_tree(root, table);
    if tree.is_empty() {
        return None;
    }
    Some(
        tree.iter()
            .map(|record| record.resident_bytes)
            .fold(0, u64::saturating_add),
    )
}

/// The descendants of a process, to end once it has been stopped: stopping a
/// language server kills only the process it started.
fn descendants_of(process_id: u32) -> Vec<ProcessRecord> {
    let descendants = process_tree(process_id, &process_table())
        .into_iter()
        .skip(1);
    // Read for every one, so that an id given to another process started in
    // the same second is not taken for it when killing.
    #[cfg(windows)]
    let descendants = descendants.map(|record| ProcessRecord {
        precise_start_time: record
            .precise_start_time
            .or_else(|| precise_start_time(&record)),
        ..record
    });
    descendants.collect()
}

/// Kills those of `processes` still running, and returns how many it killed.
/// One whose id now belongs to a process started at another time is left.
fn kill_remaining(processes: &[ProcessRecord]) -> usize {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
    let process_ids = processes
        .iter()
        .map(|record| Pid::from_u32(record.process_id))
        .collect::<Vec<_>>();
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&process_ids),
        true,
        ProcessRefreshKind::nothing().without_tasks(),
    );
    processes
        .iter()
        .filter(|record| {
            let Some(process) = system.process(Pid::from_u32(record.process_id)) else {
                return false;
            };
            // A thread's id, signalled, ends its whole process.
            if process.thread_kind().is_some() || process.start_time() != record.start_time {
                return false;
            }
            // `system` holds the process open, which keeps Windows from giving
            // its id to another process before the kill.
            #[cfg(windows)]
            if record.precise_start_time.is_some()
                && precise_start_time(record) != record.precise_start_time
            {
                return false;
            }
            let killed = process.kill();
            if !killed {
                log::warn!(
                    "could not kill process {} left by a stopped clangd",
                    record.process_id
                );
            }
            killed
        })
        .count()
}

impl LspStore {
    pub(super) fn init_clangd_memory(client: &AnyProtoClient) {
        client.add_entity_request_handler(Self::handle_get_clangd_memory_usage);
        client.add_entity_message_handler(Self::handle_clangd_memory_limit_exceeded);
    }

    /// What is known of a clangd's memory, asked of the machine it runs on.
    pub fn clangd_memory_report(
        &mut self,
        server_id: LanguageServerId,
        cx: &mut Context<Self>,
    ) -> Task<Result<ClangdMemoryReport>> {
        if let Some((client, project_id)) = self.upstream_client() {
            // A collab host would have to relay a message it does not know.
            if project_id != REMOTE_SERVER_PROJECT_ID {
                return Task::ready(Err(anyhow!(
                    "clangd's memory is only reported for local and remote server projects"
                )));
            }
            let request = client.request(proto::GetClangdMemoryUsage {
                project_id,
                language_server_id: server_id.to_proto(),
            });
            // Longer than clangd's own timeout on the host, so that a slow
            // clangd still gets its resident memory reported.
            let timeout = cx.background_executor().timer(MEMORY_REPORT_TIMEOUT * 2);
            return cx.background_spawn(async move {
                let response = smol::future::or(request, async move {
                    timeout.await;
                    Err(anyhow!(
                        "the remote host did not report clangd's memory in time"
                    ))
                })
                .await?;
                Ok(ClangdMemoryReport {
                    resident_bytes: response.resident_bytes,
                    breakdown: response
                        .breakdown
                        .as_ref()
                        .map(ClangdMemoryBreakdown::from_proto),
                })
            });
        }

        let Some(local) = self.as_local() else {
            return Task::ready(Err(anyhow!("the language server store is not local")));
        };
        let Some(server) = local.running_language_server_for_id(server_id).cloned() else {
            return Task::ready(Err(anyhow!("language server {server_id} is not running")));
        };
        if server.name() != CLANGD_SERVER_NAME {
            return Task::ready(Err(anyhow!("language server {server_id} is not clangd")));
        }
        let target = MemoryTarget {
            server_id,
            worktree_id: WorktreeId::from_usize(0),
            process_id: server.process_id(),
            limit_bytes: 0,
        };
        let measure = local.resident_memory_measurer();
        let memory_usage = (!local
            .clangd_memory
            .without_memory_usage
            .contains(&server_id))
        .then(|| server.request::<MemoryUsage>((), MEMORY_REPORT_TIMEOUT));
        cx.spawn(async move |lsp_store, cx| {
            let resident_bytes = cx.background_spawn(async move { measure(&target) }).await;
            let breakdown = match memory_usage {
                Some(request) => match request.await.into_response() {
                    Ok(tree) => ClangdMemoryBreakdown::from_memory_tree(&tree),
                    Err(error) => {
                        if error.downcast_ref::<lsp::ResponseError>().is_some() {
                            lsp_store
                                .update(cx, |lsp_store, _| {
                                    if let Some(local) = lsp_store.as_local_mut() {
                                        local.clangd_memory.without_memory_usage.insert(server_id);
                                    }
                                })
                                .ok();
                        }
                        log::info!("clangd did not report its memory: {error:#}");
                        None
                    }
                },
                None => None,
            };
            Ok(ClangdMemoryReport {
                resident_bytes,
                breakdown,
            })
        })
    }

    fn clangd_over_limit(
        &mut self,
        target: MemoryTarget,
        resident_bytes: u64,
        cx: &mut Context<Self>,
    ) {
        let now = cx.background_executor().now();
        let Some(local) = self.as_local_mut() else {
            return;
        };
        // Measured off the main thread: a change of settings may have
        // restarted or stopped it since.
        if !local.language_servers.contains_key(&target.server_id) {
            return;
        }
        let restarted = match local
            .clangd_memory
            .throttle
            .over_limit(target.worktree_id, now)
        {
            OverLimitAction::Restart => true,
            OverLimitAction::NotifyOnly => false,
            OverLimitAction::Nothing => return,
        };
        if restarted {
            log::info!(
                "restarting clangd ({}): {resident_bytes} bytes resident, over its limit of {}",
                target.server_id,
                target.limit_bytes
            );
            self.restart_clangd(target.server_id, cx);
        }
        if let Some((client, project_id)) = &self.downstream_client
            && *project_id == REMOTE_SERVER_PROJECT_ID
        {
            client
                .send(proto::ClangdMemoryLimitExceeded {
                    project_id: *project_id,
                    language_server_id: target.server_id.to_proto(),
                    resident_bytes,
                    limit_bytes: target.limit_bytes,
                    restarted,
                })
                .log_err();
        }
        ClangdMemoryEvents::emit(
            ClangdMemoryEvent::LimitExceeded {
                lsp_store: cx.entity_id(),
                server_id: target.server_id,
                resident_bytes,
                limit_bytes: target.limit_bytes,
                restarted,
            },
            cx,
        );
    }

    /// Restarts one clangd, and only it: the buffers open in it are opened in
    /// its successor, and a clangd of another worktree keeps running.
    fn restart_clangd(&mut self, server_id: LanguageServerId, cx: &mut Context<Self>) {
        let Some(local) = self.as_local_mut() else {
            return;
        };
        local.clangd_memory.launched.remove(&server_id);
        local.clangd_memory.without_memory_usage.remove(&server_id);
        let process_id = local
            .running_language_server_for_id(server_id)
            .and_then(|server| server.process_id());
        cx.spawn(async move |lsp_store, cx| {
            // Listed before the stop: once the process clangd was started
            // with is killed, its children no longer lead back to it.
            let descendants = match process_id {
                Some(process_id) => {
                    cx.background_spawn(async move { descendants_of(process_id) })
                        .await
                }
                None => Vec::new(),
            };
            let stop = lsp_store.update(cx, |lsp_store, cx| {
                let (_, stop) = lsp_store.stop_local_language_servers_for_buffers(
                    &[],
                    HashSet::from_iter([LanguageServerSelector::Id(server_id)]),
                    cx,
                );
                stop
            })?;
            stop.await;
            if !descendants.is_empty() {
                let killed = cx
                    .background_spawn(async move { kill_remaining(&descendants) })
                    .await;
                if killed > 0 {
                    log::info!(
                        "killed {killed} processes the stopped clangd ({server_id}) left running"
                    );
                }
            }
            lsp_store.update(cx, |lsp_store, cx| {
                if lsp_store.as_local().is_none() {
                    return;
                }
                // Chosen after the stop, not before, as a restart chooses them.
                let open_buffers = lsp_store.buffer_store.read(cx).buffers().collect();
                let buffers = lsp_store.buffers_to_reopen_after_restart(open_buffers, cx);
                for buffer in buffers {
                    lsp_store.register_buffer_with_language_servers(
                        &buffer,
                        HashSet::from_iter([LanguageServerSelector::Name(CLANGD_SERVER_NAME)]),
                        true,
                        cx,
                    );
                }
            })
        })
        .detach_and_log_err(cx);
    }

    /// Restarts each clangd whose command line the settings no longer match,
    /// and starts or stops watching memory as limits come and go.
    pub(super) fn on_clangd_settings_changed(&mut self, cx: &mut Context<Self>) {
        let Some(local) = self.as_local_mut() else {
            return;
        };
        let running = &local.language_servers;
        local
            .clangd_memory
            .launched
            .retain(|server_id, _| running.contains_key(server_id));
        let outdated = local
            .clangd_memory
            .launched
            .iter()
            .filter(|(_, (worktree_id, options))| {
                ClangdLaunchOptions::from(&clangd_settings(*worktree_id, cx)) != *options
            })
            .map(|(server_id, _)| *server_id)
            .collect::<Vec<_>>();
        if local.clangd_memory_limit_set(cx) {
            local.start_clangd_memory_monitor(cx);
        }
        for server_id in outdated {
            log::info!("restarting clangd ({server_id}): its settings changed");
            self.restart_clangd(server_id, cx);
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn set_clangd_resident_memory_for_tests(&mut self, measure: ResidentMemoryForTests) {
        if let Some(local) = self.as_local_mut() {
            local.clangd_memory.resident_memory_for_tests = Some(measure);
        }
    }

    async fn handle_get_clangd_memory_usage(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GetClangdMemoryUsage>,
        mut cx: AsyncApp,
    ) -> Result<proto::GetClangdMemoryUsageResponse> {
        let server_id = LanguageServerId::from_proto(envelope.payload.language_server_id);
        let report = this
            .update(&mut cx, |this, cx| this.clangd_memory_report(server_id, cx))
            .await
            .context("measuring clangd's memory")?;
        Ok(proto::GetClangdMemoryUsageResponse {
            resident_bytes: report.resident_bytes,
            breakdown: report.breakdown.map(ClangdMemoryBreakdown::to_proto),
        })
    }

    async fn handle_clangd_memory_limit_exceeded(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::ClangdMemoryLimitExceeded>,
        mut cx: AsyncApp,
    ) -> Result<()> {
        let message = envelope.payload;
        this.update(&mut cx, |_, cx| {
            ClangdMemoryEvents::emit(
                ClangdMemoryEvent::LimitExceeded {
                    lsp_store: cx.entity_id(),
                    server_id: LanguageServerId::from_proto(message.language_server_id),
                    resident_bytes: message.resident_bytes,
                    limit_bytes: message.limit_bytes,
                    restarted: message.restarted,
                },
                cx,
            );
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn default_settings_add_no_arguments() {
        let options = ClangdLaunchOptions::from(&ClangdSettings::default());
        assert_eq!(arguments_to_add(options, &[]), Vec::<OsString>::new());
    }

    #[test]
    fn light_mode_and_workers_add_their_options() {
        let options = ClangdLaunchOptions {
            index_mode: ClangdIndexMode::Light,
            max_workers: Some(2),
        };
        assert_eq!(
            arguments_to_add(options, &arguments(&["--log=error"])),
            arguments(&["--background-index=false", "-j=2"])
        );
        let full = ClangdLaunchOptions {
            index_mode: ClangdIndexMode::Full,
            max_workers: None,
        };
        assert_eq!(arguments_to_add(full, &[]), Vec::<OsString>::new());
    }

    #[test]
    fn user_arguments_of_the_same_kind_win() {
        let options = ClangdLaunchOptions {
            index_mode: ClangdIndexMode::Light,
            max_workers: Some(2),
        };
        for user_arguments in [
            &["--background-index", "-j=8"][..],
            &["-background-index=true", "--j=8"],
            &["--background-index=1", "-j", "8"],
            &["--background-index", "-j8"],
        ] {
            assert_eq!(
                arguments_to_add(options, &arguments(user_arguments)),
                Vec::<OsString>::new(),
                "{user_arguments:?}"
            );
        }
        // Options that only share a prefix are other options.
        assert_eq!(
            arguments_to_add(
                options,
                &arguments(&[
                    "--background-index-priority=low",
                    "-jobs",
                    "--pch-storage=disk"
                ])
            ),
            arguments(&["--background-index=false", "-j=2"])
        );
    }

    /// The shape clangd 18 answers `$/memoryUsage` with, trimmed to two open
    /// files (`ClangdLSPServer::onMemoryUsage` and `toJSON(MemoryTree)`).
    fn clangd_memory_tree() -> serde_json::Value {
        serde_json::json!({
            "_self": 0,
            "_total": 1_250_000,
            "clangd_server": {
                "_self": 0,
                "_total": 1_250_000,
                "background_index": {
                    "_self": 0,
                    "_total": 800_000,
                    "index": { "_self": 500_000, "_total": 500_000 },
                    "slabs": { "_self": 300_000, "_total": 300_000 }
                },
                "dynamic_index": {
                    "_self": 0,
                    "_total": 50_000,
                    "main_file": { "_self": 20_000, "_total": 20_000 },
                    "preamble": { "_self": 30_000, "_total": 30_000 }
                },
                "tuscheduler": {
                    "_self": 0,
                    "_total": 400_000,
                    "/src/a.c": {
                        "_self": 0,
                        "_total": 300_000,
                        "ast": { "_self": 100_000, "_total": 100_000 },
                        "preamble": { "_self": 200_000, "_total": 200_000 }
                    },
                    "/src/b.c": {
                        "_self": 0,
                        "_total": 100_000,
                        "ast": { "_self": 100_000, "_total": 100_000 },
                        "preamble": { "_self": 0, "_total": 0 }
                    }
                }
            }
        })
    }

    #[test]
    fn memory_tree_is_summed_by_part() {
        assert_eq!(
            ClangdMemoryBreakdown::from_memory_tree(&clangd_memory_tree()),
            Some(ClangdMemoryBreakdown {
                total_bytes: 1_250_000,
                background_index_bytes: 800_000,
                dynamic_index_bytes: 50_000,
                open_files_bytes: 400_000,
            })
        );
    }

    #[test]
    fn memory_tree_without_a_background_index_counts_it_as_nothing() {
        let mut tree = clangd_memory_tree();
        if let Some(server) = tree
            .get_mut("clangd_server")
            .and_then(|server| server.as_object_mut())
        {
            server.remove("background_index");
        }
        let breakdown = ClangdMemoryBreakdown::from_memory_tree(&tree);
        assert_eq!(
            breakdown.map(|breakdown| breakdown.background_index_bytes),
            Some(0)
        );
        assert_eq!(
            breakdown.map(|breakdown| breakdown.open_files_bytes),
            Some(400_000)
        );
        assert_eq!(
            ClangdMemoryBreakdown::from_memory_tree(&serde_json::json!({ "clangd_server": {} })),
            None,
            "an answer without a total is not a memory tree"
        );
    }

    #[test]
    fn memory_tree_of_an_unknown_shape_gives_what_it_can() {
        let tree = serde_json::json!({
            "_total": 1.5e6,
            "clangd_server": {
                "background_index": { "_total": "800000" },
                "dynamic_index": [1, 2, 3],
                "tuscheduler": { "_total": 4.0e5 },
                "something_new": { "_total": 5 }
            }
        });
        assert_eq!(
            ClangdMemoryBreakdown::from_memory_tree(&tree),
            Some(ClangdMemoryBreakdown {
                total_bytes: 1_500_000,
                background_index_bytes: 0,
                dynamic_index_bytes: 0,
                open_files_bytes: 400_000,
            })
        );
        for not_a_tree in [
            serde_json::json!(null),
            serde_json::json!([]),
            serde_json::json!("1000"),
            serde_json::json!({ "_total": -1 }),
            serde_json::json!({ "_total": null }),
            serde_json::json!({ "clangd_server": 5 }),
        ] {
            assert_eq!(
                ClangdMemoryBreakdown::from_memory_tree(&not_a_tree),
                None,
                "{not_a_tree}"
            );
        }
        assert_eq!(
            ClangdMemoryBreakdown::from_memory_tree(
                &serde_json::json!({ "_total": 7, "clangd_server": 5 })
            ),
            Some(ClangdMemoryBreakdown {
                total_bytes: 7,
                ..Default::default()
            })
        );
    }

    #[test]
    fn resident_memory_is_in_bytes_and_of_the_process_asked_for() {
        let resident = resident_bytes(std::process::id()).expect("this process is running");
        // A test binary holds tens of megabytes: counted in kilobytes it
        // would stay under this.
        assert!(resident > 1024 * 1024, "{resident}");
        assert!(resident < 1024 * 1024 * 1024 * 1024, "{resident}");
        assert_eq!(resident_bytes(u32::MAX - 1), None);
    }

    fn record(process_id: u32, parent_id: Option<u32>, start_time: u64) -> ProcessRecord {
        ProcessRecord {
            process_id,
            parent_id,
            start_time,
            precise_start_time: None,
            resident_bytes: u64::from(process_id) * 1000,
        }
    }

    fn precise(record: ProcessRecord, precise_start_time: u64) -> ProcessRecord {
        ProcessRecord {
            precise_start_time: Some(precise_start_time),
            ..record
        }
    }

    fn process_ids(tree: &[ProcessRecord]) -> Vec<u32> {
        tree.iter().map(|record| record.process_id).collect()
    }

    #[test]
    fn process_tree_holds_every_descendant_and_nothing_else() {
        let table = [
            record(1, None, 0),
            record(10, Some(1), 100),
            // The wrapper's clangd, and what clangd started.
            record(11, Some(10), 101),
            record(12, Some(11), 102),
            record(13, Some(10), 103),
            record(20, Some(1), 100),
            record(21, Some(20), 104),
            // Its parent of the same id exited long before process 10
            // started.
            record(30, Some(10), 50),
            // A parent naming itself.
            record(40, Some(40), 100),
        ];
        assert_eq!(process_ids(&process_tree(10, &table)), [10, 11, 13, 12]);
        assert_eq!(
            tree_resident_bytes(10, &table),
            Some((10 + 11 + 12 + 13) * 1000)
        );
        assert_eq!(process_ids(&process_tree(12, &table)), [12]);
        assert_eq!(tree_resident_bytes(12, &table), Some(12 * 1000));
        assert_eq!(process_ids(&process_tree(40, &table)), [40]);
        assert_eq!(process_tree(99, &table), []);
        assert_eq!(tree_resident_bytes(99, &table), None);
    }

    // On Windows an orphan keeps its exited parent's id, which a process
    // started in the same second may be given.
    #[test]
    fn a_process_of_the_same_second_is_a_child_only_if_started_no_earlier() {
        let table = [
            precise(record(10, Some(1), 100), 1_000),
            precise(record(11, Some(10), 100), 1_000),
            precise(record(12, Some(10), 100), 1_005),
            precise(record(13, Some(10), 100), 999),
            precise(record(14, Some(13), 100), 999),
            record(15, Some(10), 100),
        ];
        let tree = process_ids(&process_tree(10, &table));
        if cfg!(windows) {
            assert_eq!(tree, [10, 11, 12]);
        } else {
            assert_eq!(tree, [10, 11, 12, 15]);
        }
        assert_eq!(process_ids(&process_tree(13, &table)), [13, 14]);
    }

    fn long_running_process() -> util::command::Child {
        let mut command = if cfg!(windows) {
            let mut command = util::command::new_command("ping");
            command.args(["-n", "120", "127.0.0.1"]);
            command
        } else {
            let mut command = util::command::new_command("sleep");
            command.arg("120");
            command
        };
        command
            .stdin(util::command::Stdio::null())
            .stdout(util::command::Stdio::null())
            .stderr(util::command::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("spawning a long-running process")
    }

    fn exited_within(child: &mut util::command::Child, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if matches!(child.try_status(), Ok(Some(_))) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    #[test]
    fn a_child_process_is_measured_and_killed_with_its_parent() {
        let mut child = long_running_process();
        let child_id = child.id();
        let table = process_table();
        let tree = process_tree(std::process::id(), &table);
        assert_eq!(
            tree.first().map(|own| own.process_id),
            Some(std::process::id())
        );
        let child_record = tree
            .iter()
            .find(|record| record.process_id == child_id)
            .copied()
            .unwrap_or_else(|| panic!("the child {child_id} is not in the tree {tree:?}"));
        assert!(child_record.resident_bytes > 0, "{child_record:?}");
        #[cfg(target_os = "linux")]
        for thread in std::fs::read_dir("/proc/self/task").expect("listing own threads") {
            let thread_id = thread
                .expect("reading own threads")
                .file_name()
                .to_string_lossy()
                .parse::<u32>()
                .expect("a thread id");
            assert!(
                thread_id == std::process::id()
                    || tree.iter().all(|record| record.process_id != thread_id),
                "thread {thread_id} counted as a process"
            );
        }
        let child_record = descendants_of(std::process::id())
            .into_iter()
            .find(|record| record.process_id == child_id)
            .unwrap_or_else(|| panic!("the child {child_id} is not a descendant"));
        if cfg!(windows) {
            assert!(
                child_record.precise_start_time.is_some(),
                "{child_record:?}"
            );
        }

        let reused_id = ProcessRecord {
            start_time: child_record.start_time.saturating_sub(60),
            precise_start_time: None,
            ..child_record
        };
        let reused_in_the_same_second = ProcessRecord {
            precise_start_time: child_record
                .precise_start_time
                .map(|start| start.saturating_sub(1)),
            ..child_record
        };
        assert_eq!(kill_remaining(&[reused_id]), 0);
        if cfg!(windows) {
            assert_eq!(kill_remaining(&[reused_in_the_same_second]), 0);
        }
        assert!(
            !exited_within(&mut child, Duration::from_millis(300)),
            "a process started at another time is left alone"
        );

        assert_eq!(kill_remaining(&[child_record]), 1);
        assert!(exited_within(&mut child, Duration::from_secs(10)));
        assert_eq!(kill_remaining(&[child_record]), 0, "gone already");
    }

    // A wrapper that does not exec clangd: stopping the server kills only the
    // wrapper, and what it started is listed before and killed after.
    #[test]
    fn what_a_killed_wrapper_started_is_killed_after_it() {
        let mut command = if cfg!(windows) {
            let mut command = util::command::new_command("cmd");
            command.args(["/c", "ping -n 120 127.0.0.1 & exit"]);
            command
        } else {
            let mut command = util::command::new_command("sh");
            command.args(["-c", "sleep 120; exit"]);
            command
        };
        let mut wrapper = command
            .stdin(util::command::Stdio::null())
            .stdout(util::command::Stdio::null())
            .stderr(util::command::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("spawning a wrapper");
        let deadline = Instant::now() + Duration::from_secs(10);
        let started_program = if cfg!(windows) { "ping" } else { "sleep" };
        let descendants = loop {
            let descendants = descendants_of(wrapper.id());
            let mut system = sysinfo::System::new();
            system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
            let program_started = descendants.iter().any(|record| {
                system
                    .process(sysinfo::Pid::from_u32(record.process_id))
                    .is_some_and(|process| {
                        process
                            .name()
                            .to_string_lossy()
                            .to_lowercase()
                            .starts_with(started_program)
                    })
            });
            if program_started {
                break descendants;
            }
            assert!(Instant::now() < deadline, "the wrapper started nothing");
            std::thread::sleep(Duration::from_millis(50));
        };
        assert!(
            descendants
                .iter()
                .all(|record| record.process_id != std::process::id()),
            "{descendants:?}"
        );

        wrapper.kill().expect("killing the wrapper");
        assert!(exited_within(&mut wrapper, Duration::from_secs(10)));
        let table = process_table();
        let survivors = descendants
            .iter()
            .filter(|listed| {
                table.iter().any(|record| {
                    record.process_id == listed.process_id && record.start_time == listed.start_time
                })
            })
            .count();
        assert!(
            survivors > 0,
            "the wrapper's child outlives the wrapper: {descendants:?}"
        );

        assert!(kill_remaining(&descendants) > 0);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let table = process_table();
            let left = descendants
                .iter()
                .filter(|listed| {
                    table.iter().any(|record| {
                        record.process_id == listed.process_id
                            && record.start_time == listed.start_time
                    })
                })
                .collect::<Vec<_>>();
            if left.is_empty() {
                break;
            }
            assert!(Instant::now() < deadline, "left running: {left:?}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn restarts_are_throttled_per_worktree() {
        let mut throttle = RestartThrottle::default();
        let worktree = WorktreeId::from_usize(1);
        let other_worktree = WorktreeId::from_usize(2);
        let start = Instant::now();
        assert_eq!(
            throttle.over_limit(worktree, start),
            OverLimitAction::Restart
        );
        let soon = start + MEMORY_CHECK_INTERVAL;
        assert_eq!(
            throttle.over_limit(worktree, soon),
            OverLimitAction::NotifyOnly
        );
        assert_eq!(
            throttle.over_limit(worktree, soon + MEMORY_CHECK_INTERVAL),
            OverLimitAction::Nothing
        );
        assert_eq!(
            throttle.over_limit(other_worktree, soon),
            OverLimitAction::Restart
        );
        let later = start + MIN_RESTART_INTERVAL;
        assert_eq!(
            throttle.over_limit(worktree, later),
            OverLimitAction::Restart
        );
        assert_eq!(
            throttle.over_limit(worktree, later + MEMORY_CHECK_INTERVAL),
            OverLimitAction::NotifyOnly,
            "a new restart makes the next refusal worth telling again"
        );
    }
}
