use std::{
    cell::Cell,
    fmt,
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use acp_thread::{AcpThread, AcpThreadEvent, MentionUri, ThreadStatus, line_range_suffix};
use agent::{ContextServerRegistry, SharedThread, ThreadStore};
use agent_client_protocol::schema::v1 as acp;
use agent_servers::AgentServer;
use agent_settings::UserAgentsMd;
use collections::HashSet;
use db::kvp::{Dismissable, KeyValueStore};
use itertools::Itertools;
use project::{AgentId, ProjectItem};
use serde::{Deserialize, Serialize};

use zed_actions::{
    DecreaseBufferFontSize, IncreaseBufferFontSize, ResetBufferFontSize,
    agent::{
        AddSelectionToThread, ConflictContent, LogoutAgent, OpenSettings, ReauthenticateAgent,
        ResetAgentZoom, ResetOnboarding, ResolveConflictedFilesWithAgent,
        ResolveConflictsWithAgent, ReviewBranchDiff, SelectAgent,
    },
    assistant::{
        FocusAgent, ManageSkills, OpenGlobalAgentsMdRules, OpenProjectAgentsMdRules, Toggle,
        ToggleFocus,
    },
};

use crate::ExpandMessageEditor;
use crate::ManageProfiles;
use crate::agent_connection_store::AgentConnectionStore;
use crate::completion_provider::{AgentContextSelection, AgentContextSource};
use crate::panel_thread_history::{
    MIN_THREAD_WIDTH_BESIDE_HISTORY, OpenTabId, OpenTabRow, PANEL_THREAD_HISTORY_WIDTH,
    PanelThreadHistory, PanelThreadHistoryEvent, SLIM_PANEL_THREAD_HISTORY_WIDTH,
};
use crate::terminal_thread_metadata_store::{
    TerminalThreadMetadata, TerminalThreadMetadataStore, compose_terminal_thread_title,
    terminal_title_without_prefix,
};
use crate::thread_item::ThreadItem;
use crate::thread_metadata_store::{ThreadId, ThreadMetadataStore, ThreadMetadataStoreEvent};
use crate::{
    Agent, AgentInitialContent, AgentThreadSource, ExternalSourcePrompt, NewExternalAgentThread,
    NewNativeAgentThreadFromSummary, OpenThreadInSplit, OpenThreadInTab,
};
use crate::{
    AgentDiffPane, ConversationView, CopyThreadToClipboard, Follow, LoadThreadFromClipboard,
    NewTerminalThread, NewThread, OpenActiveThreadAsMarkdown, OpenAgentDiff, ResetFastModeWarnings,
    ResetTrialEndUpsell, ResetTrialUpsell, ShowAllSidebarThreadMetadata, ShowThreadMetadata,
    ToggleNewThreadMenu, ToggleOptionsMenu, TogglePanelTabBar, TogglePanelThreadHistory,
    conversation_view::{
        AcpThreadViewEvent, RootThreadUpdated, ThreadView, reset_fast_mode_warnings,
    },
    ui::{AgentNotification, AgentNotificationEvent, EndTrialUpsell},
};
use agent_settings::AgentSettings;
use ai_onboarding::AgentPanelOnboarding;
use anyhow::{Context as _, Result, anyhow};
use async_recursion::async_recursion;
#[cfg(feature = "audio")]
use audio::{Audio, Sound};
use chrono::{DateTime, Utc};
use client::UserStore;
use cloud_api_types::Plan;
use collections::HashMap;
use editor::{Editor, MultiBuffer};
use extension_host::ExtensionStore;
use feature_flags::{CreateThreadToolFeatureFlag, FeatureFlagAppExt as _};

use fs::Fs;
use futures::FutureExt as _;
use gpui::{
    Action, Anchor, Animation, AnimationExt, AnyElement, App, AsyncWindowContext, Axis, Bounds,
    ClipboardItem, Entity, EntityId, EventEmitter, ExternalPaths, FocusHandle, Focusable,
    KeyContext, MouseDownEvent, Pixels, PlatformDisplay, Point, Subscription, Task, TaskExt,
    WeakEntity, WindowHandle, canvas, prelude::*, pulsating_between,
};
use language::LanguageRegistry;
use language_model::LanguageModelRegistry;
use notifications::status_toast::StatusToast;
use project::{Project, ProjectPath, Worktree};
use settings::TerminalDockPosition;
use settings::{NotifyWhenAgentWaiting, PanelTabBar, Settings, update_settings_file};

use search::{BufferSearchBar, buffer_search::Deploy as DeployBufferSearch};
use terminal::{Event as TerminalEvent, terminal_settings::TerminalSettings};
use terminal_view::{TerminalView, terminal_panel::TerminalPanel};
use text::OffsetRangeExt;
use theme_settings::ThemeSettings;
use ui::{
    ContextMenu, ContextMenuEntry, GradientFade, IconButton, KeyBinding, PopoverMenu,
    PopoverMenuHandle, ProjectEmptyState, Tab, Tooltip, prelude::*, utils::WithRemSize,
};
use util::{ResultExt as _, TryFutureExt as _};
use workspace::{
    CollaboratorId, DraggedSelection, DraggedTab, ItemId, Member, MultiWorkspace, Pane, PaneAxis,
    PaneGroup, PathList, SerializedPathList, SplitDirection, SplitMode, ToggleWorkspaceSidebar,
    ToggleZoom, ToolbarItemView, Workspace, WorkspaceId,
    dock::{DockPosition, Panel, PanelEvent},
    item::{ItemEvent, ItemHandle},
    pane,
};

const AGENT_PANEL_KEY: &str = "agent_panel";
const AGENT_PANEL_LAYOUT_KEY: &str = "agent_panel_layout";
const MIN_PANEL_WIDTH: Pixels = px(300.);
const LAST_USED_AGENT_KEY: &str = "agent_panel__last_used_external_agent";
const LAST_CREATED_ENTRY_KIND_KEY: &str = "agent_panel__last_created_entry_kind";
const TERMINAL_AGENT_TELEMETRY_ID: &str = "terminal";
const TERMINAL_INIT_COMMAND_STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const KNOWN_TERMINAL_AGENT_COMMANDS: &[&str] = &[
    "agent", // Unfortunately, both Cursor cli + grok
    "agy",
    "aider",
    "amp",
    "claude",
    "codex",
    "copilot",
    "crush",
    "devin",
    "droid",
    "gemini",
    "goose",
    "grok",
    "openhands",
    "opencode",
    "pi",
    "qwen",
];

fn is_known_terminal_agent_command(command: &str) -> bool {
    KNOWN_TERMINAL_AGENT_COMMANDS.contains(&command)
}

/// The agent CLI above that the shell command line `command` starts, if any
/// of its commands starts one, however it is named: on any line or after `;`,
/// `|` or `&` (so `&&` and `||` too), past `NAME=value`, `env`, `call`,
/// `start`, `cmd /c` and `pwsh -c`, by a path, quoted, or with `.exe`, `.cmd`,
/// `.bat` or `.ps1`. The setting is the user's own, so this only has to catch
/// what a user would type, not what someone would hide.
fn terminal_agent_started_by(command: &str) -> Option<&'static str> {
    shell_commands(command)
        .iter()
        .find_map(|words| terminal_agent_started_by_words(words))
}

/// A word of a shell command line, and whether any of it was quoted.
#[derive(Default)]
struct ShellWord {
    text: String,
    quoted: bool,
}

/// `command` split into its commands, and those into words: quotes group a
/// word and are dropped, and a line break or an unquoted `;`, `|` or `&` ends
/// a command.
fn shell_commands(command: &str) -> Vec<Vec<ShellWord>> {
    fn end_word(commands: &mut [Vec<ShellWord>], word: &mut Option<ShellWord>) {
        if let (Some(word), Some(command)) = (word.take(), commands.last_mut()) {
            command.push(word);
        }
    }

    let mut commands = vec![Vec::new()];
    let mut word: Option<ShellWord> = None;
    let mut quote = None;
    for character in command.chars() {
        if let Some(open) = quote {
            if character == open {
                quote = None;
            } else {
                word.get_or_insert_with(ShellWord::default)
                    .text
                    .push(character);
            }
            continue;
        }
        match character {
            '"' | '\'' => {
                quote = Some(character);
                word.get_or_insert_with(ShellWord::default).quoted = true;
            }
            '\n' | '\r' | ';' | '|' | '&' => {
                end_word(&mut commands, &mut word);
                commands.push(Vec::new());
            }
            character if character.is_whitespace() => end_word(&mut commands, &mut word),
            character => word
                .get_or_insert_with(ShellWord::default)
                .text
                .push(character),
        }
    }
    end_word(&mut commands, &mut word);
    commands.retain(|words| !words.is_empty());
    commands
}

fn terminal_agent_started_by_words(words: &[ShellWord]) -> Option<&'static str> {
    let mut rest = words;
    loop {
        let (first, tail) = rest.split_first()?;
        if is_environment_assignment(&first.text) {
            rest = tail;
            continue;
        }
        let program = program_name(&first.text);
        rest = match program.as_str() {
            "call" => tail,
            "env" => {
                let options = tail
                    .iter()
                    .take_while(|word| word.text.starts_with('-'))
                    .count();
                tail.get(options..)?
            }
            "start" => {
                // `start "title" program`: the first quoted word is the
                // window's title, then come options.
                let tail = match tail {
                    [title, _, ..] if title.quoted => tail.get(1..)?,
                    _ => tail,
                };
                let options = tail
                    .iter()
                    .take_while(|word| word.text.starts_with('/'))
                    .count();
                tail.get(options..)?
            }
            "cmd" => {
                return inner_shell_command(tail, |flag| {
                    flag.eq_ignore_ascii_case("/c") || flag.eq_ignore_ascii_case("/k")
                });
            }
            "pwsh" | "powershell" => {
                return inner_shell_command(tail, |flag| {
                    flag.eq_ignore_ascii_case("-c") || flag.eq_ignore_ascii_case("-command")
                });
            }
            _ => {
                return KNOWN_TERMINAL_AGENT_COMMANDS
                    .iter()
                    .copied()
                    .find(|known| *known == program);
            }
        };
    }
}

/// What the command after a shell's command flag starts, when it has one.
fn inner_shell_command(
    words: &[ShellWord],
    is_command_flag: impl Fn(&str) -> bool,
) -> Option<&'static str> {
    let flag = words.iter().position(|word| is_command_flag(&word.text))?;
    let command = words
        .get(flag + 1..)?
        .iter()
        .map(|word| word.text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    terminal_agent_started_by(&command)
}

/// `NAME=value`, which a POSIX shell sets for the command that follows.
fn is_environment_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && !name.starts_with(|character: char| character.is_ascii_digit())
            && name
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '_')
    })
}

/// A program word's name: its last path component, lowercased, without the
/// extension of a Windows executable or script.
fn program_name(word: &str) -> String {
    let word = word.to_ascii_lowercase();
    let name = word.rsplit(['/', '\\']).next().unwrap_or(&word);
    [".exe", ".cmd", ".bat", ".ps1"]
        .iter()
        .find_map(|extension| name.strip_suffix(extension))
        .unwrap_or(name)
        .to_string()
}

fn terminal_program_to_report(
    last_observed_program: &mut Option<String>,
    current_program: Option<String>,
) -> Option<String> {
    let current_program =
        current_program.filter(|program| is_known_terminal_agent_command(program));
    let program_to_report =
        if current_program.is_some() && current_program != *last_observed_program {
            current_program.clone()
        } else {
            None
        };
    *last_observed_program = current_program;
    program_to_report
}

/// Maximum number of idle threads kept in the agent panel's retained list.
/// Set as a GPUI global to override; otherwise defaults to 5.
pub struct MaxIdleRetainedThreads(pub usize);
impl gpui::Global for MaxIdleRetainedThreads {}

impl MaxIdleRetainedThreads {
    pub fn global(cx: &App) -> usize {
        cx.try_global::<Self>().map_or(5, |g| g.0)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct TerminalId(uuid::Uuid);

impl TerminalId {
    pub(crate) fn new() -> Self {
        Self(uuid::Uuid::new_v4())
    }

    pub(crate) fn to_key_string(self) -> String {
        self.0.hyphenated().to_string()
    }

    pub(crate) fn from_key_string(key: &str) -> anyhow::Result<Self> {
        Ok(Self(uuid::Uuid::parse_str(key)?))
    }
}

impl fmt::Display for TerminalId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Clone, Debug)]
pub struct AgentPanelTerminalInfo {
    pub id: TerminalId,
    pub title: SharedString,
    pub created_at: DateTime<Utc>,
    pub has_notification: bool,
    pub custom_title: Option<SharedString>,
    pub working_directory: Option<PathBuf>,
}

#[derive(Serialize, Deserialize)]
struct LastUsedAgent {
    agent: Agent,
}

#[derive(Serialize, Deserialize)]
struct LastCreatedEntryKind {
    entry_kind: AgentPanelEntryKind,
}

struct SourcePanelInitialization {
    agent: Agent,
    initial_content: Option<AgentInitialContent>,
}

/// Reads the most recently used agent across all workspaces. Used as a fallback
/// when opening a workspace that has no per-workspace agent preference yet.
fn read_global_last_used_agent(kvp: &KeyValueStore) -> Option<Agent> {
    kvp.read_kvp(LAST_USED_AGENT_KEY)
        .log_err()
        .flatten()
        .and_then(|json| serde_json::from_str::<LastUsedAgent>(&json).log_err())
        .map(|entry| entry.agent)
}

async fn write_global_last_used_agent(kvp: KeyValueStore, agent: Agent) {
    if let Some(json) = serde_json::to_string(&LastUsedAgent { agent }).log_err() {
        kvp.write_kvp(LAST_USED_AGENT_KEY.to_string(), json)
            .await
            .log_err();
    }
}

fn read_global_last_created_entry_kind(kvp: &KeyValueStore) -> Option<AgentPanelEntryKind> {
    kvp.read_kvp(LAST_CREATED_ENTRY_KIND_KEY)
        .log_err()
        .flatten()
        .and_then(|json| serde_json::from_str::<LastCreatedEntryKind>(&json).log_err())
        .map(|entry| entry.entry_kind)
}

fn project_agents_md_path(
    project: &Entity<Project>,
    require_existing_file: bool,
    cx: &App,
) -> Option<PathBuf> {
    let rel_path = util::rel_path::RelPath::from_unix_str("AGENTS.md").ok()?;
    project
        .read(cx)
        .visible_worktrees(cx)
        .next()
        .and_then(|worktree| {
            let worktree = worktree.read(cx);

            if require_existing_file {
                let entry = worktree.entry_for_path(rel_path)?;
                if !entry.is_file() {
                    return None;
                }
            }

            Some(worktree.absolutize(rel_path))
        })
}

fn open_global_rules(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    workspace
        .open_abs_path(
            paths::agents_file().clone(),
            workspace::OpenOptions {
                focus: Some(true),
                ..Default::default()
            },
            window,
            cx,
        )
        .detach_and_log_err(cx);
}

fn open_project_rules(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    if let Some(path) = project_agents_md_path(workspace.project(), false, cx) {
        workspace
            .open_abs_path(
                path,
                workspace::OpenOptions {
                    focus: Some(true),
                    ..Default::default()
                },
                window,
                cx,
            )
            .detach_and_log_err(cx);
    }
}

async fn write_global_last_created_entry_kind(kvp: KeyValueStore, entry_kind: AgentPanelEntryKind) {
    if let Some(json) = serde_json::to_string(&LastCreatedEntryKind { entry_kind }).log_err() {
        kvp.write_kvp(LAST_CREATED_ENTRY_KIND_KEY.to_string(), json)
            .await
            .log_err();
    }
}

fn read_serialized_panel(
    workspace_id: workspace::WorkspaceId,
    kvp: &KeyValueStore,
) -> Option<SerializedAgentPanel> {
    let scope = kvp.scoped(AGENT_PANEL_KEY);
    let key = i64::from(workspace_id).to_string();
    scope
        .read(&key)
        .log_err()
        .flatten()
        .and_then(|json| serde_json::from_str::<SerializedAgentPanel>(&json).log_err())
}

async fn save_serialized_panel(
    workspace_id: workspace::WorkspaceId,
    panel: SerializedAgentPanel,
    kvp: KeyValueStore,
) -> Result<()> {
    let scope = kvp.scoped(AGENT_PANEL_KEY);
    let key = i64::from(workspace_id).to_string();
    scope.write(key, serde_json::to_string(&panel)?).await?;
    Ok(())
}

/// Migration: reads the original single-panel format stored under the
/// `"agent_panel"` KVP key before per-workspace keying was introduced.
fn read_legacy_serialized_panel(kvp: &KeyValueStore) -> Option<SerializedAgentPanel> {
    kvp.read_kvp(AGENT_PANEL_KEY)
        .log_err()
        .flatten()
        .and_then(|json| serde_json::from_str::<SerializedAgentPanel>(&json).log_err())
}

/// 復元中のエージェントパネル (ワークスペースごと)。`load` が返るまでは
/// `Workspace::panel::<AgentPanel>` では見つからないので、その間だけここに置く。
#[derive(Default)]
struct LoadingAgentPanels(HashMap<EntityId, WeakEntity<AgentPanel>>);

impl gpui::Global for LoadingAgentPanels {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThreadTitleRegenerationResult {
    NotOpen,
    Started,
    NoModel,
    AlreadyGenerating,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
enum AgentPanelEntryKind {
    #[default]
    Thread,
    Terminal,
}

#[derive(Serialize, Deserialize, Debug)]
struct SerializedAgentPanel {
    selected_agent: Option<Agent>,
    #[serde(default)]
    last_created_entry_kind: AgentPanelEntryKind,
    #[serde(default)]
    last_active_thread: Option<SerializedActiveThread>,
    #[serde(default)]
    last_active_terminal_id: Option<String>,
    #[serde(default)]
    new_draft_thread_id: Option<ThreadId>,
}

#[derive(Serialize, Deserialize, Debug)]
struct SerializedActiveThread {
    /// For drafts this is `None`; use `thread_id` to address them instead.
    session_id: Option<String>,
    /// Optional for back-compat with older serialized payloads that only carried `session_id`.
    #[serde(default)]
    thread_id: Option<ThreadId>,
    agent_type: Agent,
    title: Option<String>,
    work_dirs: Option<SerializedPathList>,
}

// ---------------------------------------------------------------------------
// パネル内レイアウトの永続化 (ADR-F36 Phase B / ADR-F37)
// ---------------------------------------------------------------------------

/// 分割の形とタブの並びは [`SerializedAgentPanel`] とは別の鍵に置く。
/// 両者は別々のきっかけで書かれるので、1 行にまとめると read-modify-write で
/// 取りこぼす。タブの中身は種類ごとの `SerializableItem` が自分の DB 行に持つ
/// ので、ここには「どの種類のタブがどの順で並んでいたか」だけを残す。
#[derive(Debug, Serialize, Deserialize)]
struct SerializedAgentPanelLayout {
    root: SerializedAgentPaneGroup,
}

#[derive(Debug, Serialize, Deserialize)]
enum SerializedAgentPaneGroup {
    Pane(SerializedAgentPane),
    Group {
        axis: SerializedAxis,
        flexes: Option<Vec<f32>>,
        children: Vec<SerializedAgentPaneGroup>,
    },
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct SerializedAgentPane {
    active: bool,
    #[serde(default)]
    items: Vec<SerializedPanelItem>,
}

/// パネルのタブ 1 枚。中央の `SerializedItem` と同じ 4 つ組で、種類ごとの復元は
/// `workspace::deserialize_serializable_item` に投げる (ADR-F37)。
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct SerializedPanelItem {
    kind: Arc<str>,
    item_id: ItemId,
    active: bool,
    #[serde(default)]
    preview: bool,
}

/// `gpui::Axis` は `serde` を実装していないので、読める文字列で持つ
/// (ターミナル / ブラウザーパネルの `SerializedAxis` と同じ形式)。
#[derive(Debug)]
struct SerializedAxis(Axis);

impl Serialize for SerializedAxis {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self.0 {
            Axis::Horizontal => serializer.serialize_str("horizontal"),
            Axis::Vertical => serializer.serialize_str("vertical"),
        }
    }
}

impl<'de> Deserialize<'de> for SerializedAxis {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let axis = String::deserialize(deserializer)?;
        match axis.as_str() {
            "horizontal" => Ok(SerializedAxis(Axis::Horizontal)),
            "vertical" => Ok(SerializedAxis(Axis::Vertical)),
            invalid => Err(serde::de::Error::custom(format!(
                "Invalid axis value: '{invalid}'"
            ))),
        }
    }
}

fn read_serialized_layout(
    workspace_id: WorkspaceId,
    kvp: &KeyValueStore,
) -> Option<SerializedAgentPanelLayout> {
    kvp.scoped(AGENT_PANEL_LAYOUT_KEY)
        .read(&i64::from(workspace_id).to_string())
        .log_err()
        .flatten()
        .and_then(|json| serde_json::from_str::<SerializedAgentPanelLayout>(&json).log_err())
}

fn serialize_pane_group(
    member: &Member,
    active_pane: &Entity<Pane>,
    cx: &App,
) -> SerializedAgentPaneGroup {
    match member {
        Member::Axis(PaneAxis {
            axis,
            members,
            flexes,
            bounding_boxes: _,
        }) => SerializedAgentPaneGroup::Group {
            axis: SerializedAxis(*axis),
            children: members
                .iter()
                .map(|member| serialize_pane_group(member, active_pane, cx))
                .collect(),
            flexes: Some(flexes.lock().clone()),
        },
        Member::Pane(pane) => {
            SerializedAgentPaneGroup::Pane(serialize_pane(pane, pane == active_pane, cx))
        }
    }
}

/// 種類を問わず、復元できるタブをすべて書き出す (ADR-F37)。
fn serialize_pane(pane: &Entity<Pane>, active: bool, cx: &App) -> SerializedAgentPane {
    let active_item_index = pane.read(cx).active_item_index();
    let preview_item_id = pane.read(cx).preview_item_id();
    let items = pane
        .read(cx)
        .items()
        .enumerate()
        .filter_map(|(index, item)| {
            let handle = item.to_serializable_item_handle(cx)?;
            Some(SerializedPanelItem {
                kind: Arc::from(handle.serialized_item_kind()),
                item_id: item.item_id().as_u64(),
                active: index == active_item_index,
                preview: Some(item.item_id()) == preview_item_id,
            })
        })
        .collect();
    SerializedAgentPane { active, items }
}

/// 保存したレイアウトを組み立て直す。1 枚も復元できなければ `false`。
async fn restore_layout(
    panel: &Entity<AgentPanel>,
    serialized: SerializedAgentPanelLayout,
    workspace_id: Option<WorkspaceId>,
    cx: &mut AsyncWindowContext,
) -> bool {
    let Some((root, active_pane)) =
        deserialize_pane_group(panel, &serialized.root, workspace_id, cx).await
    else {
        return false;
    };
    panel
        .update_in(cx, |panel, window, cx| {
            panel.center = PaneGroup::with_root(root);
            panel.active_pane = active_pane.unwrap_or_else(|| panel.center.first_pane());
            panel.refresh_tabbed_threads(cx);
            panel.sync_base_view_from_pane(window, cx);
        })
        .log_err()
        .is_some()
}

#[async_recursion(?Send)]
async fn deserialize_pane_group(
    panel: &Entity<AgentPanel>,
    serialized: &SerializedAgentPaneGroup,
    workspace_id: Option<WorkspaceId>,
    cx: &mut AsyncWindowContext,
) -> Option<(Member, Option<Entity<Pane>>)> {
    match serialized {
        SerializedAgentPaneGroup::Group {
            axis,
            flexes,
            children,
        } => {
            let mut active_pane = None;
            let mut members = Vec::new();
            for child in children {
                if let Some((member, child_active)) =
                    deserialize_pane_group(panel, child, workspace_id, cx).await
                {
                    members.push(member);
                    active_pane = active_pane.or(child_active);
                }
            }
            match members.len() {
                0 => None,
                1 => Some((members.remove(0), active_pane)),
                _ => Some((
                    Member::Axis(PaneAxis::load(axis.0, members, flexes.clone())),
                    active_pane,
                )),
            }
        }
        SerializedAgentPaneGroup::Pane(serialized_pane) => {
            if serialized_pane.items.is_empty() {
                return None;
            }
            let pane = panel
                .update_in(cx, |panel, window, cx| {
                    new_thread_pane(panel.workspace.clone(), panel.project.clone(), window, cx)
                })
                .log_err()?;
            restore_pane_items(panel, &pane, &serialized_pane.items, workspace_id, cx).await;
            // 1 枚も戻せなかったペインは畳む (空のペインはタブバーすら描かれない)。
            if pane.read_with(cx, |pane, _| pane.items_len()) == 0 {
                return None;
            }
            Some((
                Member::Pane(pane.clone()),
                serialized_pane.active.then_some(pane),
            ))
        }
    }
}

/// 種類ごとの復元。`workspace` に登録の無い種類は飛ばす。
async fn restore_pane_items(
    panel: &Entity<AgentPanel>,
    pane: &Entity<Pane>,
    items: &[SerializedPanelItem],
    workspace_id: Option<WorkspaceId>,
    cx: &mut AsyncWindowContext,
) {
    let Some(workspace_id) = workspace_id else {
        log::info!("agent panel: this window has no workspace id, so its tabs cannot be restored");
        return;
    };
    let (workspace, project) = panel.read_with(cx, |panel, _| {
        (panel.workspace.clone(), panel.project.clone())
    });

    let mut tasks = Vec::new();
    for item in items {
        let task = pane.update_in(cx, |_, window, cx| {
            workspace::deserialize_serializable_item(
                &item.kind,
                project.clone(),
                workspace.clone(),
                workspace_id,
                item.item_id,
                window,
                cx,
            )
        });
        match task {
            Ok(Some(task)) => tasks.push((item.kind.clone(), item.active, task)),
            Ok(None) => log::info!(
                "agent panel: no restore handler for a {:?} tab; skipping it",
                item.kind
            ),
            Err(error) => {
                log::warn!("agent panel: the window went away while restoring its tabs: {error:#}");
                return;
            }
        }
    }

    let mut active_index = None;
    let mut next_index = 0;
    for (kind, active, task) in tasks {
        // 復元できないタブは飛ばすだけ。中身の記録が (終了間際に足したタブの
        // ように) 書かれていなかった、というのが主な理由なので、警告ではなく
        // 記録として残す。
        let item = match task.await {
            Ok(item) => item,
            Err(error) => {
                log::info!(
                    "agent panel: skipping a {kind:?} tab that could not be restored: {error:#}"
                );
                continue;
            }
        };
        // 裏で足すときの挿し先は `Pane` に任せると「表のタブの次」になって
        // 並びが崩れるので、末尾を明示する。
        let added = pane
            .update_in(cx, |pane, window, cx| {
                pane.add_item_inner(item, false, false, false, Some(next_index), window, cx);
            })
            .log_err()
            .is_some();
        if added {
            if active {
                active_index = Some(next_index);
            }
            next_index += 1;
        }
    }
    pane.update_in(cx, |pane, window, cx| {
        pane.activate_item(active_index.unwrap_or(0), false, false, window, cx);
    })
    .log_err();
}

pub fn init(cx: &mut App) {
    workspace::register_serializable_item::<ThreadItem>(cx);
    cx.observe_new(
        |workspace: &mut Workspace, _window, _cx: &mut Context<Workspace>| {
            workspace
                .register_action(|workspace, _: &OpenThreadInTab, window, cx| {
                    ThreadItem::open_active_panel_thread(workspace, None, window, cx);
                })
                .register_action(|workspace, _: &OpenThreadInSplit, window, cx| {
                    ThreadItem::open_active_panel_thread(
                        workspace,
                        Some(workspace::SplitDirection::Right),
                        window,
                        cx,
                    );
                })
                .register_action(|workspace, _: &NewThread, window, cx| {
                    if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
                        panel.update(cx, |panel, cx| {
                            panel.new_thread_with_workspace(Some(workspace), window, cx)
                        });
                        workspace.focus_panel::<AgentPanel>(window, cx);
                    }
                })
                .register_action(|workspace, _: &NewTerminalThread, window, cx| {
                    if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
                        panel.update(cx, |panel, cx| {
                            panel.new_terminal(
                                Some(workspace),
                                AgentThreadSource::AgentPanel,
                                window,
                                cx,
                            )
                        });
                        workspace.focus_panel::<AgentPanel>(window, cx);
                    }
                })
                .register_action(
                    |workspace, action: &NewNativeAgentThreadFromSummary, window, cx| {
                        if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
                            panel.update(cx, |panel, cx| {
                                panel.new_native_agent_thread_from_summary(action, window, cx)
                            });
                            workspace.focus_panel::<AgentPanel>(window, cx);
                        }
                    },
                )
                .register_action(|workspace, _: &ExpandMessageEditor, window, cx| {
                    if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
                        workspace.focus_panel::<AgentPanel>(window, cx);
                        panel.update(cx, |panel, cx| panel.expand_message_editor(window, cx));
                    }
                })
                .register_action(|workspace, _: &OpenSettings, window, cx| {
                    if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
                        workspace.focus_panel::<AgentPanel>(window, cx);
                        panel.update(cx, |panel, cx| panel.open_configuration(window, cx));
                    }
                })
                .register_action(|workspace, action: &NewExternalAgentThread, window, cx| {
                    if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
                        workspace.focus_panel::<AgentPanel>(window, cx);
                        panel.update(cx, |panel, cx| {
                            panel.new_external_agent_thread(action, window, cx);
                        });
                    }
                })
                .register_action(|workspace, action: &SelectAgent, window, cx| {
                    if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
                        panel.update(cx, |panel, cx| {
                            let agent = AgentId::new(action.agent.clone()).into();
                            panel.select_agent(agent, window, cx);
                        });
                    }
                })
                .register_action(|workspace, action: &ManageSkills, window, cx| {
                    if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
                        workspace.focus_panel::<AgentPanel>(window, cx);
                        panel.update(cx, |panel, cx| panel.manage_skills(action, window, cx));
                    }
                })
                .register_action(|workspace, _: &OpenGlobalAgentsMdRules, window, cx| {
                    open_global_rules(workspace, window, cx);
                })
                .register_action(|workspace, _: &OpenProjectAgentsMdRules, window, cx| {
                    open_project_rules(workspace, window, cx);
                })
                .register_action(|workspace, _: &Follow, window, cx| {
                    workspace.follow(CollaboratorId::Agent, window, cx);
                })
                .register_action(|workspace, _: &OpenAgentDiff, window, cx| {
                    let thread = workspace
                        .panel::<AgentPanel>(cx)
                        .and_then(|panel| panel.read(cx).active_conversation_view().cloned())
                        .and_then(|conversation| {
                            conversation
                                .read(cx)
                                .root_thread_view()
                                .map(|r| r.read(cx).thread.clone())
                        });

                    if let Some(thread) = thread {
                        AgentDiffPane::deploy_in_workspace(thread, workspace, window, cx);
                    }
                })
                .register_action(|workspace, _: &ToggleOptionsMenu, window, cx| {
                    if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
                        workspace.focus_panel::<AgentPanel>(window, cx);
                        panel.update(cx, |panel, cx| {
                            panel.toggle_options_menu(&ToggleOptionsMenu, window, cx);
                        });
                    }
                })
                .register_action(|workspace, _: &ToggleNewThreadMenu, window, cx| {
                    if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
                        workspace.focus_panel::<AgentPanel>(window, cx);
                        panel.update(cx, |panel, cx| {
                            panel.toggle_new_thread_menu(&ToggleNewThreadMenu, window, cx);
                        });
                    }
                })
                .register_action(|workspace, _: &TogglePanelThreadHistory, window, cx| {
                    if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
                        panel.update(cx, |panel, cx| {
                            panel.toggle_panel_thread_history(
                                &TogglePanelThreadHistory,
                                window,
                                cx,
                            );
                        });
                    }
                })
                .register_action(|workspace, _: &TogglePanelTabBar, window, cx| {
                    if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
                        panel.update(cx, |panel, cx| {
                            panel.toggle_panel_tab_bar(&TogglePanelTabBar, window, cx);
                        });
                    }
                })
                .register_action(|_workspace, _: &ResetOnboarding, window, cx| {
                    window.dispatch_action(workspace::RestoreBanner.boxed_clone(), cx);
                    window.refresh();
                })
                .register_action(|workspace, _: &ResetTrialUpsell, _window, cx| {
                    if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
                        panel.update(cx, |panel, _| {
                            panel
                                .new_user_onboarding_upsell_dismissed
                                .store(false, Ordering::Release);
                        });
                    }
                    OnboardingUpsell::set_dismissed(false, cx);
                })
                .register_action(|_workspace, _: &ResetTrialEndUpsell, _window, cx| {
                    TrialEndUpsell::set_dismissed(false, cx);
                })
                .register_action(|_workspace, _: &ResetFastModeWarnings, _window, cx| {
                    reset_fast_mode_warnings(cx);
                })
                .register_action(|workspace, _: &ResetAgentZoom, window, cx| {
                    if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
                        panel.update(cx, |panel, cx| {
                            panel.reset_agent_zoom(window, cx);
                        });
                    }
                })
                .register_action(|workspace, _: &CopyThreadToClipboard, window, cx| {
                    if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
                        panel.update(cx, |panel, cx| {
                            panel.copy_thread_to_clipboard(window, cx);
                        });
                    }
                })
                .register_action(|workspace, _: &LoadThreadFromClipboard, window, cx| {
                    if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
                        workspace.focus_panel::<AgentPanel>(window, cx);
                        panel.update(cx, |panel, cx| {
                            panel.load_thread_from_clipboard(window, cx);
                        });
                    }
                })
                .register_action(|workspace, _: &ShowThreadMetadata, window, cx| {
                    if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
                        panel.update(cx, |panel, cx| {
                            panel.show_thread_metadata(&ShowThreadMetadata, window, cx);
                        });
                    }
                })
                .register_action(|workspace, _: &ShowAllSidebarThreadMetadata, window, cx| {
                    if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
                        panel.update(cx, |panel, cx| {
                            panel.show_all_sidebar_thread_metadata(
                                &ShowAllSidebarThreadMetadata,
                                window,
                                cx,
                            );
                        });
                    }
                })
                .register_action(|workspace, action: &ReviewBranchDiff, window, cx| {
                    if workspace.panel::<AgentPanel>(cx).is_none() {
                        return;
                    }

                    let base_ref = action.base_ref.to_string();
                    let diff_text = action.diff_text.to_string();
                    let project = workspace.project().clone();
                    // Without the repository the diff's files cannot be
                    // checked against the project and `private_files`, so
                    // nothing is sent.
                    let Some(repository) = project.read(cx).active_repository(cx) else {
                        workspace.show_error(
                            anyhow::anyhow!("レビューするリポジトリが見つかりません。"),
                            cx,
                        );
                        return;
                    };
                    cx.spawn_in(window, async move |workspace, cx| {
                        // The review sends a summary with a larger share of
                        // the patch than a chat attachment, rather than the
                        // whole patch of a branch that may touch hundreds of
                        // files.
                        const REVIEW_DIFF_BYTES: usize = 64 * 1024;
                        let content = agent::branch_diff_for_agent(
                            project,
                            repository,
                            &diff_text,
                            Some(agent::GitDiffSummaryRequest::branch(
                                &base_ref,
                                true,
                                REVIEW_DIFF_BYTES,
                            )),
                            cx,
                        )
                        .await;

                        let mention_uri = MentionUri::GitDiff { base_ref };
                        let diff_uri = mention_uri.to_uri().to_string();

                        let content_blocks = vec![
                            acp::ContentBlock::Text(acp::TextContent::new(
                                "Please review this branch diff carefully. Point out any issues, \
                                 potential bugs, or improvement opportunities you find.\n\n"
                                    .to_string(),
                            )),
                            acp::ContentBlock::Resource(acp::EmbeddedResource::new(
                                acp::EmbeddedResourceResource::TextResourceContents(
                                    acp::TextResourceContents::new(content, diff_uri),
                                ),
                            )),
                        ];

                        workspace.update_in(cx, |workspace, window, cx| {
                            let Some(panel) = workspace.panel::<AgentPanel>(cx) else {
                                return;
                            };
                            workspace.focus_panel::<AgentPanel>(window, cx);

                            panel.update(cx, |panel, cx| {
                                panel.external_thread(
                                    None,
                                    None,
                                    None,
                                    None,
                                    Some(AgentInitialContent::ContentBlock {
                                        blocks: content_blocks,
                                        auto_submit: true,
                                    }),
                                    true,
                                    AgentThreadSource::GitPanel,
                                    window,
                                    cx,
                                );
                            });
                        })
                    })
                    .detach_and_log_err(cx);
                })
                .register_action(
                    |workspace, action: &ResolveConflictsWithAgent, window, cx| {
                        let Some(panel) = workspace.panel::<AgentPanel>(cx) else {
                            return;
                        };

                        let content_blocks = build_conflict_resolution_prompt(&action.conflicts);

                        workspace.focus_panel::<AgentPanel>(window, cx);

                        panel.update(cx, |panel, cx| {
                            panel.external_thread(
                                None,
                                None,
                                None,
                                None,
                                Some(AgentInitialContent::ContentBlock {
                                    blocks: content_blocks,
                                    auto_submit: true,
                                }),
                                true,
                                AgentThreadSource::GitPanel,
                                window,
                                cx,
                            );
                        });
                    },
                )
                .register_action(
                    |workspace, action: &ResolveConflictedFilesWithAgent, window, cx| {
                        let Some(panel) = workspace.panel::<AgentPanel>(cx) else {
                            return;
                        };

                        let content_blocks =
                            build_conflicted_files_resolution_prompt(&action.conflicted_file_paths);

                        workspace.focus_panel::<AgentPanel>(window, cx);

                        panel.update(cx, |panel, cx| {
                            panel.external_thread(
                                None,
                                None,
                                None,
                                None,
                                Some(AgentInitialContent::ContentBlock {
                                    blocks: content_blocks,
                                    auto_submit: true,
                                }),
                                true,
                                AgentThreadSource::GitPanel,
                                window,
                                cx,
                            );
                        });
                    },
                )
                .register_action(
                    |workspace: &mut Workspace, _: &AddSelectionToThread, window, cx| {
                        let active_editor = workspace
                            .active_item(cx)
                            .and_then(|item| item.act_as::<Editor>(cx));
                        let has_editor_selection = active_editor.is_some_and(|editor| {
                            editor.update(cx, |editor, cx| {
                                editor.has_non_empty_selection(&editor.display_snapshot(cx))
                            })
                        });

                        let has_terminal_selection = workspace
                            .active_item(cx)
                            .and_then(|item| item.act_as::<TerminalView>(cx))
                            .is_some_and(|terminal_view| {
                                terminal_view
                                    .read(cx)
                                    .terminal()
                                    .read(cx)
                                    .last_content
                                    .selection_text
                                    .as_ref()
                                    .is_some_and(|text| !text.is_empty())
                            });

                        let has_terminal_panel_selection =
                            workspace.panel::<TerminalPanel>(cx).is_some_and(|panel| {
                                let position = match TerminalSettings::get_global(cx).dock {
                                    TerminalDockPosition::Left => DockPosition::Left,
                                    TerminalDockPosition::Bottom => DockPosition::Bottom,
                                    TerminalDockPosition::Right => DockPosition::Right,
                                };
                                let dock_is_open =
                                    workspace.dock_at_position(position).read(cx).is_open();
                                dock_is_open && !panel.read(cx).terminal_selections(cx).is_empty()
                            });

                        if !has_editor_selection
                            && !has_terminal_selection
                            && !has_terminal_panel_selection
                        {
                            return;
                        }

                        let Some(agent_panel) = workspace.panel::<AgentPanel>(cx) else {
                            return;
                        };

                        let source = AgentContextSource::from_focused(workspace, window, cx);
                        let source = source.or_else(|| {
                            let cached = agent_panel.read(cx).last_context_source.clone()?;
                            cached.exists(workspace, cx).then_some(cached)
                        });
                        let source =
                            source.or_else(|| AgentContextSource::from_active(workspace, cx));

                        let Some(source) = source else {
                            return;
                        };

                        let Some(selection) = source.read_selection(workspace, true, cx) else {
                            return;
                        };

                        if !agent_panel.focus_handle(cx).contains_focused(window, cx) {
                            workspace.toggle_panel_focus::<AgentPanel>(window, cx);
                        }

                        agent_panel.update(cx, |panel, cx| {
                            panel.last_context_source = Some(source);
                            cx.defer_in(window, move |panel, window, cx| {
                                if let Some(conversation_view) = panel.active_conversation_view() {
                                    conversation_view.update(cx, |conversation_view, cx| {
                                        conversation_view.insert_selection(selection, window, cx);
                                    });
                                } else if let Some(terminal_id) = panel.active_terminal_id()
                                    && let Some(agent_terminal) = panel.terminals.get(&terminal_id)
                                {
                                    // Resolve mentions against the cwd: live cwd, else spawn dir.
                                    let terminal = agent_terminal.view.read(cx).terminal().read(cx);
                                    let working_directory = terminal
                                        .working_directory()
                                        .or_else(|| agent_terminal.working_directory.clone());
                                    let tab_key =
                                        crate::sandboxed_claude_code::cli_tab_session_key(terminal);
                                    let runs_claude_code =
                                        tab_key.is_some() || runs_claude_code_on_host(terminal);
                                    let text = mention_selection_in_terminal(
                                        &selection,
                                        &panel.project,
                                        working_directory.as_deref(),
                                        tab_key.as_deref(),
                                        runs_claude_code,
                                        cx,
                                    );
                                    if !text.is_empty() {
                                        let view = agent_terminal.view.clone();
                                        view.update(cx, |view, cx| {
                                            view.terminal().update(cx, |terminal, _| {
                                                terminal.paste(&text);
                                            });
                                            window.focus(&view.focus_handle(cx), cx);
                                        });
                                    }
                                }
                            });
                        });
                    },
                )
                .register_action(|workspace, _: &menu::Cancel, _window, cx| {
                    if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
                        let dismissed =
                            panel.update(cx, |panel, cx| panel.dismiss_all_notifications(cx));
                        if dismissed {
                            return;
                        }
                    }
                    cx.propagate();
                });
        },
    )
    .detach();
}

/// The task id of the Claude Code the launch actions start on this machine
/// (`terminal_view::session_tools::claude_code_launch_task`).
const HOST_CLAUDE_CODE_TASK_ID: &str = "terminal::launch-claude-code";

/// ADR-F56: whether `terminal` runs Claude Code outside a sandbox — started by the
/// launch action, or typed into the tab. Only such a tab's selection may go through
/// the host's IDE server: its `at_mentioned` reaches whichever Claude Code is
/// connected there, which is not the CLI of a tab running another agent.
fn runs_claude_code_on_host(terminal: &terminal::Terminal) -> bool {
    terminal
        .task()
        .is_some_and(|task| task.spawned_task.id.0 == HOST_CLAUDE_CODE_TASK_ID)
        || terminal
            .foreground_process_command_name()
            .is_some_and(|program| program_name(&program) == "claude")
}

/// ADR-F56: what to paste into a CLI tab for `selection`. In a tab that runs Claude
/// Code (`runs_claude_code`), each editor range goes to the CLI as an IDE
/// `at_mentioned` when it is connected to Zed's IDE server (the tab's own sandbox
/// session, or the host server outside a sandbox) and is then left out of the text;
/// otherwise it becomes the CLI's own `@<path>#L<a>-<b>` reference, with the path as
/// the CLI sees it (mapped into the sandbox). Any other tab gets the plain
/// `<path>:<a>-<b>` mention, as before. A terminal selection is pasted as it is.
fn mention_selection_in_terminal(
    selection: &AgentContextSelection,
    project: &Entity<Project>,
    working_directory: Option<&std::path::Path>,
    tab_key: Option<&str>,
    runs_claude_code: bool,
    cx: &mut App,
) -> String {
    match selection {
        AgentContextSelection::Editor(ranges) if !runs_claude_code => {
            let path_style = project.read(cx).path_style(cx);
            let mut parts: Vec<String> = Vec::new();
            for (buffer, range) in ranges {
                let buffer = buffer.read(cx);
                let Some(project_path) = buffer.project_path(cx) else {
                    continue;
                };
                let snapshot = buffer.snapshot();
                let point_range = range.to_point(&snapshot);
                let line_range = point_range.start.row..=point_range.end.row;
                let path = mention_path_for_terminal(
                    project,
                    &project_path,
                    working_directory,
                    path_style,
                    cx,
                );
                parts.push(format!("{path}{}", line_range_suffix(&line_range)));
            }
            if parts.is_empty() {
                String::new()
            } else {
                // Trailing space so the mention doesn't fuse with the next input.
                format!("{} ", parts.join(" "))
            }
        }
        AgentContextSelection::Editor(ranges) => {
            let path_style = project.read(cx).path_style(cx);
            let mut parts: Vec<String> = Vec::new();
            for (buffer, range) in ranges {
                let (project_path, start_row, end_row) = {
                    let buffer = buffer.read(cx);
                    let Some(project_path) = buffer.project_path(cx) else {
                        continue;
                    };
                    let snapshot = buffer.snapshot();
                    let point_range = range.to_point(&snapshot);
                    // Selecting whole lines ends the selection at column 0 of the next
                    // line, which is not part of it.
                    let end_row = if point_range.end.row > point_range.start.row
                        && point_range.end.column == 0
                    {
                        point_range.end.row - 1
                    } else {
                        point_range.end.row
                    };
                    (project_path, point_range.start.row, end_row)
                };
                let Some(abs_path) = project.read(cx).absolute_path(&project_path, cx) else {
                    continue;
                };
                let cli_path = match ide_server::mention_selection_in_cli(
                    tab_key, project, &abs_path, start_row, end_row, cx,
                ) {
                    ide_server::CliMention::Sent | ide_server::CliMention::Unreachable => continue,
                    ide_server::CliMention::Paste {
                        cli_path: Some(cli_path),
                    } => cli_path,
                    ide_server::CliMention::Paste { cli_path: None } => mention_path_for_terminal(
                        project,
                        &project_path,
                        working_directory,
                        path_style,
                        cx,
                    ),
                };
                parts.push(format!(
                    "@{cli_path}{}",
                    cli_line_range_suffix(start_row, end_row)
                ));
            }
            if parts.is_empty() {
                String::new()
            } else {
                // Trailing space so the mention doesn't fuse with the next input.
                format!("{} ", parts.join(" "))
            }
        }
        AgentContextSelection::Terminal(texts) => texts.join("\n"),
    }
}

/// The CLI's own line reference (`#L3`, `#L3-7`) for 0-based rows.
fn cli_line_range_suffix(start_row: u32, end_row: u32) -> String {
    let start = start_row + 1;
    let end = end_row + 1;
    if start == end {
        format!("#L{start}")
    } else {
        format!("#L{start}-{end}")
    }
}

/// Path for a terminal mention: relative to the terminal cwd if possible, else absolute.
fn mention_path_for_terminal(
    project: &Entity<Project>,
    project_path: &ProjectPath,
    working_directory: Option<&std::path::Path>,
    path_style: util::paths::PathStyle,
    cx: &App,
) -> String {
    let abs_path = project.read(cx).absolute_path(project_path, cx);
    match (abs_path, working_directory) {
        (Some(abs_path), Some(working_directory)) => path_style
            .strip_prefix(&abs_path, working_directory)
            .map(|relative| relative.display(path_style).into_owned())
            .unwrap_or_else(|| abs_path.to_string_lossy().into_owned()),
        (Some(abs_path), None) => abs_path.to_string_lossy().into_owned(),
        (None, _) => project_path.path.display(path_style).into_owned(),
    }
}

fn conflict_resource_block(conflict: &ConflictContent) -> acp::ContentBlock {
    let mention_uri = MentionUri::MergeConflict {
        file_path: conflict.file_path.clone(),
    };
    acp::ContentBlock::Resource(acp::EmbeddedResource::new(
        acp::EmbeddedResourceResource::TextResourceContents(acp::TextResourceContents::new(
            conflict.conflict_text.clone(),
            mention_uri.to_uri().to_string(),
        )),
    ))
}

fn build_conflict_resolution_prompt(conflicts: &[ConflictContent]) -> Vec<acp::ContentBlock> {
    if conflicts.is_empty() {
        return Vec::new();
    }

    let mut blocks = Vec::new();

    if conflicts.len() == 1 {
        let conflict = &conflicts[0];

        blocks.push(acp::ContentBlock::Text(acp::TextContent::new(
            "Please resolve the following merge conflict in ",
        )));
        let mention = MentionUri::File {
            abs_path: PathBuf::from(conflict.file_path.clone()),
        };
        blocks.push(acp::ContentBlock::ResourceLink(acp::ResourceLink::new(
            mention.name(),
            mention.to_uri(),
        )));

        blocks.push(acp::ContentBlock::Text(acp::TextContent::new(
            indoc::formatdoc!(
                "\nThe conflict is between branch `{ours}` (ours) and `{theirs}` (theirs).

                Analyze both versions carefully and resolve the conflict by editing \
                the file directly. Choose the resolution that best preserves the intent \
                of both changes, or combine them if appropriate.

                ",
                ours = conflict.ours_branch_name,
                theirs = conflict.theirs_branch_name,
            ),
        )));
    } else {
        let n = conflicts.len();
        let unique_files: HashSet<&str> = conflicts.iter().map(|c| c.file_path.as_str()).collect();
        let ours = &conflicts[0].ours_branch_name;
        let theirs = &conflicts[0].theirs_branch_name;
        blocks.push(acp::ContentBlock::Text(acp::TextContent::new(
            indoc::formatdoc!(
                "Please resolve all {n} merge conflicts below.

                The conflicts are between branch `{ours}` (ours) and `{theirs}` (theirs).

                For each conflict, analyze both versions carefully and resolve them \
                by editing the file{suffix} directly. Choose resolutions that best preserve \
                the intent of both changes, or combine them if appropriate.

                ",
                suffix = if unique_files.len() > 1 { "s" } else { "" },
            ),
        )));
    }

    for conflict in conflicts {
        blocks.push(conflict_resource_block(conflict));
    }

    blocks
}

fn build_conflicted_files_resolution_prompt(
    conflicted_file_paths: &[String],
) -> Vec<acp::ContentBlock> {
    if conflicted_file_paths.is_empty() {
        return Vec::new();
    }

    let instruction = indoc::indoc!(
        "The following files have unresolved merge conflicts. Please open each \
         file, find the conflict markers (`<<<<<<<` / `=======` / `>>>>>>>`), \
         and resolve every conflict by editing the files directly.

         Choose resolutions that best preserve the intent of both changes, \
         or combine them if appropriate.

         Files with conflicts:
         ",
    );

    let mut content = vec![acp::ContentBlock::Text(acp::TextContent::new(instruction))];
    for path in conflicted_file_paths {
        let mention = MentionUri::File {
            abs_path: PathBuf::from(path),
        };
        content.push(acp::ContentBlock::ResourceLink(acp::ResourceLink::new(
            mention.name(),
            mention.to_uri(),
        )));
        content.push(acp::ContentBlock::Text(acp::TextContent::new("\n")));
    }
    content
}

fn format_timestamp_human(dt: &DateTime<Utc>) -> String {
    let now = Utc::now();
    let duration = now.signed_duration_since(*dt);

    let relative = if duration.num_seconds() < 0 {
        "未来".to_string()
    } else if duration.num_seconds() < 60 {
        let seconds = duration.num_seconds();
        format!("{seconds} 秒前")
    } else if duration.num_minutes() < 60 {
        let minutes = duration.num_minutes();
        format!("{minutes} 分前")
    } else if duration.num_hours() < 24 {
        let hours = duration.num_hours();
        format!("{hours} 時間前")
    } else {
        let days = duration.num_days();
        format!("{days} 日前")
    };

    format!("{} ({})", dt.to_rfc3339(), relative)
}

/// Used for `dev: show thread metadata` action
fn thread_metadata_to_debug_json(
    metadata: &crate::thread_metadata_store::ThreadMetadata,
) -> serde_json::Value {
    serde_json::json!({
        "thread_id": metadata.thread_id,
        "session_id": metadata.session_id.as_ref().map(|s| s.0.to_string()),
        "agent_id": metadata.agent_id.0.to_string(),
        "title": metadata.title.as_ref().map(|t| t.to_string()),
        "title_override": metadata.title_override.as_ref().map(|t| t.to_string()),
        "updated_at": format_timestamp_human(&metadata.updated_at),
        "created_at": metadata.created_at.as_ref().map(format_timestamp_human),
        "interacted_at": metadata.interacted_at.as_ref().map(format_timestamp_human),
        "worktree_paths": format!("{:?}", metadata.worktree_paths),
        "archived": metadata.archived,
    })
}

/// Optional parameters for `AgentPanel::create_thread_with_options`. All
/// fields default to the panel's current selection so the agent tool only
/// needs to override what it actually cares about.
#[derive(Default)]
pub struct CreateThreadOptions {
    /// Title to assign to the new thread up front.
    pub title: Option<SharedString>,
    /// Initial content to populate in the thread (optionally auto-submitted).
    pub initial_content: Option<AgentInitialContent>,
    /// Agent to use. Defaults to the panel's selected agent.
    pub agent: Option<Agent>,
    /// Model override, as `provider/model-id`. Only applied when the thread
    /// uses the native Zed agent.
    pub model: Option<String>,
    /// Working directories to attach to the new thread (e.g., the path of a
    /// freshly-created sibling worktree). When `None`, the thread inherits
    /// the project's default path list.
    pub work_dirs: Option<PathList>,
}

pub(crate) struct AgentThread {
    conversation_view: Entity<ConversationView>,
}

struct AgentTerminal {
    view: Entity<TerminalView>,
    title_editor: Option<Entity<Editor>>,
    title_editor_initial_title: Option<String>,
    title_editor_subscription: Option<Subscription>,
    last_known_title: String,
    last_known_terminal_title: String,
    last_observed_program: Option<String>,
    working_directory: Option<PathBuf>,
    created_at: DateTime<Utc>,
    has_notification: bool,
    search_bar: Option<Entity<BufferSearchBar>>,
    notification_windows: Vec<WindowHandle<AgentNotification>>,
    notification_subscriptions: Vec<Subscription>,
    _subscriptions: Vec<Subscription>,
}

impl AgentTerminal {
    fn terminal_title_for_view(view: &TerminalView, cx: &App) -> SharedString {
        let terminal = view.terminal().read(cx);
        if terminal.breadcrumb_text.is_empty() {
            let title = terminal.title(true);
            if title == "Terminal" {
                SharedString::from("")
            } else {
                title.into()
            }
        } else {
            terminal.breadcrumb_text.clone().into()
        }
    }

    fn current_terminal_title(&self, cx: &App) -> SharedString {
        let view = self.view.read(cx);
        Self::terminal_title_for_view(view, cx)
    }

    fn terminal_title(&self, cx: &App) -> SharedString {
        let title = self.current_terminal_title(cx);
        if title.is_empty() && !self.last_known_terminal_title.is_empty() {
            SharedString::from(self.last_known_terminal_title.clone())
        } else {
            title
        }
    }

    fn title(&self, cx: &App) -> SharedString {
        let terminal_title = self.terminal_title(cx);
        let custom_title = self.custom_title(cx);
        compose_terminal_thread_title(
            terminal_title.as_ref(),
            custom_title.as_ref().map(|title| title.as_ref()),
        )
    }

    fn editable_title(&self, cx: &App) -> SharedString {
        if let Some(custom_title) = self.custom_title(cx) {
            custom_title
        } else {
            let terminal_title = self.terminal_title(cx);
            SharedString::from(terminal_title_without_prefix(terminal_title.as_ref()).to_string())
        }
    }

    fn refresh_title(&mut self, cx: &mut App) -> bool {
        let terminal_title = self.current_terminal_title(cx);
        if !terminal_title.is_empty() {
            self.last_known_terminal_title = terminal_title.to_string();
        }

        let title = self.title(cx);
        let changed = self.last_known_title != title.as_ref();
        if changed {
            self.last_known_title = title.to_string();
        }
        changed
    }

    fn refresh_metadata(&mut self, cx: &mut App) -> bool {
        let title_changed = self.refresh_title(cx);
        let current_working_directory = self.view.read(cx).terminal().read(cx).working_directory();
        let working_directory_changed = current_working_directory
            .as_ref()
            .is_some_and(|current| self.working_directory.as_ref() != Some(current));
        if working_directory_changed {
            self.working_directory = current_working_directory;
        }
        title_changed || working_directory_changed
    }

    fn custom_title(&self, cx: &App) -> Option<SharedString> {
        self.view.read(cx).custom_title().map(SharedString::from)
    }

    fn report_started_terminal_program(
        &mut self,
        terminal_id: TerminalId,
        source: AgentThreadSource,
        cx: &App,
    ) {
        let current_program = self
            .view
            .read(cx)
            .terminal()
            .read(cx)
            .foreground_process_command_name();

        if let Some(program) =
            terminal_program_to_report(&mut self.last_observed_program, current_program)
        {
            telemetry::event!(
                "Agent Terminal Program Started",
                agent = TERMINAL_AGENT_TELEMETRY_ID,
                terminal_id = terminal_id.to_key_string(),
                program = program,
                source = source.as_str(),
                side = crate::agent_sidebar_side(cx),
                thread_location = "current_worktree",
            );
        }
    }
}

enum BaseView {
    Uninitialized,
    AgentThread {
        conversation_view: Entity<ConversationView>,
    },
    Terminal {
        terminal_id: TerminalId,
    },
    /// スレッドではないタブ ([`AgentPanel::add_terminal_tab`] が置く公式
    /// Claude Code CLI の端末、他所からドラッグしてきたタブ) が前に出ている
    /// (ADR-F53)。描くのはペイングループなので、パネルはどのアイテムが表に
    /// 出ているかだけを覚える。
    PaneItem {
        item_id: EntityId,
    },
}

impl From<AgentThread> for BaseView {
    fn from(thread: AgentThread) -> Self {
        BaseView::AgentThread {
            conversation_view: thread.conversation_view,
        }
    }
}

enum VisibleSurface<'a> {
    Uninitialized,
    AgentThread(&'a Entity<ConversationView>),
    Terminal(&'a Entity<TerminalView>),
    /// スレッドではないタブ。中身はペイングループが描く。
    PaneItem,
}

enum WhichFontSize {
    AgentFont,
    None,
}

/// スレッド履歴の一覧に出せる幅 (ADR-F53)。パネルはドックの幅で決まるので、
/// 「出す / 出さない」の 2 択だと少し狭めただけで一覧ごと消えてしまう。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ThreadHistoryFit {
    Full,
    /// 題名 1 行だけの細い一覧。
    Slim,
    /// 一覧の置き場が無い = 一覧を開くボタンも出さない。
    None,
}

impl BaseView {
    pub fn which_font_size_used(&self) -> WhichFontSize {
        match self {
            BaseView::AgentThread { .. } => WhichFontSize::AgentFont,
            BaseView::Terminal { .. } | BaseView::PaneItem { .. } | BaseView::Uninitialized => {
                WhichFontSize::None
            }
        }
    }
}

pub struct AgentPanel {
    workspace: WeakEntity<Workspace>,
    /// Workspace id is used as a database key
    workspace_id: Option<WorkspaceId>,
    user_store: Entity<UserStore>,
    project: Entity<Project>,
    fs: Arc<dyn Fs>,
    language_registry: Arc<LanguageRegistry>,
    thread_store: Entity<ThreadStore>,
    connection_store: Entity<AgentConnectionStore>,
    context_server_registry: Entity<ContextServerRegistry>,
    focus_handle: FocusHandle,
    base_view: BaseView,
    /// パネルの中のタブ列 (ADR-F36 Phase B)。スレッドは 1 本 = 1 枚の
    /// [`ThreadItem`] としてここに載る。`base_view` の `AgentThread` は
    /// 「[`Self::active_pane`] のアクティブなアイテム」の写しで、両者は
    /// [`Self::sync_base_view_from_pane`] / [`Self::set_base_view`] で
    /// 同期する。
    center: PaneGroup,
    active_pane: Entity<Pane>,
    /// パネルに載っているスレッドの索引。`Panel::panes()` を辿れば同じものが
    /// 得られるが、`&App` を取れない呼び出し元 ([`Self::conversation_views`])
    /// のために持つ。更新はペインのイベントごとに丸ごと作り直す。
    tabbed_threads: HashMap<ThreadId, Entity<ConversationView>>,
    /// 保存したレイアウトを組み立てている最中。この間の保存は「まだ空の
    /// パネル」を書いてしまうので止める。
    restoring_layout: bool,
    pending_layout_serialization: Task<Option<()>>,
    last_created_entry_kind: AgentPanelEntryKind,
    draft_thread: Option<Entity<ConversationView>>,
    /// The window this panel is in. The idle disconnect reaches the panel from
    /// a sweep that has no window of its own, and parking a draft needs one
    /// ([`ConversationView::release_idle_connection`]).
    window_handle: gpui::AnyWindowHandle,
    retained_threads: HashMap<ThreadId, Entity<ConversationView>>,
    terminals: HashMap<TerminalId, AgentTerminal>,
    pending_terminal_spawn: Option<TerminalId>,
    new_thread_menu_handle: PopoverMenuHandle<ContextMenu>,
    agent_panel_menu_handle: PopoverMenuHandle<ContextMenu>,
    thread_history: Entity<PanelThreadHistory>,
    /// Written by a measuring canvas during paint and read on the next render,
    /// so the history list can hide itself when the panel gets too narrow to
    /// hold both halves.
    measured_panel_width: Rc<Cell<Option<Pixels>>>,
    _thread_history_subscription: Subscription,
    _extension_subscription: Option<Subscription>,
    _project_subscription: Subscription,
    zoomed: bool,
    pending_serialization: Option<Task<Result<()>>>,
    new_user_onboarding: Entity<AgentPanelOnboarding>,
    new_user_onboarding_upsell_dismissed: AtomicBool,
    selected_agent: Agent,
    _thread_view_subscription: Option<Subscription>,
    _active_thread_focus_subscription: Option<Subscription>,
    _base_view_observation: Option<Subscription>,
    _draft_editor_observation: Option<Subscription>,
    _active_draft_reclaim_observation: Option<Subscription>,
    _thread_metadata_store_subscription: Subscription,
    last_context_source: Option<AgentContextSource>,

    is_active: bool,
}

impl AgentPanel {
    fn serialize(&mut self, cx: &mut App) {
        let Some(workspace_id) = self.workspace_id else {
            return;
        };

        let selected_agent = self.selected_agent.clone();
        let last_created_entry_kind = self.last_created_entry_kind;
        let last_active_terminal_id = self
            .active_terminal_id()
            .map(|terminal_id| terminal_id.to_key_string());

        let last_active_thread = if last_active_terminal_id.is_some() {
            None
        } else {
            let is_draft_active = self.active_thread_is_draft(cx);
            let active_thread_id = self.active_thread_id(cx);
            let active_thread_agent = self
                .active_conversation_view()
                .map(|cv| cv.read(cx).agent_key().clone())
                .unwrap_or_else(|| self.selected_agent.clone());
            self.active_agent_thread(cx)
                .map(|thread| {
                    let thread = thread.read(cx);

                    let title = thread.title();
                    let work_dirs = thread.work_dirs().cloned();
                    SerializedActiveThread {
                        session_id: (!is_draft_active).then(|| thread.session_id().0.to_string()),
                        thread_id: active_thread_id,
                        agent_type: active_thread_agent.clone(),
                        title: title.map(|t| t.to_string()),
                        work_dirs: work_dirs.map(|dirs| dirs.serialize()),
                    }
                })
                .or_else(|| {
                    // The active view may be in `Loading` or `LoadError` — for
                    // example, while a restored thread is waiting for a custom
                    // agent to finish registering. Without this fallback, a
                    // stray `serialize()` triggered during that window would
                    // write `session_id=None` and wipe the restored session
                    if is_draft_active {
                        return None;
                    }
                    let conversation_view = self.active_conversation_view()?;
                    let session_id = conversation_view.read(cx).root_session_id.clone()?;
                    let metadata = ThreadMetadataStore::try_global(cx)
                        .and_then(|store| store.read(cx).entry_by_session(&session_id).cloned());
                    Some(SerializedActiveThread {
                        session_id: Some(session_id.0.to_string()),
                        thread_id: active_thread_id,
                        agent_type: active_thread_agent.clone(),
                        title: metadata
                            .as_ref()
                            .and_then(|m| m.title.as_ref())
                            .map(|t| t.to_string()),
                        work_dirs: metadata.map(|m| m.folder_paths().serialize()),
                    })
                })
        };

        let new_draft_thread_id = self
            .draft_thread
            .as_ref()
            .map(|draft| draft.read(cx).thread_id);

        let kvp = KeyValueStore::global(cx);
        self.pending_serialization = Some(cx.background_spawn(async move {
            save_serialized_panel(
                workspace_id,
                SerializedAgentPanel {
                    selected_agent: Some(selected_agent),
                    last_created_entry_kind,
                    last_active_thread,
                    last_active_terminal_id,
                    new_draft_thread_id,
                },
                kvp,
            )
            .await?;
            anyhow::Ok(())
        }));
    }

    /// 復元しなかったアイテムの行の掃除は、中央とすべてのパネルの復元が揃って
    /// からワークスペースが 1 回だけ走らせる (ADR-F37)。ここでやることは
    /// 待ち合わせへの参加だけで、失敗しても必ず抜ける。
    ///
    /// The first time that bookkeeping drains also ends the workspace's dock
    /// startup phase (ADR-F48). This runs again whenever the panel is loaded
    /// later - AI turned back on, the active workspace changing, the sidebar
    /// loading it into a workspace - and if that workspace's startup is
    /// already over, taking part again does not bring it back.
    pub fn load(
        workspace: WeakEntity<Workspace>,
        cx: AsyncWindowContext,
    ) -> Task<Result<Entity<Self>>> {
        cx.spawn(async move |cx| {
            workspace
                .update(cx, |workspace, _| workspace.begin_item_restore())
                .log_err();
            let panel = Self::load_inner(workspace.clone(), cx).await;
            workspace
                .update_in(cx, |workspace, window, cx| {
                    workspace.end_item_restore(window, cx);
                })
                .log_err();
            panel
        })
    }

    fn load_inner(
        workspace: WeakEntity<Workspace>,
        cx: &mut AsyncWindowContext,
    ) -> Task<Result<Entity<Self>>> {
        let kvp = cx.update(|_window, cx| KeyValueStore::global(cx)).ok();
        cx.spawn(async move |cx| {
            let workspace_id = workspace
                .read_with(cx, |workspace, _| workspace.database_id())
                .ok()
                .flatten();

            let (
                serialized_panel,
                serialized_layout,
                global_last_used_agent,
                global_last_created_entry_kind,
            ) = cx
                .background_spawn(async move {
                    match kvp {
                        Some(kvp) => {
                            let panel = workspace_id
                                .and_then(|id| read_serialized_panel(id, &kvp))
                                .or_else(|| read_legacy_serialized_panel(&kvp));
                            let layout =
                                workspace_id.and_then(|id| read_serialized_layout(id, &kvp));
                            let global_agent = read_global_last_used_agent(&kvp);
                            let global_entry_kind = read_global_last_created_entry_kind(&kvp);
                            (panel, layout, global_agent, global_entry_kind)
                        }
                        None => (None, None, None, None),
                    }
                })
                .await;

            let has_open_project = workspace
                .read_with(cx, |workspace, cx| !workspace.root_paths(cx).is_empty())
                .unwrap_or(false);
            let terminal_id_to_restore = if has_open_project {
                serialized_panel
                    .as_ref()
                    .and_then(|panel| panel.last_active_terminal_id.as_deref())
                    .and_then(|terminal_id| {
                        match TerminalId::from_key_string(terminal_id) {
                            Ok(terminal_id) => Some(terminal_id),
                            Err(error) => {
                                log::warn!("failed to parse last active terminal id: {error}");
                                None
                            }
                        }
                    })
            } else {
                None
            };
            let terminal_to_restore = if let Some(terminal_id) = terminal_id_to_restore {
                match cx.update(|_window, cx| {
                    TerminalThreadMetadataStore::try_global(cx).map(|store| {
                        let reload_task = store.read(cx).reload_task();
                        (store, reload_task)
                    })
                }) {
                    Ok(Some((store, reload_task))) => {
                        reload_task.await;
                        match store
                            .read_with(cx, |store, _cx| store.entry(terminal_id).cloned())
                        {
                            Some(metadata) => Some(metadata),
                            None => {
                                log::info!(
                                    "last active terminal is missing, skipping restoration"
                                );
                                None
                            }
                        }
                    }
                    Ok(None) => {
                        log::warn!("failed to restore active terminal: metadata store missing");
                        None
                    }
                    Err(err) => {
                        log::warn!("failed to access terminal metadata store: {err}");
                        None
                    }
                }
            } else {
                None
            };

            // 保存したレイアウト (タブの並び) があるなら、旧形式の
            // 「最後に見ていたスレッド 1 本」は読まない。1 世代だけ両方を読む
            // 移行期間で、レイアウトが無いときだけ旧形式が効く。
            let thread_to_restore = if has_open_project && terminal_to_restore.is_none() {
                if let Some(info) = serialized_panel
                    .as_ref()
                    .and_then(|panel| panel.last_active_thread.as_ref())
                {
                    match cx.update(|_window, cx| {
                        ThreadMetadataStore::try_global(cx).map(|store| {
                            let reload_task = store.read(cx).reload_task();
                            (store, reload_task)
                        })
                    }) {
                        Ok(Some((store, reload_task))) => {
                            reload_task.await;
                            let thread_id = store.read_with(cx, |store, _cx| {
                                let primary = info.thread_id.and_then(|tid| store.entry(tid));
                                let fallback = info.session_id.as_ref().and_then(|sid| {
                                    store.entry_by_session(&acp::SessionId::new(sid.clone()))
                                });
                                primary
                                    .or(fallback)
                                    .filter(|entry| !entry.archived)
                                    .map(|entry| entry.thread_id)
                            });
                            match thread_id {
                                Some(thread_id) => Some((info, thread_id)),
                                None => {
                                    log::info!(
                                        "last active thread is archived or missing, skipping restoration"
                                    );
                                    None
                                }
                            }
                        }
                        Ok(None) => {
                            log::warn!("failed to restore active thread: metadata store missing");
                            None
                        }
                        Err(err) => {
                            log::warn!("failed to access thread metadata store: {err}");
                            None
                        }
                    }
                } else {
                    None
                }
            } else {
                None
            };
            // 借りたままだと `serialized_panel` を動かせないので、必要な分だけ写す。
            let thread_to_restore = thread_to_restore.map(|(info, thread_id)| {
                (
                    info.agent_type.clone(),
                    info.work_dirs.clone(),
                    info.title.clone(),
                    thread_id,
                )
            });
            // レイアウト (タブの並び) があるならそちらが本命。旧形式の
            // 「最後に見ていたスレッド 1 本」は、レイアウトが 1 枚も戻せな
            // かったときの受け皿として取っておく (移行期の 1 世代)。
            let restore_thread_now = serialized_layout.is_none();
            let new_draft_thread_id = serialized_panel
                .as_ref()
                .and_then(|panel| panel.new_draft_thread_id);

            let panel = workspace.update_in(cx, |workspace, window, cx| {
                let panel = cx.new(|cx| Self::new(workspace, window, cx));

                panel.update(cx, |panel, cx| {
                    let is_via_collab = panel.project.read(cx).is_via_collab();
                    // Collab workspaces only support NativeAgent; clamp any
                    // non-native choice so `set_active` can't bypass the
                    // collab guard in `external_thread`.
                    let clamp = |agent: Agent| {
                        if is_via_collab && !agent.is_native() {
                            Agent::NativeAgent
                        } else {
                            agent
                        }
                    };
                    let global_fallback =
                        global_last_used_agent.filter(|agent| !is_via_collab || agent.is_native());

                    if let Some(serialized_panel) = &serialized_panel {
                        panel.last_created_entry_kind = serialized_panel.last_created_entry_kind;
                    } else if let Some(entry_kind) = global_last_created_entry_kind {
                        panel.last_created_entry_kind = entry_kind;
                    }

                    // The thread being restored may have been bound to an
                    // agent different from the panel's last selected one
                    // (e.g. a draft created while a different agent was
                    // active). When restoring a thread, prefer its agent
                    // so the draft survives reload bound to the right
                    // backend; otherwise fall back to the serialized
                    // selection, then the global last-used agent.
                    let initial_agent = match &thread_to_restore {
                        Some((agent_type, ..)) => Some(clamp(agent_type.clone())),
                        None => serialized_panel
                            .as_ref()
                            .and_then(|p| p.selected_agent.clone())
                            .map(clamp)
                            .or(global_fallback),
                    };
                    if let Some(agent) = initial_agent {
                        panel.selected_agent = agent;
                    }

                    if let Some(metadata) = terminal_to_restore {
                        panel.restore_terminal_for_panel_load(
                            metadata,
                            false,
                            AgentThreadSource::AgentPanel,
                            Some(workspace),
                            window,
                            cx,
                        );
                    } else if restore_thread_now
                        && let Some((_, work_dirs, title, thread_id)) = &thread_to_restore
                    {
                        let agent = panel.selected_agent.clone();
                        panel.load_agent_thread(
                            agent,
                            *thread_id,
                            work_dirs.as_ref().map(PathList::deserialize),
                            title.clone().map(Into::into),
                            false,
                            AgentThreadSource::AgentPanel,
                            window,
                            cx,
                        );
                    }
                    cx.notify();
                });

                panel
            })?;

            if let Some(layout) = serialized_layout {
                // タブの復元 (`ThreadItem::deserialize`) はエージェントパネルを
                // 借りに来るが、パネルがワークスペースに載るのは `load` が
                // 返ってから。復元のあいだだけ「読み込み中のパネル」として
                // 見せる ([`AgentPanel::loading_for_workspace`])。
                let workspace_key = workspace.entity_id();
                cx.update(|_window, cx| {
                    cx.default_global::<LoadingAgentPanels>()
                        .0
                        .insert(workspace_key, panel.downgrade());
                })
                .log_err();
                panel.update(cx, |panel, _| panel.restoring_layout = true);
                let restored = restore_layout(&panel, layout, workspace_id, cx).await;
                panel.update(cx, |panel, _| panel.restoring_layout = false);
                // 1 枚も戻せなかった (保存した行がもう無い等) ときは、旧形式の
                // 「最後に見ていたスレッド」でいつもどおり 1 本だけ開く。
                if !restored
                    && let Some((_, work_dirs, title, thread_id)) = thread_to_restore
                {
                    panel
                        .update_in(cx, |panel, window, cx| {
                            let agent = panel.selected_agent.clone();
                            panel.load_agent_thread(
                                agent,
                                thread_id,
                                work_dirs.as_ref().map(PathList::deserialize),
                                title.map(Into::into),
                                false,
                                AgentThreadSource::AgentPanel,
                                window,
                                cx,
                            );
                        })
                        .log_err();
                }
                cx.update(|_window, cx| {
                    if let Some(loading) = cx.try_global::<LoadingAgentPanels>()
                        && loading.0.contains_key(&workspace_key)
                    {
                        cx.default_global::<LoadingAgentPanels>()
                            .0
                            .remove(&workspace_key);
                    }
                })
                .log_err();
            }

            // 下書き枠の復元はタブを組み立て終えてから。先にやると、復元した
            // タブと同じスレッドをもう 1 つ作ってしまう (ADR-F36 Phase B)。
            if let Some(new_draft_thread_id) = new_draft_thread_id {
                panel
                    .update_in(cx, |panel, window, cx| {
                        panel.restore_new_draft(new_draft_thread_id, window, cx);
                        cx.notify();
                    })
                    .log_err();
            }

            Ok(panel)
        })
    }

    /// 復元中でまだワークスペースに載っていないパネル。
    pub(crate) fn loading_for_workspace(
        workspace: &WeakEntity<Workspace>,
        cx: &App,
    ) -> Option<Entity<Self>> {
        cx.try_global::<LoadingAgentPanels>()?
            .0
            .get(&workspace.entity_id())?
            .upgrade()
    }

    pub(crate) fn new(workspace: &Workspace, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let fs = workspace.app_state().fs.clone();
        let user_store = workspace.app_state().user_store.clone();
        let project = workspace.project();
        let language_registry = project.read(cx).languages().clone();
        let client = workspace.client().clone();
        let workspace_id = workspace.database_id();
        let workspace = workspace.weak_handle();

        let context_server_registry =
            cx.new(|cx| ContextServerRegistry::new(project.read(cx).context_server_store(), cx));

        let thread_store = ThreadStore::global(cx);

        let base_view = BaseView::Uninitialized;

        let weak_panel = cx.entity().downgrade();
        let onboarding = cx.new(|cx| {
            AgentPanelOnboarding::new(
                user_store.clone(),
                client,
                move |_window, cx| {
                    weak_panel
                        .update(cx, |panel, cx| {
                            panel.dismiss_ai_onboarding(cx);
                        })
                        .ok();
                },
                cx,
            )
        });

        // Subscribe to extension events to sync agent servers when extensions change
        let extension_subscription = ExtensionStore::try_global(cx).map(|store| {
            cx.subscribe(&store, |this, _source, event, cx| match event {
                extension_host::Event::ExtensionUninstalled(id) => {
                    this.migrate_agent_server_from_extensions(id.clone(), cx);
                }
                _ => {}
            })
        });

        let connection_store = cx.new(|cx| AgentConnectionStore::new(project.clone(), cx));
        let _project_subscription =
            cx.subscribe(&project, |this, _project, event, cx| match event {
                project::Event::WorktreeAdded(_)
                | project::Event::WorktreeRemoved(_)
                | project::Event::WorktreeOrderChanged
                | project::Event::WorktreePathsChanged { .. } => {
                    this.ensure_native_agent_connection(cx);
                    this.update_thread_work_dirs(cx);
                    this.persist_all_terminal_metadata(cx);
                    cx.notify();
                }
                _ => {}
            });

        let _thread_metadata_store_subscription = cx.subscribe(
            &ThreadMetadataStore::global(cx),
            |this, _store, event, cx| {
                let ThreadMetadataStoreEvent::ThreadArchived(thread_id) = event;
                if this.retained_threads.remove(thread_id).is_some() {
                    cx.notify();
                }
            },
        );

        cx.on_release(|this, cx| {
            this.dismiss_all_terminal_notifications(cx);
        })
        .detach();

        let thread_history =
            cx.new(|cx| PanelThreadHistory::new(workspace.clone(), project.clone(), window, cx));
        let _thread_history_subscription = cx.subscribe_in(
            &thread_history,
            window,
            |this, _history, event, window, cx| match event {
                PanelThreadHistoryEvent::Activate(metadata) => {
                    this.load_agent_thread(
                        Agent::from(metadata.agent_id.clone()),
                        metadata.thread_id,
                        Some(metadata.folder_paths().clone()),
                        metadata.title.clone(),
                        true,
                        AgentThreadSource::AgentPanel,
                        window,
                        cx,
                    );
                }
                PanelThreadHistoryEvent::NewThread => {
                    this.new_thread(&NewThread, window, cx);
                }
                PanelThreadHistoryEvent::ActivateOpenTab(id) => {
                    this.activate_open_tab(*id, window, cx);
                }
                PanelThreadHistoryEvent::CloseOpenTab(id) => {
                    this.close_open_tab(*id, window, cx);
                }
                PanelThreadHistoryEvent::Collapse => {
                    this.toggle_panel_thread_history(&TogglePanelThreadHistory, window, cx);
                }
                PanelThreadHistoryEvent::OpenInTab { thread, split } => {
                    let agent = Agent::from(thread.agent_id.clone());
                    let thread_id = thread.thread_id;
                    let split = *split;
                    let workspace = this.workspace.clone();
                    // The panel is mid-update here, and opening the tab has to
                    // update the panel again to take the thread out of it.
                    window.defer(cx, move |window, cx| {
                        workspace
                            .update(cx, |workspace, cx| {
                                ThreadItem::open_thread(
                                    workspace,
                                    agent,
                                    thread_id,
                                    split.then_some(workspace::SplitDirection::Right),
                                    window,
                                    cx,
                                );
                            })
                            .log_err();
                    });
                }
            },
        );

        let pane = new_thread_pane(workspace.clone(), project.clone(), window, cx);

        let panel = Self {
            workspace_id,
            base_view,
            center: PaneGroup::new(pane.clone()),
            active_pane: pane,
            tabbed_threads: HashMap::default(),
            restoring_layout: false,
            pending_layout_serialization: Task::ready(None),
            last_created_entry_kind: AgentPanelEntryKind::Thread,
            workspace,
            user_store,
            project: project.clone(),
            fs: fs.clone(),
            language_registry,
            connection_store,
            focus_handle: cx.focus_handle(),
            context_server_registry,
            draft_thread: None,
            window_handle: window.window_handle(),
            retained_threads: HashMap::default(),
            terminals: HashMap::default(),
            pending_terminal_spawn: None,
            new_thread_menu_handle: PopoverMenuHandle::default(),
            agent_panel_menu_handle: PopoverMenuHandle::default(),
            thread_history,
            measured_panel_width: Rc::new(Cell::new(None)),
            _thread_history_subscription,

            _extension_subscription: extension_subscription,
            _project_subscription,
            zoomed: false,
            pending_serialization: None,
            new_user_onboarding: onboarding,
            thread_store,
            selected_agent: Agent::default(),
            _thread_view_subscription: None,
            _active_thread_focus_subscription: None,
            new_user_onboarding_upsell_dismissed: AtomicBool::new(OnboardingUpsell::dismissed(cx)),
            _base_view_observation: None,
            _draft_editor_observation: None,
            _active_draft_reclaim_observation: None,
            _thread_metadata_store_subscription,
            last_context_source: None,
            is_active: false,
        };

        panel.ensure_native_agent_connection(cx);
        panel
    }

    pub fn toggle_focus(
        workspace: &mut Workspace,
        _: &ToggleFocus,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        if workspace
            .panel::<Self>(cx)
            .is_some_and(|panel| panel.read(cx).enabled(cx))
        {
            workspace.toggle_panel_focus::<Self>(window, cx);
        }
    }

    pub fn focus(
        workspace: &mut Workspace,
        _: &FocusAgent,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        if workspace
            .panel::<Self>(cx)
            .is_some_and(|panel| panel.read(cx).enabled(cx))
        {
            workspace.focus_panel::<Self>(window, cx);
        }
    }

    pub fn toggle(
        workspace: &mut Workspace,
        _: &Toggle,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        if workspace
            .panel::<Self>(cx)
            .is_some_and(|panel| panel.read(cx).enabled(cx))
        {
            if !workspace.toggle_panel_focus::<Self>(window, cx) {
                workspace.close_panel::<Self>(window, cx);
            }
        }
    }

    pub fn thread_store(&self) -> &Entity<ThreadStore> {
        &self.thread_store
    }

    pub fn connection_store(&self) -> &Entity<AgentConnectionStore> {
        &self.connection_store
    }

    pub fn selected_agent(&self, cx: &App) -> Agent {
        if self.project.read(cx).is_via_collab() {
            Agent::NativeAgent
        } else {
            self.selected_agent.clone()
        }
    }

    pub fn open_thread(
        &mut self,
        session_id: acp::SessionId,
        work_dirs: Option<PathList>,
        title: Option<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Share links / clipboard imports enter with only a session id. If
        // this machine already has a metadata row for the session, route
        // through the normal thread-id path.
        let existing_thread_id = ThreadMetadataStore::try_global(cx).and_then(|store| {
            store
                .read(cx)
                .entry_by_session(&session_id)
                .map(|m| m.thread_id)
        });
        if let Some(thread_id) = existing_thread_id {
            self.load_agent_thread(
                crate::Agent::NativeAgent,
                thread_id,
                work_dirs,
                title,
                true,
                AgentThreadSource::AgentPanel,
                window,
                cx,
            );
        } else {
            self.external_thread_by_session(
                crate::Agent::NativeAgent,
                session_id,
                work_dirs,
                title,
                true,
                AgentThreadSource::AgentPanel,
                window,
                cx,
            );
        }
    }

    fn external_thread_by_session(
        &mut self,
        agent: Agent,
        session_id: acp::SessionId,
        work_dirs: Option<PathList>,
        title: Option<SharedString>,
        focus: bool,
        source: AgentThreadSource,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let thread = self.create_agent_thread_with_server_for_external_session(
            agent, None, session_id, work_dirs, title, None, source, window, cx,
        );
        self.set_base_view(thread.into(), focus, window, cx);
    }

    pub(crate) fn context_server_registry(&self) -> &Entity<ContextServerRegistry> {
        &self.context_server_registry
    }

    pub fn is_visible(workspace: &Entity<Workspace>, cx: &App) -> bool {
        let workspace_read = workspace.read(cx);

        workspace_read
            .panel::<AgentPanel>(cx)
            .map(|panel| {
                let panel_id = Entity::entity_id(&panel);

                workspace_read.all_docks().iter().any(|dock| {
                    dock.read(cx)
                        .visible_panels()
                        .any(|visible_panel| visible_panel.panel_id() == panel_id)
                })
            })
            .unwrap_or(false)
    }

    /// Clear the active view, retaining any running thread in the background.
    pub fn clear_base_view(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let old_view = std::mem::replace(&mut self.base_view, BaseView::Uninitialized);
        self.retain_running_thread(old_view, cx);
        self.activate_draft(false, AgentThreadSource::AgentPanel, window, cx);
        self.serialize(cx);
        cx.emit(AgentPanelEvent::ActiveViewChanged);
        cx.notify();
    }

    pub fn new_thread(&mut self, _action: &NewThread, window: &mut Window, cx: &mut Context<Self>) {
        if !self.has_open_project(cx) {
            return;
        }

        self.new_thread_with_workspace(None, window, cx);
    }

    fn new_thread_with_workspace(
        &mut self,
        workspace: Option<&Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.should_create_terminal_for_new_entry(cx) {
            self.new_terminal(workspace, AgentThreadSource::AgentPanel, window, cx);
        } else {
            self.activate_new_thread(true, AgentThreadSource::AgentPanel, window, cx);
        }
    }

    pub fn activate_new_thread(
        &mut self,
        focus: bool,
        source: AgentThreadSource,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.has_open_project(cx) {
            return;
        }

        self.set_last_created_entry_kind_from_user_action(AgentPanelEntryKind::Thread, cx);

        // If the user is viewing a *parked* draft and the ephemeral
        // new-draft slot is occupied, pressing `+` should just focus the
        // ephemeral draft — not park it and create yet another empty one.
        // This matches the mental model of `+` as "go to my new-thread
        // slot". The parked draft will be put back into `retained_threads`
        // by `set_base_view`'s `retain_running_thread` call.
        if let Some(draft) = self.draft_thread.clone()
            && self.active_thread_is_draft(cx)
            && !self.active_view_is_new_draft(cx)
            && *draft.read(cx).agent_key() == self.selected_agent
        {
            self.set_base_view(
                BaseView::AgentThread {
                    conversation_view: draft,
                },
                focus,
                window,
                cx,
            );
            return;
        }

        if let Some(draft) = self.draft_thread.clone() {
            if self.draft_has_content(&draft, cx) {
                let draft_id = draft.read(cx).thread_id;
                self.draft_thread = None;
                self._draft_editor_observation = None;
                // タブに載っている下書きは、そのタブが持ち続ける
                // (ADR-F36 Phase B: 保持枠はタブの無いスレッドだけ)。
                if !ThreadItem::is_open_in_a_tab(draft_id, cx) {
                    self.retained_threads.insert(draft_id, draft);
                }
            } else if *draft.read(cx).agent_key() != self.selected_agent {
                let old_draft_id = draft.read(cx).thread_id;
                ThreadMetadataStore::global(cx).update(cx, |store, cx| {
                    store.delete(old_draft_id, cx);
                });
                self.draft_thread = None;
                self._draft_editor_observation = None;
            }
        }
        self.activate_draft(focus, source, window, cx);
    }

    fn draft_has_content(&self, draft: &Entity<ConversationView>, cx: &App) -> bool {
        let cv = draft.read(cx);
        // ADR-F49 (new decision): a parked draft keeps its editor outside the
        // thread, so this has to ask the view rather than the thread view.
        if let Some(message_editor) = cv.draft_message_editor(cx) {
            let text = message_editor.read(cx).text(cx);
            if !text.trim().is_empty() {
                return true;
            }
        }
        if let Some(acp_thread) = cv.root_thread(cx) {
            let thread = acp_thread.read(cx);
            if !thread.is_draft_thread() {
                return true;
            }
            if thread
                .draft_prompt()
                .is_some_and(|blocks| !blocks.is_empty())
            {
                return true;
            }
        }
        false
    }

    /// Reattaches the panel's new-draft slot to the persisted `thread_id`,
    /// seeding the editor with any prompt text from the draft-prompt kvp
    /// store.
    ///
    /// If the active view already holds this thread — because the user's
    /// last-active thread was the new-draft itself — we reuse that
    /// ConversationView instead of building a second one.
    fn restore_new_draft(
        &mut self,
        thread_id: ThreadId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.has_open_project(cx) {
            return;
        }

        // 復元したタブに同じスレッドが載っているなら、それを下書き枠に据える
        // (同じスレッドの `ConversationView` を 2 つ作らない)。
        let active_matching = match &self.base_view {
            BaseView::AgentThread { conversation_view }
                if conversation_view.read(cx).thread_id == thread_id =>
            {
                Some(conversation_view.clone())
            }
            _ => self.tabbed_threads.get(&thread_id).cloned(),
        };
        if let Some(conversation_view) = active_matching {
            self.observe_draft_editor(&conversation_view, cx);
            self.draft_thread = Some(conversation_view);
            return;
        }

        let Some(metadata) = ThreadMetadataStore::try_global(cx)
            .and_then(|store| store.read(cx).entry(thread_id).cloned())
            .filter(|m| m.is_draft())
        else {
            return;
        };

        let agent = if self.project.read(cx).is_via_collab() {
            Agent::NativeAgent
        } else {
            Agent::from(metadata.agent_id.clone())
        };
        let initial_content = crate::draft_prompt_store::read(thread_id, cx).map(|blocks| {
            AgentInitialContent::ContentBlock {
                blocks,
                auto_submit: false,
            }
        });
        let thread = self.create_agent_thread_with_server(
            agent,
            None,
            Some(thread_id),
            Some(metadata.folder_paths().clone()),
            metadata.title.clone(),
            initial_content,
            None,
            AgentThreadSource::AgentPanel,
            window,
            cx,
        );
        self.observe_draft_editor(&thread.conversation_view, cx);
        self.draft_thread = Some(thread.conversation_view);
    }

    pub fn new_external_agent_thread(
        &mut self,
        action: &NewExternalAgentThread,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.has_open_project(cx) {
            return;
        }

        self.selected_agent = action.agent.clone().into();
        self.activate_new_thread(true, AgentThreadSource::AgentPanel, window, cx);
    }

    fn set_selected_agent_and_persist(&mut self, agent: Agent, cx: &mut Context<Self>) {
        if self.selected_agent != agent {
            self.selected_agent = agent.clone();
            self.serialize(cx);
        }

        cx.background_spawn({
            let kvp = KeyValueStore::global(cx);
            async move {
                write_global_last_used_agent(kvp, agent).await;
            }
        })
        .detach();
    }

    /// Sets the panel's selected agent without opening the panel or focusing
    /// it, so the agent is launched the next time the panel is opened (or
    /// right away, if the panel is already showing the empty new-thread
    /// draft).
    pub fn select_agent(&mut self, agent: Agent, window: &mut Window, cx: &mut Context<Self>) {
        if self.project.read(cx).is_via_collab() && !agent.is_native() {
            return;
        }

        let showing_new_draft = matches!(
            (&self.base_view, &self.draft_thread),
            (BaseView::AgentThread { conversation_view }, Some(draft))
                if conversation_view.entity_id() == draft.entity_id()
        );

        if matches!(self.base_view, BaseView::AgentThread { .. }) && showing_new_draft {
            self.set_selected_agent_and_persist(agent, cx);
            self.activate_draft(false, AgentThreadSource::AgentPanel, window, cx);
            cx.notify();
        }
    }

    pub fn new_terminal(
        &mut self,
        workspace: Option<&Workspace>,
        source: AgentThreadSource,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.supports_terminal(cx) {
            return;
        }
        self.set_last_created_entry_kind_from_user_action(AgentPanelEntryKind::Terminal, cx);
        let working_directory = self.terminal_working_directory(workspace, cx);
        self.spawn_terminal(
            TerminalId::new(),
            working_directory,
            None,
            None,
            None,
            true,
            true,
            true,
            source,
            window,
            cx,
        );
    }

    fn terminal_working_directory(
        &self,
        workspace: Option<&Workspace>,
        cx: &App,
    ) -> Option<PathBuf> {
        workspace
            .map(|workspace| terminal_view::default_working_directory(workspace, cx))
            .unwrap_or_else(|| self.default_terminal_working_directory(cx))
    }

    pub fn supports_terminal(&self, cx: &App) -> bool {
        self.has_open_project(cx) && self.project.read(cx).supports_terminal(cx)
    }

    pub fn should_create_terminal_for_new_entry(&self, cx: &App) -> bool {
        self.last_created_entry_kind == AgentPanelEntryKind::Terminal
            && self.project.read(cx).supports_terminal(cx)
    }

    fn set_last_created_entry_kind_from_user_action(
        &mut self,
        entry_kind: AgentPanelEntryKind,
        cx: &mut Context<Self>,
    ) {
        if self.last_created_entry_kind != entry_kind {
            self.last_created_entry_kind = entry_kind;
            self.serialize(cx);
        }

        cx.background_spawn({
            let kvp = KeyValueStore::global(cx);
            async move {
                write_global_last_created_entry_kind(kvp, entry_kind).await;
            }
        })
        .detach();
    }

    fn spawn_terminal(
        &mut self,
        terminal_id: TerminalId,
        working_directory: Option<PathBuf>,
        custom_title: Option<SharedString>,
        initial_title: Option<SharedString>,
        created_at: Option<DateTime<Utc>>,
        select: bool,
        focus: bool,
        run_init_command: bool,
        source: AgentThreadSource,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let terminal_working_directory = working_directory.clone();
        let init_command = Self::terminal_init_command(run_init_command, cx);
        let terminal_task = self.project.update(cx, |project, cx| {
            project.create_terminal_shell(working_directory, cx)
        });
        let workspace = self.workspace.clone();
        let workspace_id = self.workspace_id;
        let project = self.project.downgrade();

        cx.spawn_in(window, async move |this, cx| {
            let terminal = match terminal_task.await {
                Ok(terminal) => terminal,
                Err(error) => {
                    log::error!("failed to spawn agent panel terminal: {error:#}");
                    workspace
                        .update(cx, |workspace, cx| workspace.show_error(error, cx))
                        .log_err();
                    this.update(cx, |this, cx| {
                        if this.pending_terminal_spawn == Some(terminal_id) {
                            this.pending_terminal_spawn = None;
                            cx.notify();
                        }
                    })
                    .log_err();
                    return anyhow::Ok(());
                }
            };
            this.update_in(cx, |this, window, cx| {
                let terminal_for_init_command = terminal.clone();
                let terminal_view = cx.new(|cx| {
                    let mut view =
                        TerminalView::new(terminal, workspace, workspace_id, project, window, cx);
                    view.set_show_workspace_actions(false, cx);
                    view
                });
                this.insert_terminal(
                    terminal_id,
                    terminal_view,
                    terminal_working_directory,
                    custom_title,
                    initial_title,
                    created_at,
                    select,
                    focus,
                    source,
                    window,
                    cx,
                );
                this.deliver_terminal_init_command(
                    &terminal_for_init_command,
                    init_command,
                    window,
                    cx,
                );
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn terminal_init_command(run_init_command: bool, cx: &App) -> Option<String> {
        run_init_command
            .then(|| AgentSettings::get_global(cx).terminal_init_command.clone())
            .flatten()
            .filter(|command| !command.trim().is_empty())
    }

    /// A restored terminal thread types its init command in again, unless it
    /// starts an agent CLI: that runs outside the sandbox, and a restore is
    /// nobody's request to run it (ADR-F49).
    fn runs_init_command_on_restore(cx: &App) -> bool {
        Self::terminal_init_command(true, cx)
            .as_deref()
            .and_then(terminal_agent_started_by)
            .is_none()
    }

    /// Types `init_command` into `terminal` — after asking, every time, when
    /// it starts an agent CLI, which in this shell runs outside the sandbox
    /// with the project's hooks and MCP servers (ADR-F49).
    fn deliver_terminal_init_command(
        &self,
        terminal: &Entity<terminal::Terminal>,
        init_command: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(program) = init_command.as_deref().and_then(terminal_agent_started_by) else {
            Self::write_terminal_init_command(terminal, init_command, cx);
            return;
        };
        let on_remote_host = self.project.read(cx).is_via_remote_server();
        let confirmed = crate::sandboxed_claude_code::confirm_running_outside_sandbox(
            program,
            on_remote_host,
            None,
            window,
            cx,
        );
        let terminal = terminal.downgrade();
        cx.spawn(async move |this, cx| {
            if !confirmed.await {
                return anyhow::Ok(());
            }
            this.update(cx, |_, cx| {
                if let Some(terminal) = terminal.upgrade() {
                    Self::write_terminal_init_command(&terminal, init_command, cx);
                }
            })
        })
        .detach_and_log_err(cx);
    }

    fn write_terminal_init_command(
        terminal: &Entity<terminal::Terminal>,
        init_command: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(command) = init_command else {
            return;
        };

        if !terminal.read(cx).is_pty() {
            terminal.update(cx, |terminal, _| {
                terminal.write_init_command(Self::terminal_init_command_input(command))
            });
            return;
        }

        let startup = terminal.update(cx, |terminal, _| {
            terminal.start_init_command_startup_handshake()
        });

        let terminal = terminal.downgrade();
        cx.spawn(async move |_this, cx| {
            // Fall back to the timeout so the init command is still delivered if
            // the shell never echoes the marker.
            let timeout = cx
                .background_executor()
                .timer(TERMINAL_INIT_COMMAND_STARTUP_TIMEOUT);
            futures::select_biased! {
                _ = startup.fuse() => {}
                _ = timeout.fuse() => {}
            }

            let input = Self::terminal_init_command_input(command);
            if let Err(error) = terminal.update(cx, move |terminal, cx| {
                if !terminal.write_init_command_after_startup(input, cx) {
                    log::debug!(
                        "skipping terminal init command because the terminal is no longer eligible"
                    );
                }
            }) {
                log::debug!("skipping terminal init command because the terminal closed: {error}");
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn terminal_init_command_input(command: String) -> Vec<u8> {
        let mut input = command.into_bytes();
        // CR, not "\r\n": "\r\n" puts PowerShell into continuation
        // mode (same convention as the activation-script writes in
        // `TerminalBuilder::new`).
        input.push(b'\x0d');
        input
    }

    fn insert_terminal(
        &mut self,
        terminal_id: TerminalId,
        terminal_view: Entity<TerminalView>,
        working_directory: Option<PathBuf>,
        custom_title: Option<SharedString>,
        initial_title: Option<SharedString>,
        created_at: Option<DateTime<Utc>>,
        select: bool,
        focus: bool,
        source: AgentThreadSource,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(custom_title) = custom_title {
            terminal_view.update(cx, |terminal_view, cx| {
                terminal_view.set_custom_title(Some(custom_title.to_string()), cx);
            });
        }
        let terminal_entity = terminal_view.read(cx).terminal().clone();
        let view_subscription = cx.subscribe(
            &terminal_view,
            move |this, _terminal_view, event: &ItemEvent, cx| match event {
                ItemEvent::UpdateTab | ItemEvent::UpdateBreadcrumbs => {
                    this.refresh_terminal_metadata(terminal_id, cx);
                }
                ItemEvent::CloseItem | ItemEvent::Edit => {}
            },
        );
        // Listen on the underlying `Terminal` entity for shell-driven metadata
        // changes and bell.
        let terminal_subscription = cx.subscribe_in(
            &terminal_entity,
            window,
            move |this, _terminal, event: &TerminalEvent, window, cx| match event {
                TerminalEvent::TitleChanged
                | TerminalEvent::Wakeup
                | TerminalEvent::BreadcrumbsChanged => {
                    this.refresh_terminal_metadata(terminal_id, cx);
                    this.report_terminal_program(terminal_id, source, cx);
                }
                TerminalEvent::Bell => this.mark_terminal_notification(terminal_id, window, cx),
                TerminalEvent::CloseTerminal => {
                    this.request_close_terminal_from_terminal_event(terminal_id, cx);
                }
                TerminalEvent::BlinkChanged(_)
                | TerminalEvent::SelectionsChanged
                | TerminalEvent::NewNavigationTarget(_)
                | TerminalEvent::SessionLogChanged
                | TerminalEvent::RemoteConnectionLost
                | TerminalEvent::Open(_) => {}
            },
        );

        let last_known_terminal_title = initial_title
            .map(|title| title.to_string())
            .unwrap_or_default();
        let mut terminal = AgentTerminal {
            view: terminal_view,
            title_editor: None,
            title_editor_initial_title: None,
            title_editor_subscription: None,
            last_known_title: last_known_terminal_title.clone(),
            last_known_terminal_title,
            last_observed_program: None,
            working_directory,
            created_at: created_at.unwrap_or_else(Utc::now),
            has_notification: false,
            search_bar: None,
            notification_windows: Vec::new(),
            notification_subscriptions: Vec::new(),
            _subscriptions: vec![view_subscription, terminal_subscription],
        };
        if self.pending_terminal_spawn == Some(terminal_id) {
            self.pending_terminal_spawn = None;
        }
        terminal.refresh_metadata(cx);
        terminal.report_started_terminal_program(terminal_id, source, cx);
        self.terminals.insert(terminal_id, terminal);
        self.persist_terminal_metadata(terminal_id, cx);
        self.emit_terminal_thread_started(terminal_id, source, cx);
        if select {
            self.set_base_view(BaseView::Terminal { terminal_id }, focus, window, cx);
        }
        cx.emit(AgentPanelEvent::EntryChanged);
        cx.notify();
    }

    /// 出来合いの [`TerminalView`] をこのパネルのアクティブなペインのタブとして
    /// 置く (ADR-F53)。公式 Claude Code CLI のように「Zed が中身を作らない」
    /// 端末の置き場で、パネルの端末スレッド (`terminals` / `BaseView::Terminal`)
    /// とは別物: メタデータも永続化も持たず、タブを閉じれば終わる。
    pub fn add_terminal_tab(
        &mut self,
        terminal_view: Entity<TerminalView>,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pane = self.active_pane.clone();
        pane.update(cx, |pane, cx| {
            pane.add_item(Box::new(terminal_view), true, focus, None, window, cx);
        });
    }

    pub fn activate_terminal(
        &mut self,
        terminal_id: TerminalId,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(terminal) = self.terminals.get_mut(&terminal_id) else {
            return;
        };
        let had_notification = terminal.has_notification;
        terminal.has_notification = false;
        if had_notification {
            self.dismiss_terminal_notifications(terminal_id, cx);
        }
        self.set_base_view(BaseView::Terminal { terminal_id }, focus, window, cx);
        if had_notification {
            cx.emit(AgentPanelEvent::EntryChanged);
            cx.notify();
        }
    }

    pub fn close_terminal(
        &mut self,
        terminal_id: TerminalId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_terminal_internal(terminal_id, true, window, cx);
    }

    pub fn close_terminal_without_activating_draft(
        &mut self,
        terminal_id: TerminalId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_terminal_internal(terminal_id, false, window, cx);
    }

    fn close_terminal_internal(
        &mut self,
        terminal_id: TerminalId,
        activate_draft_after_close: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let was_active = self.active_terminal_id() == Some(terminal_id);

        if self.pending_terminal_spawn == Some(terminal_id) {
            self.pending_terminal_spawn = None;
        }
        self.dismiss_terminal_notifications(terminal_id, cx);
        if self.terminals.remove(&terminal_id).is_none() {
            return;
        }
        if let Some(store) = TerminalThreadMetadataStore::try_global(cx) {
            store.update(cx, |store, cx| {
                store.delete(terminal_id, cx);
            });
        }
        if was_active {
            self.base_view = BaseView::Uninitialized;
            self.refresh_base_view_subscriptions(window, cx);
            if activate_draft_after_close {
                self.activate_draft(false, AgentThreadSource::AgentPanel, window, cx);
            }
        }

        cx.emit(AgentPanelEvent::EntryChanged);
        cx.notify();
    }

    fn request_close_terminal_from_terminal_event(
        &mut self,
        terminal_id: TerminalId,
        cx: &mut Context<Self>,
    ) {
        if let Some(metadata) = self.terminal_metadata(terminal_id, cx) {
            cx.emit(AgentPanelEvent::TerminalCloseRequested { metadata });
        }
    }

    fn emit_terminal_thread_started(
        &self,
        terminal_id: TerminalId,
        source: AgentThreadSource,
        cx: &App,
    ) {
        telemetry::event!(
            "Agent Thread Started",
            agent = TERMINAL_AGENT_TELEMETRY_ID,
            terminal_id = terminal_id.to_key_string(),
            source = source.as_str(),
            side = crate::agent_sidebar_side(cx),
            thread_location = "current_worktree",
        );
    }

    fn refresh_terminal_metadata(&mut self, terminal_id: TerminalId, cx: &mut Context<Self>) {
        if let Some(terminal) = self.terminals.get_mut(&terminal_id)
            && terminal.refresh_metadata(cx)
        {
            self.persist_terminal_metadata(terminal_id, cx);
            cx.emit(AgentPanelEvent::EntryChanged);
            cx.notify();
        }
    }

    fn report_terminal_program(
        &mut self,
        terminal_id: TerminalId,
        source: AgentThreadSource,
        cx: &mut Context<Self>,
    ) {
        if let Some(terminal) = self.terminals.get_mut(&terminal_id) {
            terminal.report_started_terminal_program(terminal_id, source, cx);
        }
    }

    fn persist_all_terminal_metadata(&self, cx: &mut Context<Self>) {
        let terminal_ids = self.terminals.keys().copied().collect::<Vec<_>>();
        for terminal_id in terminal_ids {
            self.persist_terminal_metadata(terminal_id, cx);
        }
    }

    fn persist_terminal_metadata(&self, terminal_id: TerminalId, cx: &mut Context<Self>) {
        let Some(store) = TerminalThreadMetadataStore::try_global(cx) else {
            return;
        };
        let Some(metadata) = self.terminal_metadata(terminal_id, cx) else {
            return;
        };
        store.update(cx, |store, cx| {
            store.save(metadata, cx);
        });
    }

    fn terminal_metadata(
        &self,
        terminal_id: TerminalId,
        cx: &App,
    ) -> Option<TerminalThreadMetadata> {
        let terminal = self.terminals.get(&terminal_id)?;
        let project = self.project.read(cx);
        Some(TerminalThreadMetadata {
            terminal_id,
            title: terminal.terminal_title(cx),
            custom_title: terminal.custom_title(cx),
            created_at: terminal.created_at,
            worktree_paths: project.worktree_paths(cx),
            remote_connection: project.remote_connection_options(cx),
            working_directory: terminal.working_directory.clone(),
        })
    }

    pub fn restore_terminal(
        &mut self,
        metadata: TerminalThreadMetadata,
        focus: bool,
        source: AgentThreadSource,
        workspace: Option<&Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.has_terminal(metadata.terminal_id) {
            self.activate_terminal(metadata.terminal_id, focus, window, cx);
            return;
        }

        if !self.supports_terminal(cx) {
            return;
        }

        self.pending_terminal_spawn = Some(metadata.terminal_id);
        let working_directory = self.terminal_restore_working_directory(&metadata, workspace, cx);
        let initial_title = Self::terminal_restore_initial_title(&metadata);
        self.spawn_terminal(
            metadata.terminal_id,
            working_directory,
            metadata.custom_title.clone(),
            initial_title,
            Some(metadata.created_at),
            true,
            focus,
            Self::runs_init_command_on_restore(cx),
            source,
            window,
            cx,
        );
    }

    fn restore_terminal_for_panel_load(
        &mut self,
        metadata: TerminalThreadMetadata,
        focus: bool,
        source: AgentThreadSource,
        workspace: Option<&Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        #[cfg(test)]
        self.restore_test_terminal(metadata, focus, source, workspace, window, cx)
            .log_err();

        #[cfg(not(test))]
        self.restore_terminal(metadata, focus, source, workspace, window, cx);
    }

    fn terminal_restore_working_directory(
        &self,
        metadata: &TerminalThreadMetadata,
        workspace: Option<&Workspace>,
        cx: &App,
    ) -> Option<PathBuf> {
        if let Some(working_directory) = metadata.working_directory.clone() {
            return Some(working_directory);
        }

        if let Some(workspace) = workspace {
            return terminal_view::default_working_directory(workspace, cx);
        }

        self.default_terminal_working_directory(cx)
    }

    fn terminal_restore_initial_title(metadata: &TerminalThreadMetadata) -> Option<SharedString> {
        (!metadata.title.is_empty()).then(|| metadata.title.clone())
    }

    fn edit_terminal_title(
        &mut self,
        terminal_id: TerminalId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(terminal) = self.terminals.get_mut(&terminal_id) else {
            return;
        };

        if let Some(title_editor) = terminal.title_editor.as_ref() {
            title_editor.focus_handle(cx).focus(window, cx);
            return;
        }

        let title = terminal.editable_title(cx).to_string();
        let title_editor_initial_title = title.clone();
        let title_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text(title, window, cx);
            editor
        });
        let title_editor_subscription = cx.subscribe_in(
            &title_editor,
            window,
            move |this, title_editor, event: &editor::EditorEvent, window, cx| {
                this.handle_terminal_title_editor_event(
                    terminal_id,
                    title_editor,
                    event,
                    window,
                    cx,
                );
            },
        );
        title_editor.update(cx, |editor, cx| {
            editor.select_all(&editor::actions::SelectAll, window, cx);
            editor.focus_handle(cx).focus(window, cx);
        });
        terminal.title_editor = Some(title_editor);
        terminal.title_editor_initial_title = Some(title_editor_initial_title);
        terminal.title_editor_subscription = Some(title_editor_subscription);
        cx.notify();
    }

    fn stop_editing_terminal_title(
        &mut self,
        terminal_id: TerminalId,
        focus_terminal: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(terminal) = self.terminals.get_mut(&terminal_id) else {
            return;
        };
        let terminal_view = terminal.view.clone();
        terminal.title_editor = None;
        terminal.title_editor_initial_title = None;
        terminal.title_editor_subscription = None;
        let title_changed = terminal.refresh_title(cx);

        if focus_terminal {
            terminal_view.focus_handle(cx).focus(window, cx);
        }
        if title_changed {
            cx.emit(AgentPanelEvent::EntryChanged);
        }
        cx.notify();
    }

    fn handle_terminal_title_editor_event(
        &mut self,
        terminal_id: TerminalId,
        title_editor: &Entity<Editor>,
        event: &editor::EditorEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            editor::EditorEvent::BufferEdited => {
                if !title_editor.read(cx).is_focused(window) {
                    return;
                }
                let Some((terminal_view, initial_title, terminal_title)) =
                    self.terminals.get(&terminal_id).and_then(|terminal| {
                        terminal
                            .title_editor
                            .as_ref()
                            .is_some_and(|current_editor| current_editor == title_editor)
                            .then(|| {
                                (
                                    terminal.view.clone(),
                                    terminal.title_editor_initial_title.clone(),
                                    terminal.terminal_title(cx),
                                )
                            })
                    })
                else {
                    return;
                };
                let new_title = title_editor.read(cx).text(cx);
                if initial_title.as_deref() == Some(new_title.as_str()) {
                    return;
                }
                let label = if new_title.trim().is_empty()
                    || new_title == terminal_title_without_prefix(terminal_title.as_ref())
                {
                    None
                } else {
                    Some(new_title)
                };

                cx.defer(move |cx| {
                    terminal_view.update(cx, |terminal_view, cx| {
                        terminal_view.set_custom_title(label, cx);
                    });
                });
            }
            editor::EditorEvent::Blurred => {
                if self
                    .terminals
                    .get(&terminal_id)
                    .and_then(|terminal| terminal.title_editor.as_ref())
                    .is_some_and(|current_editor| current_editor == title_editor)
                {
                    self.stop_editing_terminal_title(terminal_id, false, window, cx);
                }
            }
            _ => {}
        }
    }

    fn mark_terminal_notification(
        &mut self,
        terminal_id: TerminalId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.active_terminal_visible(terminal_id, window, cx) {
            return;
        }
        let newly_notified = {
            let Some(terminal) = self.terminals.get_mut(&terminal_id) else {
                return;
            };
            if terminal.has_notification {
                false
            } else {
                terminal.has_notification = true;
                true
            }
        };
        if newly_notified {
            cx.emit(AgentPanelEvent::EntryChanged);
            cx.notify();
            #[cfg(feature = "audio")]
            self.play_terminal_notification_sound(
                self.terminal_status_visible(terminal_id, window, cx),
                cx,
            );
            self.show_terminal_notification(terminal_id, window, cx);
        }
    }

    fn show_terminal_notification(
        &mut self,
        terminal_id: TerminalId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(terminal) = self.terminals.get(&terminal_id) else {
            return;
        };
        if !terminal.notification_windows.is_empty() {
            return;
        }
        let title = terminal.title(cx);
        if self.terminal_status_visible(terminal_id, window, cx) {
            return;
        }
        let settings = AgentSettings::get_global(cx);
        match settings.notify_when_agent_waiting {
            NotifyWhenAgentWaiting::PrimaryScreen => {
                window.request_attention();
                if let Some(primary) = cx.primary_display() {
                    self.pop_up_terminal_notification(terminal_id, &title, primary, window, cx);
                }
            }
            NotifyWhenAgentWaiting::AllScreens => {
                window.request_attention();
                for screen in cx.displays() {
                    self.pop_up_terminal_notification(terminal_id, &title, screen, window, cx);
                }
            }
            NotifyWhenAgentWaiting::Never => {}
        }
    }

    fn pop_up_terminal_notification(
        &mut self,
        terminal_id: TerminalId,
        title: &SharedString,
        screen: Rc<dyn PlatformDisplay>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let options = AgentNotification::window_options(screen, cx);
        let project_name = self.workspace.upgrade().and_then(|workspace| {
            workspace
                .read(cx)
                .project()
                .read(cx)
                .visible_worktrees(cx)
                .next()
                .map(|worktree| worktree.read(cx).root_name_str().to_string())
        });
        let title = title.clone();
        let Ok(screen_window) = cx.open_window(options, |_window, cx| {
            cx.new(|_cx| AgentNotification::new(title, None, IconName::Terminal, project_name))
        }) else {
            return;
        };
        let Ok(pop_up) = screen_window.entity(cx) else {
            return;
        };

        let event_subscription = cx.subscribe_in(&pop_up, window, {
            move |this, _, event: &AgentNotificationEvent, window, cx| match event {
                AgentNotificationEvent::Accepted => {
                    let Some(handle) = window.window_handle().downcast::<MultiWorkspace>() else {
                        log::error!("root view should be a MultiWorkspace");
                        return;
                    };
                    cx.activate(true);

                    let workspace = this.workspace.clone();
                    cx.defer(move |cx| {
                        handle
                            .update(cx, |multi_workspace, window, cx| {
                                window.activate_window();

                                let Some(workspace) = workspace.upgrade() else {
                                    return;
                                };
                                multi_workspace.activate(workspace.clone(), None, window, cx);

                                workspace.update(cx, |workspace, cx| {
                                    workspace.reveal_panel::<AgentPanel>(window, cx);
                                    if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
                                        panel.update(cx, |panel, cx| {
                                            panel.activate_terminal(terminal_id, true, window, cx);
                                        });
                                    }
                                    workspace.focus_panel::<AgentPanel>(window, cx);
                                });
                            })
                            .log_err();
                    });

                    this.dismiss_terminal_notifications(terminal_id, cx);
                }
                AgentNotificationEvent::Dismissed => {
                    this.dismiss_terminal_notifications(terminal_id, cx);
                }
            }
        });

        let pop_up_weak = pop_up.downgrade();
        let window_activation_subscription = cx.observe_window_activation(window, {
            let pop_up_weak = pop_up_weak.clone();
            move |this, window, cx| {
                this.dismiss_terminal_pop_up_if_visible(terminal_id, &pop_up_weak, window, cx);
            }
        });

        let multi_workspace_subscription = {
            let pop_up_weak = pop_up_weak.clone();
            window.root::<MultiWorkspace>().flatten().map(|mw| {
                cx.observe_in(&mw, window, move |this, _, window, cx| {
                    this.dismiss_terminal_pop_up_if_visible(terminal_id, &pop_up_weak, window, cx);
                })
            })
        };

        let this_panel = cx.entity();
        let agent_panel_subscription = cx.subscribe_in(&this_panel, window, {
            move |this, _, event: &AgentPanelEvent, window, cx| match event {
                AgentPanelEvent::ActiveViewChanged | AgentPanelEvent::ActiveViewFocused => {
                    this.dismiss_terminal_pop_up_if_visible(terminal_id, &pop_up_weak, window, cx);
                }
                AgentPanelEvent::EntryChanged
                | AgentPanelEvent::TerminalCloseRequested { .. }
                | AgentPanelEvent::ThreadInteracted { .. } => {}
            }
        });

        let Some(terminal) = self.terminals.get_mut(&terminal_id) else {
            screen_window
                .update(cx, |_, window, _| window.remove_window())
                .ok();
            return;
        };
        terminal.notification_windows.push(screen_window);
        terminal.notification_subscriptions.push(event_subscription);
        terminal
            .notification_subscriptions
            .push(window_activation_subscription);
        terminal
            .notification_subscriptions
            .push(agent_panel_subscription);
        if let Some(subscription) = multi_workspace_subscription {
            terminal.notification_subscriptions.push(subscription);
        }
    }

    fn dismiss_terminal_notifications(&mut self, terminal_id: TerminalId, cx: &mut App) {
        let Some(terminal) = self.terminals.get_mut(&terminal_id) else {
            return;
        };
        let windows = std::mem::take(&mut terminal.notification_windows);
        terminal.notification_subscriptions.clear();
        for window in windows {
            window
                .update(cx, |_, window, _| {
                    window.remove_window();
                })
                .ok();
        }
    }

    fn dismiss_all_terminal_notifications(&mut self, cx: &mut App) {
        let terminal_ids = self.terminals.keys().copied().collect::<Vec<_>>();
        for terminal_id in terminal_ids {
            self.dismiss_terminal_notifications(terminal_id, cx);
        }
    }

    pub fn dismiss_all_notifications(&mut self, cx: &mut Context<Self>) -> bool {
        let mut dismissed = false;
        for conversation_view in self.conversation_views() {
            dismissed |= conversation_view.update(cx, |view, cx| view.dismiss_notifications(cx));
        }
        let had_terminal_notifications = self
            .terminals
            .values()
            .any(|t| !t.notification_windows.is_empty());
        if had_terminal_notifications {
            self.dismiss_all_terminal_notifications(cx);
            dismissed = true;
        }
        dismissed
    }

    fn active_terminal_visible(&self, terminal_id: TerminalId, window: &Window, cx: &App) -> bool {
        if !window.is_window_active() {
            return false;
        }
        if !self.terminal_surface_visible(terminal_id) {
            return false;
        }
        let Some(workspace) = self.workspace.upgrade() else {
            return false;
        };
        if let Some(multi_workspace) = window.root::<MultiWorkspace>().flatten() {
            let multi_workspace = multi_workspace.read(cx);
            if multi_workspace.workspace() != &workspace {
                return false;
            }
        }
        AgentPanel::is_visible(&workspace, cx)
    }

    fn terminal_surface_visible(&self, terminal_id: TerminalId) -> bool {
        self.active_terminal_id() == Some(terminal_id)
            && matches!(self.visible_surface(), VisibleSurface::Terminal(_))
    }

    fn terminal_status_visible(&self, terminal_id: TerminalId, window: &Window, cx: &App) -> bool {
        if !window.is_window_active() {
            return false;
        }

        if let Some(multi_workspace) = window.root::<MultiWorkspace>().flatten() {
            let multi_workspace = multi_workspace.read(cx);
            if multi_workspace.sidebar_open() && multi_workspace.is_threads_list_view_active(cx) {
                return true;
            }

            let Some(workspace) = self.workspace.upgrade() else {
                return false;
            };

            return multi_workspace.workspace() == &workspace
                && self.terminal_surface_visible(terminal_id)
                && AgentPanel::is_visible(&workspace, cx);
        }

        self.workspace.upgrade().is_some_and(|workspace| {
            self.terminal_surface_visible(terminal_id) && AgentPanel::is_visible(&workspace, cx)
        })
    }

    fn dismiss_terminal_pop_up_if_visible(
        &mut self,
        terminal_id: TerminalId,
        pop_up: &WeakEntity<AgentNotification>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.terminal_status_visible(terminal_id, window, cx) {
            return;
        }
        if self.active_terminal_visible(terminal_id, window, cx)
            && let Some(terminal) = self.terminals.get_mut(&terminal_id)
            && terminal.has_notification
        {
            terminal.has_notification = false;
            cx.emit(AgentPanelEvent::EntryChanged);
            cx.notify();
        }
        if let Some(pop_up) = pop_up.upgrade() {
            pop_up.update(cx, |notification, cx| {
                notification.dismiss(cx);
            });
        }
    }

    #[cfg(feature = "audio")]
    fn play_terminal_notification_sound(&self, visible: bool, cx: &mut App) {
        let settings = AgentSettings::get_global(cx);
        if settings.play_sound_when_agent_done.should_play(visible) {
            Audio::play_sound(Sound::AgentDone, cx);
        }
    }

    fn default_terminal_working_directory(&self, cx: &App) -> Option<PathBuf> {
        // Reuse the workspace-based helper so behavior matches the regular
        // terminal panel (e.g. `WorkingDirectory::FirstProjectDirectory` falling
        // back to a file's parent directory when the worktree root is a file).
        self.workspace
            .upgrade()
            .and_then(|workspace| terminal_view::default_working_directory(workspace.read(cx), cx))
    }

    fn has_open_project(&self, cx: &App) -> bool {
        self.project.read(cx).visible_worktrees(cx).next().is_some()
    }

    fn ensure_native_agent_connection(&self, cx: &mut Context<Self>) {
        if !self.has_open_project(cx) {
            return;
        }

        let fs = self.fs.clone();
        let thread_store = self.thread_store.clone();
        self.connection_store.update(cx, |store, cx| {
            store.request_connection(
                Agent::NativeAgent,
                Agent::NativeAgent.server(fs, thread_store),
                cx,
            );
        });
    }

    pub fn activate_draft(
        &mut self,
        focus: bool,
        source: AgentThreadSource,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.has_open_project(cx) {
            return;
        }

        let draft = self.ensure_draft(source, window, cx);
        if let BaseView::AgentThread { conversation_view } = &self.base_view {
            if conversation_view.entity_id() == draft.entity_id() {
                if focus {
                    self.activation_focus_handle(cx).focus(window, cx);
                }
                return;
            }
        }
        self.set_base_view(
            BaseView::AgentThread {
                conversation_view: draft,
            },
            focus,
            window,
            cx,
        );
    }

    fn ensure_draft(
        &mut self,
        source: AgentThreadSource,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<ConversationView> {
        let desired_agent = self.selected_agent(cx);
        if let Some(draft) = &self.draft_thread {
            let draft_entity = draft.entity_id();
            let agent_matches = *draft.read(cx).agent_key() == desired_agent;
            let has_editor_content = draft
                .read(cx)
                .draft_message_editor(cx)
                .is_some_and(|editor| !editor.read(cx).text(cx).trim().is_empty());
            // Only retarget the empty draft when the user is actively
            // viewing it — that's the case where switching agents in the
            // toolbar should replace the draft with one bound to the
            // newly-selected agent. When the draft is parked in its slot
            // while the user is viewing a real thread, `selected_agent`
            // reflects that real thread's agent and must not be allowed
            // to silently rebuild the draft.
            let draft_is_active = matches!(
                &self.base_view,
                BaseView::AgentThread { conversation_view }
                    if conversation_view.entity_id() == draft_entity
            );

            if agent_matches || has_editor_content || !draft_is_active {
                return draft.clone();
            }

            // Clean up the old empty draft's metadata so it doesn't
            // linger as a ghost entry in the sidebar.
            let old_draft_id = draft.read(cx).thread_id;
            ThreadMetadataStore::global(cx).update(cx, |store, cx| {
                store.delete(old_draft_id, cx);
            });

            self.draft_thread = None;
            self._draft_editor_observation = None;
        }

        let thread = self.create_agent_thread_with_server(
            desired_agent,
            None,
            None,
            None,
            None,
            None,
            None,
            source,
            window,
            cx,
        );

        self.draft_thread = Some(thread.conversation_view.clone());
        self.observe_draft_editor(&thread.conversation_view, cx);
        thread.conversation_view
    }

    fn observe_draft_editor(
        &mut self,
        conversation_view: &Entity<ConversationView>,
        cx: &mut Context<Self>,
    ) {
        if let Some(acp_thread) = conversation_view.read(cx).root_thread(cx) {
            self._draft_editor_observation = Some(cx.subscribe(
                &acp_thread,
                |this, acp_thread, event: &AcpThreadEvent, cx| {
                    if !acp_thread.read(cx).is_draft_thread()
                        && this.draft_thread.as_ref().is_some_and(|draft| {
                            draft
                                .read(cx)
                                .root_thread(cx)
                                .is_some_and(|thread| thread.entity_id() == acp_thread.entity_id())
                        })
                    {
                        this.draft_thread = None;
                        this._draft_editor_observation = None;
                        this.serialize(cx);
                        return;
                    }

                    if let AcpThreadEvent::PromptUpdated = event {
                        this.serialize(cx);
                    }
                },
            ));
        } else {
            let cv = conversation_view.clone();
            self._draft_editor_observation = Some(cx.observe(&cv, |this, cv, cx| {
                if cv.read(cx).root_thread(cx).is_some() {
                    this.observe_draft_editor(&cv, cx);
                }
            }));
        }
    }

    /// Sets up an editor observation on the active view that reclaims
    /// it as ephemeral when the editor becomes empty. Only activates
    /// for non-ephemeral draft threads.
    fn observe_active_draft_for_empty_editor(
        &mut self,
        conversation_view: &Entity<ConversationView>,
        cx: &mut Context<Self>,
    ) {
        let thread_id = conversation_view.read(cx).thread_id;
        let is_ephemeral = self
            .draft_thread
            .as_ref()
            .is_some_and(|d| d.read(cx).thread_id == thread_id);
        if is_ephemeral {
            self._active_draft_reclaim_observation = None;
            return;
        }
        if !conversation_view.read(cx).is_draft(cx) {
            self._active_draft_reclaim_observation = None;
            return;
        }
        let Some(editor) = conversation_view.read(cx).draft_message_editor(cx) else {
            self._active_draft_reclaim_observation = None;
            return;
        };
        let cv = conversation_view.clone();
        self._active_draft_reclaim_observation =
            Some(cx.observe(&editor, move |this, _editor, cx| {
                let editor_has_text = cv
                    .read(cx)
                    .draft_message_editor(cx)
                    .is_some_and(|editor| !editor.read(cx).text(cx).trim().is_empty());
                if editor_has_text {
                    return;
                }
                if this.ephemeral_draft_thread_id(cx) == Some(thread_id) {
                    return;
                }
                if this.active_thread_id(cx) != Some(thread_id) {
                    return;
                }
                if this.try_make_empty_draft_ephemeral(cv.clone(), cx) {
                    this._active_draft_reclaim_observation = None;
                    cx.emit(AgentPanelEvent::EntryChanged);
                    cx.notify();
                }
            }));
    }

    fn try_make_empty_draft_ephemeral(
        &mut self,
        conversation_view: Entity<ConversationView>,
        cx: &mut Context<Self>,
    ) -> bool {
        let (thread_id, is_draft, is_empty) = {
            let conversation = conversation_view.read(cx);
            let thread_id = conversation.thread_id;
            let is_draft = conversation.is_draft(cx);
            let is_empty = if let Some(message_editor) = conversation.draft_message_editor(cx) {
                message_editor.read(cx).text(cx).trim().is_empty()
            } else {
                !self.draft_has_content(&conversation_view, cx)
            };

            (thread_id, is_draft, is_empty)
        };

        if !is_draft || !is_empty {
            return false;
        }

        self.retained_threads.remove(&thread_id);
        self.set_ephemeral_draft(conversation_view, cx);
        true
    }

    /// Moves a conversation view into the ephemeral `draft_thread` slot,
    /// cleaning up any previous ephemeral draft and deleting the thread's
    /// metadata so it no longer appears in the sidebar.
    fn set_ephemeral_draft(
        &mut self,
        conversation_view: Entity<ConversationView>,
        cx: &mut Context<Self>,
    ) {
        if let Some(old_draft) = self.draft_thread.take() {
            let old_id = old_draft.read(cx).thread_id;
            let new_id = conversation_view.read(cx).thread_id;
            if old_id != new_id {
                ThreadMetadataStore::global(cx).update(cx, |store, cx| {
                    store.delete(old_id, cx);
                });
            }
            self._draft_editor_observation = None;
        }
        self.draft_thread = Some(conversation_view.clone());
        self.observe_draft_editor(&conversation_view, cx);
        self.serialize(cx);
    }

    /// Creates a new retained thread and inserts it into the sidebar without
    /// switching the active view to it. Used by the `create_thread` agent tool,
    /// which passes an initial prompt, and optionally an agent and model
    /// override.
    pub fn create_thread_with_options(
        &mut self,
        options: CreateThreadOptions,
        source: AgentThreadSource,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> ThreadId {
        let (agent, override_used) = if self.project.read(cx).is_via_collab() {
            (Agent::NativeAgent, false)
        } else if let Some(override_agent) = options.agent {
            (override_agent, true)
        } else {
            (self.selected_agent.clone(), false)
        };
        // If the caller explicitly overrode the agent (e.g., the `create_thread`
        // tool wants to spawn a sibling thread using a specific agent), we
        // shouldn't let that change the panel's selected_agent or the
        // last-used-agent preference. Snapshot and restore both.
        let saved_selected_agent = override_used.then(|| self.selected_agent.clone());
        let thread = self.create_agent_thread_with_server(
            agent,
            None,
            None,
            options.work_dirs,
            options.title.clone(),
            options.initial_content,
            options.model,
            source,
            window,
            cx,
        );
        if let Some(original) = saved_selected_agent {
            self.set_selected_agent_and_persist(original, cx);
        }
        let thread_id = thread.conversation_view.read(cx).thread_id;
        self.retained_threads
            .insert(thread_id, thread.conversation_view);
        thread_id
    }

    pub fn activate_retained_thread(
        &mut self,
        id: ThreadId,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let conversation_view = if let Some(view) = self.retained_threads.remove(&id) {
            self.try_make_empty_draft_ephemeral(view.clone(), cx);
            view
        } else if let Some(view) = self.tabbed_threads.get(&id).cloned() {
            // すでにパネルのタブに載っている: そのタブを前に出すだけ。
            view
        } else if let Some(draft) = &self.draft_thread {
            if draft.read(cx).thread_id == id {
                draft.clone()
            } else {
                return;
            }
        } else {
            return;
        };
        self.set_base_view(
            BaseView::AgentThread { conversation_view },
            focus,
            window,
            cx,
        );
    }

    pub fn active_thread_id(&self, cx: &App) -> Option<ThreadId> {
        match &self.base_view {
            BaseView::AgentThread { conversation_view } => {
                Some(conversation_view.read(cx).thread_id)
            }
            _ => None,
        }
    }

    /// Drops a thread — retained or the active ephemeral draft — from
    /// the panel and deletes its metadata row. Used by the sidebar when
    /// the user dismisses a parked draft.
    pub fn remove_thread(&mut self, id: ThreadId, window: &mut Window, cx: &mut Context<Self>) {
        self.remove_thread_internal(id, true, window, cx);
    }

    pub fn remove_thread_without_activating_draft(
        &mut self,
        id: ThreadId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.remove_thread_internal(id, false, window, cx);
    }

    fn remove_thread_internal(
        &mut self,
        id: ThreadId,
        activate_draft_after_remove: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.retained_threads.remove(&id);
        // 消したスレッドのタブがパネルに残っていると、無いスレッドを描き続ける。
        self.take_thread_from_tab(id, window, cx);
        ThreadMetadataStore::global(cx).update(cx, |store, cx| {
            store.delete(id, cx);
        });

        if self
            .draft_thread
            .as_ref()
            .is_some_and(|d| d.read(cx).thread_id == id)
        {
            self.draft_thread = None;
            self._draft_editor_observation = None;
        }

        if self.active_thread_id(cx) == Some(id) {
            if activate_draft_after_remove {
                self.activate_draft(false, AgentThreadSource::AgentPanel, window, cx);
            } else {
                self.base_view = BaseView::Uninitialized;
                self.refresh_base_view_subscriptions(window, cx);
            }
            self.serialize(cx);
            cx.emit(AgentPanelEvent::ActiveViewChanged);
            cx.notify();
        }
    }

    pub fn ephemeral_draft_thread_id(&self, cx: &App) -> Option<ThreadId> {
        let draft = self.draft_thread.as_ref()?;
        let draft = draft.read(cx);
        draft.is_draft(cx).then_some(draft.thread_id)
    }

    pub fn active_terminal_id(&self) -> Option<TerminalId> {
        match &self.base_view {
            BaseView::Terminal { terminal_id } => Some(*terminal_id),
            _ => None,
        }
    }

    pub fn has_terminal(&self, terminal_id: TerminalId) -> bool {
        self.terminals.contains_key(&terminal_id)
    }

    pub fn terminals(&self, cx: &App) -> Vec<AgentPanelTerminalInfo> {
        self.terminals
            .iter()
            .map(|(id, terminal)| AgentPanelTerminalInfo {
                id: *id,
                title: terminal.title(cx),
                created_at: terminal.created_at,
                has_notification: terminal.has_notification,
                custom_title: terminal.custom_title(cx),
                working_directory: terminal.working_directory.clone(),
            })
            .collect()
    }

    pub fn editor_text(&self, id: ThreadId, cx: &App) -> Option<String> {
        self.editor_text_if_in_memory(id, cx).flatten()
    }

    pub fn editor_text_if_in_memory(&self, id: ThreadId, cx: &App) -> Option<Option<String>> {
        let cv = self
            .retained_threads
            .get(&id)
            .or_else(|| self.tabbed_threads.get(&id))
            .or_else(|| {
                self.draft_thread
                    .as_ref()
                    .filter(|draft| draft.read(cx).thread_id == id)
            })
            .or_else(|| match &self.base_view {
                BaseView::AgentThread { conversation_view }
                    if conversation_view.read(cx).thread_id == id =>
                {
                    Some(conversation_view)
                }
                _ => None,
            })?;
        let text = cv.read(cx).draft_message_editor(cx)?.read(cx).text(cx);
        if text.trim().is_empty() {
            Some(None)
        } else {
            Some(Some(text))
        }
    }

    pub fn draft_prompt_blocks_if_in_memory(
        &self,
        id: ThreadId,
        cx: &App,
    ) -> Option<Vec<acp::ContentBlock>> {
        let cv = self
            .retained_threads
            .get(&id)
            .or_else(|| self.tabbed_threads.get(&id))
            .or_else(|| {
                self.draft_thread
                    .as_ref()
                    .filter(|draft| draft.read(cx).thread_id == id)
            })
            .or_else(|| match &self.base_view {
                BaseView::AgentThread { conversation_view }
                    if conversation_view.read(cx).thread_id == id =>
                {
                    Some(conversation_view)
                }
                _ => None,
            })?;
        let message_editor = cv.read(cx).draft_message_editor(cx)?;
        Some(message_editor.read(cx).draft_content_blocks_snapshot(cx))
    }

    fn new_native_agent_thread_from_summary(
        &mut self,
        action: &NewNativeAgentThreadFromSummary,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let session_id = action.from_session_id.clone();

        let Some(content) = Self::initial_content_for_thread_summary(session_id.clone(), cx) else {
            log::error!("No session found for summarization with id {}", session_id);
            return;
        };

        cx.spawn_in(window, async move |this, cx| {
            this.update_in(cx, |this, window, cx| {
                this.external_thread(
                    Some(Agent::NativeAgent),
                    None,
                    None,
                    None,
                    Some(content),
                    true,
                    AgentThreadSource::AgentPanel,
                    window,
                    cx,
                );
                anyhow::Ok(())
            })
        })
        .detach_and_log_err(cx);
    }

    fn initial_content_for_thread_summary(
        session_id: acp::SessionId,
        cx: &App,
    ) -> Option<AgentInitialContent> {
        let thread = ThreadStore::global(cx)
            .read(cx)
            .entries()
            .find(|t| t.id == session_id)?;

        Some(AgentInitialContent::ThreadSummary {
            session_id: thread.id,
            title: Some(thread.title),
        })
    }

    fn external_thread(
        &mut self,
        agent_choice: Option<crate::Agent>,
        resume_thread_id: Option<ThreadId>,
        work_dirs: Option<PathList>,
        title: Option<SharedString>,
        initial_content: Option<AgentInitialContent>,
        focus: bool,
        source: AgentThreadSource,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if resume_thread_id.is_none() && !self.has_open_project(cx) {
            return;
        }

        let agent = agent_choice.unwrap_or_else(|| self.selected_agent(cx));
        let thread = self.create_agent_thread_with_server(
            agent,
            None,
            resume_thread_id,
            work_dirs,
            title,
            initial_content,
            None,
            source,
            window,
            cx,
        );
        self.set_base_view(thread.into(), focus, window, cx);
    }

    fn manage_skills(
        &mut self,
        _action: &ManageSkills,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.dispatch_action(
            Box::new(zed_actions::OpenSettingsAt {
                path: zed_actions::AGENT_SKILLS_SETTINGS_PATH.to_string(),
                target: None,
            }),
            cx,
        );
    }

    /// Refresh the native agent's view of available skills
    pub fn refresh_skills(&mut self, cx: &mut Context<Self>) {
        if !self.has_open_project(cx) {
            return;
        }

        self.ensure_native_agent_connection(cx);
        let Some(connect_task) = self.connection_store.update(cx, |store, cx| {
            store
                .entry(&Agent::NativeAgent)
                .map(|entry| entry.read(cx).wait_for_connection())
        }) else {
            return;
        };
        let project = self.project.clone();
        cx.spawn(async move |_this, cx| -> Result<()> {
            let connected = connect_task.await?;
            if let Some(native_connection) = connected
                .connection
                .downcast::<agent::NativeAgentConnection>()
            {
                cx.update(|cx| native_connection.refresh_skills_for_project(project, cx));
            }
            Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn expand_message_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(conversation_view) = self.active_conversation_view() else {
            return;
        };

        let Some(active_thread) = conversation_view.read(cx).root_thread_view() else {
            return;
        };

        active_thread.update(cx, |active_thread, cx| {
            active_thread.expand_message_editor(&ExpandMessageEditor, window, cx);
            active_thread.activation_focus_handle(cx).focus(window, cx);
        })
    }

    pub fn toggle_options_menu(
        &mut self,
        _: &ToggleOptionsMenu,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle, cx);
        self.agent_panel_menu_handle.toggle(window, cx);
    }

    pub fn toggle_new_thread_menu(
        &mut self,
        _: &ToggleNewThreadMenu,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.has_open_project(cx) {
            return;
        }

        self.new_thread_menu_handle.toggle(window, cx);
    }

    /// Whether the in-panel thread history is enabled by settings. It can
    /// still be suppressed for the current frame — see
    /// [`Self::thread_history_visible`].
    fn thread_history_enabled(&self, cx: &App) -> bool {
        AgentSettings::get_global(cx).panel_thread_history && self.has_open_project(cx)
    }

    /// Which of the history list's two widths the panel can afford beside the
    /// surface, if either. The width is measured, so an unmeasured panel (the
    /// first frame) is assumed to be wide.
    fn thread_history_fit(&self) -> ThreadHistoryFit {
        let Some(width) = self.measured_panel_width.get() else {
            return ThreadHistoryFit::Full;
        };
        if width >= PANEL_THREAD_HISTORY_WIDTH + MIN_THREAD_WIDTH_BESIDE_HISTORY {
            ThreadHistoryFit::Full
        } else if width >= SLIM_PANEL_THREAD_HISTORY_WIDTH + MIN_THREAD_WIDTH_BESIDE_HISTORY {
            ThreadHistoryFit::Slim
        } else {
            ThreadHistoryFit::None
        }
    }

    /// Whether the panel is wide enough to give the history list a column —
    /// either width — without squeezing the surface beside it.
    fn fits_thread_history(&self) -> bool {
        self.thread_history_fit() != ThreadHistoryFit::None
    }

    fn thread_history_visible(&self, cx: &App) -> bool {
        self.thread_history_enabled(cx) && self.fits_thread_history()
    }

    /// Whether the toolbar offers the button that reopens the closed list. A
    /// panel too narrow for the list does not offer it: there the button would
    /// promise an expansion that would immediately hide itself.
    fn thread_history_open_button_visible(&self, cx: &App) -> bool {
        !self.thread_history_enabled(cx) && self.has_open_project(cx) && self.fits_thread_history()
    }

    pub fn toggle_panel_thread_history(
        &mut self,
        _: &TogglePanelThreadHistory,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let visible = !AgentSettings::get_global(cx).panel_thread_history;
        update_settings_file(self.fs.clone(), cx, move |settings, _| {
            settings
                .agent
                .get_or_insert_default()
                .set_panel_thread_history(visible);
        });
    }

    /// 一覧そのものか、それを呼び戻すツールバーのボタンのどちらかが出ている
    /// = 開いているタブへはそこから届く。
    fn thread_history_reachable(&self, cx: &App) -> bool {
        self.thread_history_visible(cx) || self.thread_history_open_button_visible(cx)
    }

    /// Whether the panel's panes draw their tab bar. `auto` drops it while the
    /// history list is showing beside a single pane: that list carries the open
    /// tabs, so the horizontal row would repeat it (ADR-F53).
    fn tab_bar_visible(&self, cx: &App) -> bool {
        match AgentSettings::get_global(cx).panel_tab_bar {
            PanelTabBar::Always => true,
            // 「表示しない」でも、一覧もそれを開くボタンも出せない幅では
            // 開いているタブへ行く手段が一つも残らない。行き止まりを作らない
            // ぶんだけ戻す。
            PanelTabBar::Never => !self.thread_history_reachable(cx),
            PanelTabBar::Auto => {
                !(self.thread_history_visible(cx) && self.center.panes().len() == 1)
            }
        }
    }

    /// 縦の一覧の「開いているタブ」の節に出す行 (ADR-F53)。ペインのタブを
    /// 並び順どおりに (分割しているときは全ペインぶん) 並べ、そのあとにタブを
    /// 持たないパネルの端末スレッドを古い順で続ける。
    fn open_tab_rows(&self, window: &Window, cx: &App) -> Vec<OpenTabRow> {
        let active_pane_item_id = matches!(
            self.base_view,
            BaseView::AgentThread { .. } | BaseView::PaneItem { .. }
        )
        .then(|| {
            self.active_pane
                .read(cx)
                .active_item()
                .map(|item| item.item_id())
        })
        .flatten();

        let mut rows = Vec::new();
        for pane in self.center.panes() {
            for item in pane.read(cx).items() {
                let item_id = item.item_id();
                rows.push(OpenTabRow {
                    id: OpenTabId::PaneItem(item_id),
                    title: item.tab_content_text(0, cx),
                    icon: Self::open_tab_icon(item.as_ref(), window, cx),
                    is_active: Some(item_id) == active_pane_item_id,
                });
            }
        }

        let active_terminal_id = self.active_terminal_id();
        // `terminals` は HashMap なので、同じ時刻に生まれた 2 つが毎フレーム
        // 入れ替わらないよう id まで見て並べる。
        for (terminal_id, terminal) in self
            .terminals
            .iter()
            .sorted_by_key(|(terminal_id, terminal)| (terminal.created_at, **terminal_id))
        {
            rows.push(OpenTabRow {
                id: OpenTabId::PanelTerminal(*terminal_id),
                title: terminal.title(cx),
                icon: Icon::new(IconName::Terminal),
                is_active: Some(*terminal_id) == active_terminal_id,
            });
        }

        rows
    }

    /// 行のアイコンは、そのタブがタブバーで出しているもの
    /// ([`workspace::Item::tab_icon`]) をそのまま使う。持たないタブ
    /// ([`TerminalView`] など) だけ端末のアイコンに落とす。
    fn open_tab_icon(item: &dyn ItemHandle, window: &Window, cx: &App) -> Icon {
        item.tab_icon(window, cx)
            .unwrap_or_else(|| Icon::new(IconName::Terminal))
    }

    /// 「開いているタブ」の行から前面化する。ペインのタブはそのペインごと
    /// アクティブにし、パネルの端末スレッドは全面サーフェスに戻す。
    fn activate_open_tab(&mut self, id: OpenTabId, window: &mut Window, cx: &mut Context<Self>) {
        match id {
            OpenTabId::PaneItem(item_id) => {
                let Some((pane, index)) = self.pane_item_location(item_id, cx) else {
                    return;
                };
                pane.update(cx, |pane, cx| {
                    pane.activate_item(index, true, true, window, cx);
                });
                self.activate_pane(pane, cx);
                self.sync_base_view_from_pane(window, cx);
            }
            OpenTabId::PanelTerminal(terminal_id) => {
                self.activate_terminal(terminal_id, true, window, cx);
            }
        }
    }

    /// 「開いているタブ」の行の ×。未保存の確認などはペインの通常の経路に委ねる。
    fn close_open_tab(&mut self, id: OpenTabId, window: &mut Window, cx: &mut Context<Self>) {
        match id {
            OpenTabId::PaneItem(item_id) => {
                let Some((pane, _index)) = self.pane_item_location(item_id, cx) else {
                    return;
                };
                pane.update(cx, |pane, cx| {
                    pane.close_item_by_id(item_id, pane::SaveIntent::Close, window, cx)
                })
                .detach_and_log_err(cx);
            }
            OpenTabId::PanelTerminal(terminal_id) => {
                self.close_terminal(terminal_id, window, cx);
            }
        }
    }

    fn pane_item_location(&self, item_id: EntityId, cx: &App) -> Option<(Entity<Pane>, usize)> {
        self.center.panes().into_iter().find_map(|pane| {
            let index = pane
                .read(cx)
                .items()
                .position(|item| item.item_id() == item_id)?;
            Some((pane.clone(), index))
        })
    }

    pub fn toggle_panel_tab_bar(
        &mut self,
        _: &TogglePanelTabBar,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // `auto` is a rule, not a state, so the toggle writes the explicit
        // value that flips what the user is looking at right now.
        let tab_bar = if self.tab_bar_visible(cx) {
            PanelTabBar::Never
        } else {
            PanelTabBar::Always
        };
        update_settings_file(self.fs.clone(), cx, move |settings, _| {
            settings
                .agent
                .get_or_insert_default()
                .set_panel_tab_bar(tab_bar);
        });
    }

    pub fn increase_font_size(
        &mut self,
        action: &IncreaseBufferFontSize,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.handle_font_size_action(action.persist, px(1.0), cx);
    }

    pub fn decrease_font_size(
        &mut self,
        action: &DecreaseBufferFontSize,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.handle_font_size_action(action.persist, px(-1.0), cx);
    }

    fn handle_font_size_action(&mut self, persist: bool, delta: Pixels, cx: &mut Context<Self>) {
        match self.visible_font_size() {
            WhichFontSize::AgentFont => {
                if persist {
                    update_settings_file(self.fs.clone(), cx, move |settings, cx| {
                        let agent_ui_font_size =
                            ThemeSettings::get_global(cx).agent_ui_font_size(cx) + delta;
                        let agent_buffer_font_size =
                            ThemeSettings::get_global(cx).agent_buffer_font_size(cx) + delta;

                        let _ = settings.theme.agent_ui_font_size.insert(
                            f32::from(theme_settings::clamp_font_size(agent_ui_font_size)).into(),
                        );
                        let _ = settings.theme.agent_buffer_font_size.insert(
                            f32::from(theme_settings::clamp_font_size(agent_buffer_font_size))
                                .into(),
                        );
                    });
                } else {
                    theme_settings::adjust_agent_ui_font_size(cx, |size| size + delta);
                    theme_settings::adjust_agent_buffer_font_size(cx, |size| size + delta);
                }
            }
            WhichFontSize::None => {
                // The agent panel does not own this font size (e.g. when a
                // terminal is the visible surface). Let the action bubble up
                // to the workspace handler so the global buffer font size is
                // adjusted instead.
                cx.propagate();
            }
        }
    }

    pub fn reset_font_size(
        &mut self,
        action: &ResetBufferFontSize,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.visible_font_size() {
            WhichFontSize::AgentFont => {
                if action.persist {
                    update_settings_file(self.fs.clone(), cx, move |settings, _| {
                        settings.theme.agent_ui_font_size = None;
                        settings.theme.agent_buffer_font_size = None;
                    });
                } else {
                    theme_settings::reset_agent_ui_font_size(cx);
                    theme_settings::reset_agent_buffer_font_size(cx);
                }
            }
            WhichFontSize::None => {
                // Let the workspace handler reset the global buffer font size
                // that the terminal uses.
                cx.propagate();
            }
        }
    }

    pub fn reset_agent_zoom(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        theme_settings::reset_agent_ui_font_size(cx);
        theme_settings::reset_agent_buffer_font_size(cx);
    }

    pub fn toggle_zoom(&mut self, _: &ToggleZoom, window: &mut Window, cx: &mut Context<Self>) {
        if self.zoomed {
            cx.emit(PanelEvent::ZoomOut);
        } else {
            if !self.focus_handle(cx).contains_focused(window, cx) {
                self.activation_focus_handle(cx).focus(window, cx);
            }
            cx.emit(PanelEvent::ZoomIn);
        }
    }

    pub(crate) fn open_configuration(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.dispatch_action(
            Box::new(zed_actions::OpenSettingsPage {
                page: "AI".to_string(),
                target: None,
            }),
            cx,
        );
    }

    pub(crate) fn open_active_thread_as_markdown(
        &mut self,
        _: &OpenActiveThreadAsMarkdown,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(workspace) = self.workspace.upgrade()
            && let Some(conversation_view) = self.active_conversation_view()
            && let Some(active_thread) = conversation_view.read(cx).active_thread().cloned()
        {
            active_thread.update(cx, |thread, cx| {
                thread
                    .open_thread_as_markdown(workspace, window, cx)
                    .detach_and_log_err(cx);
            });
        }
    }

    pub fn open_thread_as_markdown(
        &mut self,
        thread_id: ThreadId,
        workspace: Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(conversation_view) = self.conversation_view_for_id(&thread_id, cx).cloned() else {
            return false;
        };
        let Some(thread_view) = conversation_view.read(cx).root_thread_view() else {
            return false;
        };
        thread_view.update(cx, |thread, cx| {
            thread
                .open_thread_as_markdown(workspace, window, cx)
                .detach_and_log_err(cx);
        });
        true
    }

    fn copy_thread_to_clipboard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(thread) = self.active_native_agent_thread(cx) else {
            Self::show_deferred_toast(
                &self.workspace,
                "コピーできるネイティブスレッドがありません。",
                cx,
            );
            return;
        };

        let workspace = self.workspace.clone();
        let load_task = thread.read(cx).to_db(cx);

        cx.spawn_in(window, async move |_this, cx| {
            let db_thread = load_task.await;
            let shared_thread = SharedThread::from_db_thread(&db_thread);
            let thread_data = shared_thread.to_bytes()?;
            let encoded = base64::Engine::encode(&base64::prelude::BASE64_STANDARD, &thread_data);

            cx.update(|_window, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(encoded));
                if let Some(workspace) = workspace.upgrade() {
                    workspace.update(cx, |workspace, cx| {
                        struct ThreadCopiedToast;
                        workspace.show_toast(
                            workspace::Toast::new(
                                workspace::notifications::NotificationId::unique::<ThreadCopiedToast>(),
                                "スレッドをクリップボードにコピーしました (base64 エンコード)。",
                            )
                            .autohide(),
                            cx,
                        );
                    });
                }
            })?;

            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn show_deferred_toast(
        workspace: &WeakEntity<workspace::Workspace>,
        message: &'static str,
        cx: &mut App,
    ) {
        let workspace = workspace.clone();
        cx.defer(move |cx| {
            if let Some(workspace) = workspace.upgrade() {
                workspace.update(cx, |workspace, cx| {
                    struct ClipboardToast;
                    workspace.show_toast(
                        workspace::Toast::new(
                            workspace::notifications::NotificationId::unique::<ClipboardToast>(),
                            message,
                        )
                        .autohide(),
                        cx,
                    );
                });
            }
        });
    }

    fn load_thread_from_clipboard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.has_open_project(cx) {
            Self::show_deferred_toast(
                &self.workspace,
                "スレッドを読み込むにはプロジェクトを開いてください。",
                cx,
            );
            return;
        }

        let Some(clipboard) = cx.read_from_clipboard() else {
            Self::show_deferred_toast(&self.workspace, "クリップボードに内容がありません。", cx);
            return;
        };

        let Some(encoded) = clipboard.text() else {
            Self::show_deferred_toast(
                &self.workspace,
                "クリップボードにテキストが含まれていません。",
                cx,
            );
            return;
        };

        let thread_data = match base64::Engine::decode(&base64::prelude::BASE64_STANDARD, &encoded)
        {
            Ok(data) => data,
            Err(_) => {
                Self::show_deferred_toast(
                    &self.workspace,
                    "クリップボードの内容をデコードできませんでした (base64 が必要です)。",
                    cx,
                );
                return;
            }
        };

        let shared_thread = match SharedThread::from_bytes(&thread_data) {
            Ok(thread) => thread,
            Err(_) => {
                Self::show_deferred_toast(
                    &self.workspace,
                    "クリップボードのスレッドデータを解析できませんでした。",
                    cx,
                );
                return;
            }
        };

        let db_thread = shared_thread.to_db_thread();
        let session_id = acp::SessionId::new(uuid::Uuid::new_v4().to_string());
        let thread_store = self.thread_store.clone();
        let title = db_thread.title.clone();
        let workspace = self.workspace.clone();

        cx.spawn_in(window, async move |this, cx| {
            thread_store
                .update(&mut cx.clone(), |store, cx| {
                    store.save_thread(session_id.clone(), db_thread, Default::default(), cx)
                })
                .await?;

            this.update_in(cx, |this, window, cx| {
                this.open_thread(session_id, None, Some(title), window, cx);
            })?;

            this.update_in(cx, |_, _window, cx| {
                if let Some(workspace) = workspace.upgrade() {
                    workspace.update(cx, |workspace, cx| {
                        struct ThreadLoadedToast;
                        workspace.show_toast(
                            workspace::Toast::new(
                                workspace::notifications::NotificationId::unique::<ThreadLoadedToast>(),
                                "クリップボードからスレッドを読み込みました。",
                            )
                            .autohide(),
                            cx,
                        );
                    });
                }
            })?;

            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn show_thread_metadata(
        &mut self,
        _: &ShowThreadMetadata,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(thread_id) = self.active_thread_id(cx) else {
            Self::show_deferred_toast(&self.workspace, "No active thread", cx);
            return;
        };

        let Some(store) = ThreadMetadataStore::try_global(cx) else {
            Self::show_deferred_toast(&self.workspace, "Thread metadata store not available", cx);
            return;
        };

        let Some(metadata) = store.read(cx).entry(thread_id).cloned() else {
            Self::show_deferred_toast(&self.workspace, "No metadata found for active thread", cx);
            return;
        };

        let json = thread_metadata_to_debug_json(&metadata);
        let text = serde_json::to_string_pretty(&json).unwrap_or_default();
        let title = format!("Thread Metadata: {}", metadata.display_title());

        self.open_json_buffer(title, text, window, cx);
    }

    fn show_all_sidebar_thread_metadata(
        &mut self,
        _: &ShowAllSidebarThreadMetadata,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(store) = ThreadMetadataStore::try_global(cx) else {
            Self::show_deferred_toast(&self.workspace, "Thread metadata store not available", cx);
            return;
        };

        let entries: Vec<serde_json::Value> = store
            .read(cx)
            .entries()
            .filter(|t| !t.archived)
            .map(thread_metadata_to_debug_json)
            .collect();

        let json = serde_json::Value::Array(entries);
        let text = serde_json::to_string_pretty(&json).unwrap_or_default();

        self.open_json_buffer("All Sidebar Thread Metadata".to_string(), text, window, cx);
    }

    fn open_json_buffer(
        &self,
        title: String,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let json_language = self.language_registry.language_for_name("JSON");
        let project = self.project.clone();
        let workspace = self.workspace.clone();

        window
            .spawn(cx, async move |cx| {
                let json_language = json_language.await.ok();

                let buffer = project
                    .update(cx, |project, cx| {
                        project.create_buffer(json_language, false, cx)
                    })
                    .await?;

                buffer.update(cx, |buffer, cx| {
                    buffer.set_text(text, cx);
                    buffer.set_capability(language::Capability::ReadWrite, cx);
                });

                workspace.update_in(cx, |workspace, window, cx| {
                    let buffer =
                        cx.new(|cx| MultiBuffer::singleton(buffer, cx).with_title(title.clone()));

                    workspace.add_item_to_active_pane(
                        Box::new(cx.new(|cx| {
                            let mut editor =
                                Editor::for_multibuffer(buffer, Some(project.clone()), window, cx);
                            editor.set_breadcrumb_header(title);
                            editor.disable_mouse_wheel_zoom();
                            editor
                        })),
                        None,
                        true,
                        window,
                        cx,
                    );
                })?;

                anyhow::Ok(())
            })
            .detach_and_log_err(cx);
    }

    pub fn workspace_id(&self) -> Option<WorkspaceId> {
        self.workspace_id
    }

    pub fn retained_threads(&self) -> &HashMap<ThreadId, Entity<ConversationView>> {
        &self.retained_threads
    }

    /// このパネルが抱えているが、いま表に出してはいないスレッド。ADR-F36
    /// Phase B 以降、スレッドはパネルのタブ (`tabbed_threads`) にも載るので、
    /// `retained_threads` だけを見ると裏で生きている分を数え落とす。同じ
    /// スレッドが両方に載ることはないはずだが、診断の数え上げが二重計上で
    /// 嘘をつかないよう ID で重複を落とす。
    pub fn background_threads(
        &self,
    ) -> impl Iterator<Item = (ThreadId, &Entity<ConversationView>)> + '_ {
        let visible = self
            .active_conversation_view()
            .map(|conversation_view| conversation_view.entity_id());
        let mut seen = HashSet::default();
        self.retained_threads
            .iter()
            .chain(self.tabbed_threads.iter())
            .filter(move |(_, view)| Some(view.entity_id()) != visible)
            .filter(move |(id, _)| seen.insert(**id))
            .map(|(id, view)| (*id, view))
    }

    pub fn active_conversation_view(&self) -> Option<&Entity<ConversationView>> {
        match &self.base_view {
            BaseView::AgentThread { conversation_view } => Some(conversation_view),
            _ => None,
        }
    }

    pub(crate) fn visible_conversation_view(&self) -> Option<&Entity<ConversationView>> {
        match self.visible_surface() {
            VisibleSurface::AgentThread(conversation_view) => Some(conversation_view),
            _ => None,
        }
    }

    pub fn visible_terminal_view(&self) -> Option<&Entity<TerminalView>> {
        match self.visible_surface() {
            VisibleSurface::Terminal(terminal_view) => Some(terminal_view),
            _ => None,
        }
    }

    fn toggle_terminal_thread_search(
        &mut self,
        _: &crate::ToggleSearch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // スレッド以外のタブ (公式 CLI の端末など) はペインが抱えているので、
        // 検索バーもそのペインのツールバーのもの。パネルの端末スレッドが持つ
        // `search_bar` とは別系統 (ADR-F53)。
        if matches!(self.visible_surface(), VisibleSurface::PaneItem) {
            self.deploy_active_pane_search(window, cx);
            return;
        }

        let Some(terminal) = self
            .active_terminal_id()
            .and_then(|terminal_id| self.terminals.get_mut(&terminal_id))
        else {
            cx.propagate();
            return;
        };

        let terminal_view = terminal.view.clone();
        let search_bar = terminal
            .search_bar
            .get_or_insert_with(|| cx.new(|cx| BufferSearchBar::new(None, window, cx)))
            .clone();
        let deployed = search_bar.update(cx, |search_bar, cx| {
            let terminal_item: &dyn ItemHandle = &terminal_view;
            search_bar.set_active_pane_item(Some(terminal_item), window, cx);
            search_bar.deploy(&DeployBufferSearch::find(), None, window, cx)
        });
        if deployed {
            cx.stop_propagation();
        } else {
            cx.propagate();
        }
    }

    /// アクティブなペインのツールバーの検索バーを出す。検索できないタブ
    /// (スレッドなど) では [`BufferSearchBar::deploy`] が false を返すので、
    /// 何も出さずに次のハンドラーへ回す。
    fn deploy_active_pane_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(search_bar) = self
            .active_pane
            .read(cx)
            .toolbar()
            .read(cx)
            .item_of_type::<BufferSearchBar>()
        else {
            cx.propagate();
            return;
        };
        let deployed = search_bar.update(cx, |search_bar, cx| {
            search_bar.deploy(&DeployBufferSearch::find(), None, window, cx)
        });
        if deployed {
            cx.stop_propagation();
        } else {
            cx.propagate();
        }
    }

    pub fn conversation_view_for_id(
        &self,
        thread_id: &ThreadId,
        cx: &App,
    ) -> Option<&Entity<ConversationView>> {
        self.retained_threads
            .get(thread_id)
            .or_else(|| self.tabbed_threads.get(thread_id))
            .or_else(|| {
                if let Some(view) = self.active_conversation_view()
                    && view.read(cx).thread_id == *thread_id
                {
                    Some(view)
                } else {
                    None
                }
            })
    }

    pub fn regenerate_thread_title(
        &mut self,
        thread_id: ThreadId,
        cx: &mut Context<Self>,
    ) -> ThreadTitleRegenerationResult {
        let Some(conversation_view) = self.conversation_view_for_id(&thread_id, cx).cloned() else {
            return ThreadTitleRegenerationResult::NotOpen;
        };
        Self::regenerate_conversation_thread_title(conversation_view, cx)
    }

    fn regenerate_conversation_thread_title(
        conversation_view: Entity<ConversationView>,
        cx: &mut App,
    ) -> ThreadTitleRegenerationResult {
        let Some(thread) = conversation_view.read(cx).as_native_thread(cx) else {
            return ThreadTitleRegenerationResult::NotOpen;
        };
        let thread_id = conversation_view.read(cx).parent_id();
        thread.update(cx, |thread, cx| {
            if thread.is_generating_title() {
                ThreadTitleRegenerationResult::AlreadyGenerating
            } else if thread.summarization_model().is_none() {
                ThreadTitleRegenerationResult::NoModel
            } else if thread.regenerate_title_with_callback(cx, move |title, cx| {
                ThreadMetadataStore::global(cx).update(cx, |store, cx| {
                    store.set_generated_title(thread_id, title, cx);
                });
            }) {
                ThreadTitleRegenerationResult::Started
            } else {
                ThreadTitleRegenerationResult::AlreadyGenerating
            }
        })
    }

    /// パネルが知っているスレッドの一覧: 表示中のもの、パネルのタブに載って
    /// いるもの、保持枠のもの。同じスレッドが 2 度出ないように束ねる。
    pub fn conversation_views(&self) -> Vec<Entity<ConversationView>> {
        let mut seen = HashSet::default();
        self.active_conversation_view()
            .into_iter()
            .cloned()
            .chain(self.tabbed_threads.values().cloned())
            .chain(self.retained_threads.values().cloned())
            .filter(|view| seen.insert(view.entity_id()))
            .collect()
    }

    pub fn active_thread_view(&self, cx: &App) -> Option<Entity<ThreadView>> {
        let server_view = self.active_conversation_view()?;
        server_view.read(cx).root_thread_view()
    }

    pub fn active_agent_thread(&self, cx: &App) -> Option<Entity<AcpThread>> {
        match &self.base_view {
            BaseView::AgentThread { conversation_view } => {
                conversation_view.read(cx).root_thread(cx)
            }
            _ => None,
        }
    }

    pub fn is_retained_thread(&self, id: &ThreadId) -> bool {
        self.retained_threads.contains_key(id)
    }

    pub fn cancel_thread(&self, thread_id: &ThreadId, cx: &mut Context<Self>) -> bool {
        let conversation_views = self
            .active_conversation_view()
            .into_iter()
            .chain(self.tabbed_threads.values())
            .chain(self.retained_threads.values());

        for conversation_view in conversation_views {
            if *thread_id == conversation_view.read(cx).thread_id {
                if let Some(thread_view) = conversation_view.read(cx).root_thread_view() {
                    thread_view.update(cx, |view, cx| view.cancel_generation(cx));
                    return true;
                }
            }
        }
        false
    }

    fn update_thread_work_dirs(&self, cx: &mut Context<Self>) {
        let new_work_dirs = self.project.read(cx).default_path_list(cx);
        let new_worktree_paths = self.project.read(cx).worktree_paths(cx);

        if let Some(conversation_view) = self.active_conversation_view() {
            conversation_view.update(cx, |conversation_view, cx| {
                conversation_view.set_work_dirs(new_work_dirs.clone(), cx);
            });
        }

        for conversation_view in self
            .retained_threads
            .values()
            .chain(self.tabbed_threads.values())
            .cloned()
            .collect::<Vec<_>>()
        {
            conversation_view.update(cx, |conversation_view, cx| {
                conversation_view.set_work_dirs(new_work_dirs.clone(), cx);
            });
        }

        if self.project.read(cx).is_via_collab() {
            return;
        }

        // Update metadata store so threads' path lists stay in sync with
        // the project's current worktrees. Without this, threads saved
        // before a worktree was added would have stale paths and not
        // appear under the correct sidebar group.
        let mut seen_thread_ids: HashSet<ThreadId> = self
            .retained_threads
            .keys()
            .chain(self.tabbed_threads.keys())
            .copied()
            .collect();
        if let Some(active_id) = self.active_thread_id(cx) {
            seen_thread_ids.insert(active_id);
        }
        let thread_ids: Vec<ThreadId> = seen_thread_ids.into_iter().collect();
        if !thread_ids.is_empty() {
            ThreadMetadataStore::global(cx).update(cx, |store, cx| {
                store.update_worktree_paths(&thread_ids, new_worktree_paths, cx);
            });
        }
    }

    /// 表示枠から外れたスレッドを保持枠へ預ける。
    ///
    /// 預けるのは**どこにもタブが無い**スレッドだけ (ADR-F36 Phase B)。
    /// パネル自身がタブ列になったので、タブを切り替えただけのスレッドは
    /// そのタブが持ち続ける。保持枠にも入れると同じ `ConversationView` を
    /// 2 か所が握り、`cleanup_retained_threads` の勘定も狂う。
    fn retain_running_thread(&mut self, old_view: BaseView, cx: &mut Context<Self>) {
        let BaseView::AgentThread { conversation_view } = old_view else {
            return;
        };

        if ThreadItem::is_open_in_a_tab(conversation_view.read(cx).thread_id, cx) {
            return;
        }

        if self
            .draft_thread
            .as_ref()
            .is_some_and(|d| d.entity_id() == conversation_view.entity_id())
        {
            if self.draft_has_content(&conversation_view, cx) {
                let thread_id = conversation_view.read(cx).thread_id;
                self.draft_thread = None;
                self._draft_editor_observation = None;
                self.retained_threads.insert(thread_id, conversation_view);
                self.cleanup_retained_threads(cx);
            }
            return;
        }

        let thread_id = conversation_view.read(cx).thread_id;

        if self.retained_threads.contains_key(&thread_id) {
            return;
        }

        self.retained_threads.insert(thread_id, conversation_view);
        self.cleanup_retained_threads(cx);
    }

    /// ADR-F49 (new decision): lets go of a thread the panel is only parking.
    ///
    /// [`AgentConnectionStore`] comes through here before it drops a
    /// connection nothing has used: if the view let go of here was the last
    /// hand on that connection, dropping it reaches the agent's own process
    /// and stops it. A thread that is open in a tab, or sitting in the draft
    /// slot, is not something the panel is merely parking, so it is kept and
    /// the answer is `false` — the connection then stays as it was.
    pub(crate) fn release_parked_thread(
        &mut self,
        thread_id: ThreadId,
        conversation_view: gpui::EntityId,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.is_parking_thread(thread_id, conversation_view) {
            return false;
        }
        self.retained_threads.remove(&thread_id);
        cx.notify();
        true
    }

    /// Whether this exact view is one the panel is only parking, which is the
    /// same question [`Self::release_parked_thread`] answers before letting
    /// go, asked without letting go.
    pub(crate) fn is_parking_thread(
        &self,
        thread_id: ThreadId,
        conversation_view: gpui::EntityId,
    ) -> bool {
        self.retained_threads
            .get(&thread_id)
            .is_some_and(|view| view.entity_id() == conversation_view)
    }

    /// ADR-F49 (new decision): follows the panel's own draft again after its
    /// connection was let go of.
    ///
    /// [`Self::observe_draft_editor`] watches the draft through that draft's
    /// [`AcpThread`], and parking is exactly the moment that thread goes. The
    /// watch it installs instead waits for the view to have a thread again and
    /// re-arms itself, which is what happens when the next send reconnects.
    pub(crate) fn reobserve_draft_editor(
        &mut self,
        conversation_view: &Entity<ConversationView>,
        cx: &mut Context<Self>,
    ) {
        if self
            .draft_thread
            .as_ref()
            .is_some_and(|draft| draft.entity_id() == conversation_view.entity_id())
        {
            self.observe_draft_editor(conversation_view, cx);
        }
    }

    pub(crate) fn window_handle(&self) -> gpui::AnyWindowHandle {
        self.window_handle
    }

    fn cleanup_retained_threads(&mut self, cx: &App) {
        let mut potential_removals = self
            .retained_threads
            .iter()
            .filter(|(_id, view)| {
                let Some(thread_view) = view.read(cx).root_thread_view() else {
                    return true;
                };
                let thread = thread_view.read(cx).thread.read(cx);
                thread.connection().supports_load_session() && thread.status() == ThreadStatus::Idle
            })
            .collect::<Vec<_>>();

        let max_idle = MaxIdleRetainedThreads::global(cx);

        potential_removals.sort_unstable_by_key(|(_, view)| view.read(cx).updated_at(cx));
        let n = potential_removals.len().saturating_sub(max_idle);
        let to_remove = potential_removals
            .into_iter()
            .map(|(id, _)| *id)
            .take(n)
            .collect::<Vec<_>>();
        for id in to_remove {
            self.retained_threads.remove(&id);
        }
    }

    pub(crate) fn active_native_agent_thread(&self, cx: &App) -> Option<Entity<agent::Thread>> {
        match &self.base_view {
            BaseView::AgentThread { conversation_view } => {
                conversation_view.read(cx).as_native_thread(cx)
            }
            _ => None,
        }
    }

    /// パネルの持ち物からこの `ConversationView` を外す (ADR-F36)。タブへ渡す
    /// 前に必ず通す — どこかに残ったままだと同じスレッドが 2 か所で描かれる。
    fn release_conversation_view(
        &mut self,
        conversation_view: &Entity<ConversationView>,
        cx: &mut Context<Self>,
    ) {
        if self
            .draft_thread
            .as_ref()
            .is_some_and(|draft| draft.entity_id() == conversation_view.entity_id())
        {
            self.draft_thread = None;
            self._draft_editor_observation = None;
        }
        let thread_id = conversation_view.read(cx).thread_id;
        self.retained_threads.remove(&thread_id);
    }

    /// パネルのペインからこのスレッドのタブを外し、`ConversationView` だけ
    /// 明け渡す。アイテムごと移せない経路 (別ウィンドウのタブとして開き直す
    /// 等) の受け口で、外したタブはパネルに何も残さない。
    fn take_thread_from_tab(
        &mut self,
        thread_id: ThreadId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<ConversationView>> {
        let (pane, item) = self.center.panes().into_iter().find_map(|pane| {
            let item = pane
                .read(cx)
                .items()
                .filter_map(|item| item.downcast::<ThreadItem>())
                .find(|item| item.read(cx).thread_id() == thread_id)?;
            Some((pane.clone(), item))
        })?;

        let conversation_view = item.read(cx).conversation_view().clone();
        // 外したときにパネルへ返させない (返させると同じスレッドを 2 か所が持つ)。
        item.update(cx, |item, cx| item.give_up_thread(cx));
        pane.update(cx, |pane, cx| {
            pane.remove_item(item.entity_id(), false, false, window, cx);
        });
        self.release_conversation_view(&conversation_view, cx);
        self.refresh_tabbed_threads(cx);
        Some(conversation_view)
    }

    /// 指定のスレッドをタブへ明け渡す。パネルが持っていれば取り上げ、持って
    /// いなければ読み込む。
    pub(crate) fn detach_thread_for_tab(
        &mut self,
        agent: Agent,
        thread_id: ThreadId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<ConversationView> {
        if let Some(conversation_view) = self.take_thread_from_tab(thread_id, window, cx) {
            self.sync_base_view_from_pane(window, cx);
            return conversation_view;
        }

        if let Some(draft) = self.draft_thread.clone()
            && draft.read(cx).thread_id == thread_id
        {
            self.draft_thread = None;
            self._draft_editor_observation = None;
            return draft;
        }

        if let Some(conversation_view) = self.retained_threads.remove(&thread_id) {
            return conversation_view;
        }

        let metadata = ThreadMetadataStore::try_global(cx)
            .and_then(|store| store.read(cx).entry(thread_id).cloned());
        let (work_dirs, title) = metadata
            .as_ref()
            .map(|metadata| {
                (
                    Some(metadata.folder_paths().clone()),
                    metadata.title.clone(),
                )
            })
            .unwrap_or((None, None));
        // 下書きのまま保存されていたスレッドは、打ちかけの本文も一緒に戻す
        // (`restore_new_draft` と同じ扱い)。
        let initial_content = metadata
            .as_ref()
            .filter(|metadata| metadata.is_draft())
            .and_then(|_| crate::draft_prompt_store::read(thread_id, cx))
            .map(|blocks| AgentInitialContent::ContentBlock {
                blocks,
                auto_submit: false,
            });

        // タブへ渡すスレッドはパネルの選択とは無関係なので、パネルが覚えて
        // いるエージェント (と「最後に使ったエージェント」) を書き換えない。
        // `create_agent_thread_inner` が副作用で更新するので、
        // `create_thread_with_options` と同じ流儀で控えて戻す。
        let saved_selected_agent =
            (self.selected_agent != agent).then(|| self.selected_agent.clone());
        let thread = self.create_agent_thread_with_server(
            agent,
            None,
            Some(thread_id),
            work_dirs,
            title,
            initial_content,
            None,
            AgentThreadSource::AgentPanel,
            window,
            cx,
        );
        if let Some(original) = saved_selected_agent {
            self.set_selected_agent_and_persist(original, cx);
        }
        thread.conversation_view
    }

    /// タブから戻ってきたスレッドをパネルで見せる。
    pub(crate) fn show_detached_thread(
        &mut self,
        conversation_view: Entity<ConversationView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_base_view(
            BaseView::AgentThread { conversation_view },
            true,
            window,
            cx,
        );
        self.serialize(cx);
    }

    /// タブが閉じられたスレッドを引き取る。生成中ならそのまま裏で走り続け、
    /// 待機中のものは `cleanup_retained_threads` の上限に従って落ちる。
    pub(crate) fn reclaim_detached_thread(
        &mut self,
        conversation_view: Entity<ConversationView>,
        cx: &mut Context<Self>,
    ) {
        self.retain_running_thread(BaseView::AgentThread { conversation_view }, cx);
    }

    // -- パネル内のペイン (ADR-F36 Phase B) ----------------------------------

    pub(crate) fn active_pane(&self) -> &Entity<Pane> {
        &self.active_pane
    }

    /// 分割の形とタブの並びを保存する。中身は種類ごとの `SerializableItem` が
    /// 自分の行に書くので、ここが持つのは並びだけ (ADR-F37)。
    fn serialize_layout(&mut self, cx: &mut Context<Self>) {
        if self.restoring_layout {
            return;
        }
        let Some(workspace_id) = self.workspace_id else {
            return;
        };
        let kvp = KeyValueStore::global(cx);
        self.pending_layout_serialization = cx.spawn(async move |panel, cx| {
            // まとめて動く操作 (ドロップ = 取り外し + 追加) で 2 回書かないように。
            cx.background_executor()
                .timer(Duration::from_millis(50))
                .await;
            let panel = panel.upgrade()?;
            let serialized = panel.update(cx, |panel, cx| SerializedAgentPanelLayout {
                root: serialize_pane_group(&panel.center.root, &panel.active_pane, cx),
            });
            cx.background_spawn(
                async move {
                    kvp.scoped(AGENT_PANEL_LAYOUT_KEY)
                        .write(
                            i64::from(workspace_id).to_string(),
                            serde_json::to_string(&serialized)?,
                        )
                        .await?;
                    anyhow::Ok(())
                }
                .log_err(),
            )
            .await;
            Some(())
        });
    }

    /// パネルのアクティブなペインが出しているスレッドのタブ。
    pub(crate) fn active_thread_item(&self, cx: &App) -> Option<(Entity<Pane>, EntityId)> {
        let item = self.active_pane.read(cx).active_item()?;
        item.downcast::<ThreadItem>()?;
        Some((self.active_pane.clone(), item.item_id()))
    }

    fn panel_panes(&self) -> Vec<Entity<Pane>> {
        self.center.panes().into_iter().cloned().collect()
    }

    fn thread_items(&self, cx: &App) -> Vec<Entity<ThreadItem>> {
        self.center
            .panes()
            .into_iter()
            .flat_map(|pane| pane.read(cx).items())
            .filter_map(|item| item.downcast::<ThreadItem>())
            .collect()
    }

    /// このパネルの中で、その `ConversationView` を出しているタブ。
    fn thread_item_for_view(
        &self,
        conversation_view: &Entity<ConversationView>,
        cx: &App,
    ) -> Option<(Entity<Pane>, Entity<ThreadItem>)> {
        for pane in self.center.panes() {
            for item in pane.read(cx).items() {
                let Some(item) = item.downcast::<ThreadItem>() else {
                    continue;
                };
                if item.read(cx).conversation_view().entity_id() == conversation_view.entity_id() {
                    return Some((pane.clone(), item));
                }
            }
        }
        None
    }

    /// パネルに載っているスレッドの索引を作り直す。
    fn refresh_tabbed_threads(&mut self, cx: &App) {
        self.tabbed_threads = self
            .thread_items(cx)
            .into_iter()
            .map(|item| {
                let conversation_view = item.read(cx).conversation_view().clone();
                let thread_id = conversation_view.read(cx).thread_id;
                (thread_id, conversation_view)
            })
            .collect();
    }

    /// `conversation_view` のタブをアクティブなペインに用意して前に出す。
    /// すでにどこかのペインにあれば、そのペインごとアクティブにする。
    fn show_thread_in_pane(
        &mut self,
        conversation_view: &Entity<ConversationView>,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some((pane, item)) = self.thread_item_for_view(conversation_view, cx) {
            let index = pane
                .read(cx)
                .items()
                .position(|candidate| candidate.item_id() == item.entity_id());
            if let Some(index) = index {
                pane.update(cx, |pane, cx| {
                    pane.activate_item(index, focus, focus, window, cx);
                });
            }
            self.activate_pane(pane, cx);
            return;
        }

        let item = ThreadItem::for_panel(
            conversation_view.clone(),
            self.workspace.clone(),
            window,
            cx,
        );
        let pane = self.active_pane.clone();
        pane.update(cx, |pane, cx| {
            pane.add_item(Box::new(item), focus, focus, None, window, cx);
        });
    }

    /// [`Self::active_pane`] のアクティブなアイテムに `base_view` を合わせる。
    /// タブのクリックやタブを閉じたあとの選び直しがここを通る。
    fn sync_base_view_from_pane(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = self.active_pane.read(cx).active_item() else {
            // 最後のタブが閉じられた。パネルは畳まず、いつもの下書きに戻す
            // (ADR-F36 Phase B)。
            if matches!(self.base_view, BaseView::Terminal { .. }) {
                return;
            }
            // レイアウトを組み立て直している最中は、途中の「まだ空の
            // ペイン」を見て下書きを作らない (作ると復元したタブの隣に
            // 空の下書きが混ざる)。
            if self.restoring_layout {
                return;
            }
            let old_view = std::mem::replace(&mut self.base_view, BaseView::Uninitialized);
            self.retain_running_thread(old_view, cx);
            self.refresh_base_view_subscriptions(window, cx);
            self.activate_draft(false, AgentThreadSource::AgentPanel, window, cx);
            cx.emit(AgentPanelEvent::ActiveViewChanged);
            cx.notify();
            return;
        };

        let Some(conversation_view) = item
            .downcast::<ThreadItem>()
            .map(|item| item.read(cx).conversation_view().clone())
        else {
            // スレッドではないタブ (公式 CLI の端末、他所から持ち込んだタブ) が
            // 前に出ている。下書きに戻すとそのタブが裏へ落ちてしまうので、その
            // タブを出したままにする (ADR-F53)。
            let item_id = item.item_id();
            if matches!(
                self.base_view,
                BaseView::PaneItem { item_id: current } if current == item_id
            ) {
                return;
            }
            self.set_base_view(BaseView::PaneItem { item_id }, false, window, cx);
            return;
        };

        if matches!(
            &self.base_view,
            BaseView::AgentThread { conversation_view: current }
                if current.entity_id() == conversation_view.entity_id()
        ) {
            return;
        }
        self.set_base_view(
            BaseView::AgentThread { conversation_view },
            false,
            window,
            cx,
        );
    }

    /// このペインを「次の操作の宛先」にする。フォーカスは動かさない。
    fn activate_pane(&mut self, pane: Entity<Pane>, cx: &mut Context<Self>) {
        if pane == self.active_pane {
            return;
        }
        self.active_pane = pane;
        cx.notify();
    }

    /// クリックが落ちたペインをアクティブにする ([`pane::Event::Focus`] は
    /// ペインの外からフォーカスが入ったときにしか鳴らないので取りこぼす)。
    fn activate_pane_at(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pane) = self.center.pane_at_pixel_position(position).cloned() else {
            return;
        };
        if pane == self.active_pane {
            return;
        }
        self.activate_pane(pane, cx);
        self.sync_base_view_from_pane(window, cx);
    }

    fn split_pane(
        &mut self,
        pane_to_split: &Entity<Pane>,
        direction: SplitDirection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<Pane> {
        let new_pane = new_thread_pane(self.workspace.clone(), self.project.clone(), window, cx);
        self.center.split(pane_to_split, &new_pane, direction, cx);
        cx.notify();
        new_pane
    }

    /// 新しいスレッドを**このペインに**開く。
    ///
    /// 分割した先を埋めるための入り口。`activate_new_thread` は「下書き枠」を
    /// 使い回すので、既にどこかのタブに載っている下書きを前に出すだけになり、
    /// 割ったペインが空のまま残ってしまう。ここでは必ず新しいスレッドを作る。
    fn open_new_thread_in_pane(
        &mut self,
        pane: &Entity<Pane>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.has_open_project(cx) {
            return;
        }
        let thread = self.create_agent_thread_with_server(
            self.selected_agent(cx),
            None,
            None,
            None,
            None,
            None,
            None,
            AgentThreadSource::AgentPanel,
            window,
            cx,
        );
        let conversation_view = thread.conversation_view;
        let item = ThreadItem::for_panel(
            conversation_view.clone(),
            self.workspace.clone(),
            window,
            cx,
        );
        pane.update(cx, |pane, cx| {
            pane.add_item(Box::new(item), true, true, None, window, cx);
        });
        self.set_base_view(
            BaseView::AgentThread { conversation_view },
            true,
            window,
            cx,
        );
    }

    /// 今のスレッドを右に切り出して左右に分割する。ペインに 1 枚しか無いときは
    /// 新しいスレッドを右に開く。
    pub(crate) fn split_active_thread_right(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pane = self.active_pane.clone();
        if pane.read(cx).items_len() < 2 {
            let new_pane = self.split_pane(&pane, SplitDirection::Right, window, cx);
            self.activate_pane(new_pane.clone(), cx);
            self.open_new_thread_in_pane(&new_pane, window, cx);
            window.focus(&new_pane.focus_handle(cx), cx);
            return;
        }
        let Some(item) = pane.update(cx, |pane, cx| pane.take_active_item(window, cx)) else {
            return;
        };
        let new_pane = self.split_pane(&pane, SplitDirection::Right, window, cx);
        new_pane.update(cx, |pane, cx| {
            pane.add_item(item, true, true, None, window, cx);
        });
        self.activate_pane(new_pane.clone(), cx);
        window.focus(&new_pane.focus_handle(cx), cx);
        self.sync_base_view_from_pane(window, cx);
    }

    /// パネルの外のペインへ移っていったスレッドへの参照を手放す。
    ///
    /// タブを他所へ動かすと `Pane` は `ActivateItem` → `RemovedItem` の順に
    /// 鳴らすので、最初の 1 発の時点でもう「このパネルの持ち物ではない」。
    /// ここで手放しておかないと、その直後の `activate_draft` が
    /// `draft_thread` に残った参照を拾って、他所のタブにあるスレッドを
    /// パネルにもう 1 枚描いてしまう (ADR-F36 Phase B)。
    ///
    /// **閉じられた**アイテムは手放さない (どのペインにも居ないので区別が付く)。
    /// 走っているスレッドの引き取りは従来どおり `retain_running_thread` の担当。
    fn release_views_that_left_the_panel(&mut self, cx: &mut Context<Self>) {
        let this = cx.entity_id();
        if let Some(draft) = self.draft_thread.clone()
            && self.view_moved_out_of_this_panel(&draft, this, cx)
        {
            self.draft_thread = None;
            self._draft_editor_observation = None;
        }

        let moved_out = self
            .retained_threads
            .iter()
            .filter(|(_, view)| self.view_moved_out_of_this_panel(view, this, cx))
            .map(|(thread_id, _)| *thread_id)
            .collect::<Vec<_>>();
        for thread_id in moved_out {
            self.retained_threads.remove(&thread_id);
        }
    }

    /// そのスレッドのタブが、このパネルの外のペインに載っているか。
    ///
    /// 閉じられたばかりのアイテムは索引には残るがどのペインにも居ないので
    /// `false` (= このパネルがまだ持ち主)。走っているスレッドの引き取りは
    /// 従来どおり [`Self::retain_running_thread`] の担当。
    fn view_moved_out_of_this_panel(
        &self,
        conversation_view: &Entity<ConversationView>,
        this: EntityId,
        cx: &App,
    ) -> bool {
        let thread_id = conversation_view.read(cx).thread_id;
        let Some(item_id) =
            ThreadItem::item_showing_view(thread_id, conversation_view.entity_id(), cx)
        else {
            return false;
        };
        let hosted_by =
            |pane: &Entity<Pane>| pane.read(cx).items().any(|item| item.item_id() == item_id);
        if self.center.panes().into_iter().any(hosted_by) {
            return false;
        }
        self.panes_outside_this_panel(this, cx)
            .iter()
            .any(hosted_by)
    }

    /// このウィンドウのワークスペースが持つペインのうち、このパネルのもの
    /// 以外。**自分を読まない**のが肝心で、`Workspace::all_panes` はドックの
    /// 全パネルを `read` するので、パネル自身の更新中に呼ぶと落ちる。
    fn panes_outside_this_panel(&self, this: EntityId, cx: &App) -> Vec<Entity<Pane>> {
        let Some(workspace) = self.workspace.upgrade() else {
            return Vec::new();
        };
        let workspace = workspace.read(cx);
        let mut panes = workspace.panes().to_vec();
        for dock in workspace.all_docks() {
            for panel in dock.read(cx).panels() {
                if panel.panel_id() == this {
                    continue;
                }
                panes.extend(panel.panes(cx));
            }
        }
        panes
    }

    fn handle_pane_event(
        &mut self,
        pane: &Entity<Pane>,
        event: &pane::Event,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.release_views_that_left_the_panel(cx);
        match event {
            pane::Event::ActivateItem { local, .. } => {
                if *local {
                    self.activate_pane(pane.clone(), cx);
                }
                if pane == &self.active_pane {
                    self.sync_base_view_from_pane(window, cx);
                }
                self.serialize_layout(cx);
            }
            pane::Event::AddItem { item } => {
                if let Some(workspace) = self.workspace.upgrade() {
                    workspace.update(cx, |workspace, cx| {
                        item.added_to_pane(workspace, pane.clone(), window, cx)
                    })
                }
                // `Item::added_to_workspace` が既定の「エディター領域のタブ」に
                // 戻すので、パネルに載ったことはここで上書きして教える。
                if let Some(item) = item.downcast::<ThreadItem>() {
                    item.update(cx, |item, cx| {
                        item.set_host(crate::thread_item::ThreadItemHost::Panel, cx)
                    });
                    // タブで戻ってきたスレッドは保持枠から出す (二重所有を避ける)。
                    let thread_id = item.read(cx).thread_id();
                    self.retained_threads.remove(&thread_id);
                }
                self.refresh_tabbed_threads(cx);
                if pane == &self.active_pane {
                    self.sync_base_view_from_pane(window, cx);
                }
                self.serialize_layout(cx);
            }
            pane::Event::RemovedItem { .. } => {
                self.refresh_tabbed_threads(cx);
                if pane == &self.active_pane {
                    self.sync_base_view_from_pane(window, cx);
                }
                self.serialize_layout(cx);
            }
            pane::Event::Remove { focus_on_pane } => {
                let pane_count_before_removal = self.center.panes().len();
                if pane_count_before_removal == 1 {
                    // 最後のペインは畳まない。空のままだとタブバーすら出ない
                    // ので、いつもの下書きを 1 枚出しておく (ADR-F36 Phase B)。
                    self.sync_base_view_from_pane(window, cx);
                    return;
                }
                if let Err(error) = self.center.remove(pane, cx) {
                    log::warn!("agent panel: could not remove a pane: {error:#}");
                }
                let focus_on_pane = focus_on_pane
                    .clone()
                    .or_else(|| self.center.panes().pop().cloned());
                if let Some(focus_on_pane) = focus_on_pane {
                    focus_on_pane.focus_handle(cx).focus(window, cx);
                    if self.center.panes().contains(&&focus_on_pane) {
                        self.activate_pane(focus_on_pane, cx);
                    }
                }
                if !self.center.panes().contains(&&self.active_pane) {
                    self.active_pane = self.center.first_pane();
                }
                self.refresh_tabbed_threads(cx);
                self.sync_base_view_from_pane(window, cx);
                self.serialize_layout(cx);
            }
            pane::Event::ZoomIn => {
                for pane in self.center.panes() {
                    pane.update(cx, |pane, cx| pane.set_zoomed(true, cx));
                }
                self.zoomed = true;
                cx.emit(PanelEvent::ZoomIn);
                cx.notify();
            }
            pane::Event::ZoomOut => {
                for pane in self.center.panes() {
                    pane.update(cx, |pane, cx| pane.set_zoomed(false, cx));
                }
                self.zoomed = false;
                cx.emit(PanelEvent::ZoomOut);
                cx.notify();
            }
            &pane::Event::Split { direction, mode } => {
                let pane = pane.clone();
                match mode {
                    SplitMode::ClonePane | SplitMode::EmptyPane => {
                        let new_pane = self.split_pane(&pane, direction, window, cx);
                        self.activate_pane(new_pane.clone(), cx);
                        // 新しいペインは空なので、新しいスレッドを 1 本入れる。
                        self.open_new_thread_in_pane(&new_pane, window, cx);
                        window.focus(&new_pane.focus_handle(cx), cx);
                    }
                    SplitMode::MovePane => {
                        let Some(item) =
                            pane.update(cx, |pane, cx| pane.take_active_item(window, cx))
                        else {
                            return;
                        };
                        let new_pane = self.split_pane(&pane, direction, window, cx);
                        new_pane.update(cx, |pane, cx| {
                            pane.add_item(item, true, true, None, window, cx);
                        });
                        self.activate_pane(new_pane.clone(), cx);
                        window.focus(&new_pane.focus_handle(cx), cx);
                        self.sync_base_view_from_pane(window, cx);
                    }
                }
            }
            pane::Event::Focus => {
                self.activate_pane(pane.clone(), cx);
                self.sync_base_view_from_pane(window, cx);
            }
            pane::Event::ItemPinned | pane::Event::ItemUnpinned => self.serialize_layout(cx),
            _ => {}
        }
    }

    fn set_base_view(
        &mut self,
        new_view: BaseView,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let old_view = std::mem::replace(&mut self.base_view, new_view);
        self.retain_running_thread(old_view, cx);

        if let BaseView::AgentThread { conversation_view } = &self.base_view {
            let conversation_view = conversation_view.clone();
            self.show_thread_in_pane(&conversation_view, focus, window, cx);
        }

        if let BaseView::AgentThread { conversation_view } = &self.base_view {
            let conversation_view = conversation_view.read(cx);
            let thread_agent = conversation_view.agent_key().clone();
            if self.selected_agent != thread_agent {
                self.selected_agent = thread_agent;
                self.serialize(cx);
            }
        }

        self.refresh_base_view_subscriptions(window, cx);

        if focus {
            self.activation_focus_handle(cx).focus(window, cx);
        }
        cx.emit(AgentPanelEvent::ActiveViewChanged);
    }

    fn refresh_base_view_subscriptions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self._base_view_observation = match &self.base_view {
            BaseView::AgentThread { conversation_view } => {
                self._thread_view_subscription =
                    Self::subscribe_to_active_thread_view(conversation_view, window, cx);
                let focus_handle = conversation_view.focus_handle(cx);
                self._active_thread_focus_subscription =
                    Some(cx.on_focus_in(&focus_handle, window, |_this, _window, cx| {
                        cx.emit(AgentPanelEvent::ActiveViewFocused);
                        cx.notify();
                    }));
                let cv = conversation_view.clone();
                self.observe_active_draft_for_empty_editor(&cv, cx);
                Some(cx.observe_in(&cv, window, |this, server_view, window, cx| {
                    this._thread_view_subscription =
                        Self::subscribe_to_active_thread_view(&server_view, window, cx);
                    this.observe_active_draft_for_empty_editor(&server_view, cx);
                    cx.emit(AgentPanelEvent::ActiveViewChanged);
                    this.serialize(cx);
                    cx.notify();
                }))
            }
            BaseView::Terminal { terminal_id } => {
                self._thread_view_subscription = None;
                if let Some(terminal) = self.terminals.get(terminal_id) {
                    let terminal_id = *terminal_id;
                    let focus_handle = terminal.view.focus_handle(cx);
                    self._active_thread_focus_subscription =
                        Some(
                            cx.on_focus_in(&focus_handle, window, move |this, _window, cx| {
                                if let Some(terminal) = this.terminals.get_mut(&terminal_id) {
                                    terminal.has_notification = false;
                                }
                                cx.emit(AgentPanelEvent::ActiveViewFocused);
                                cx.notify();
                            }),
                        );
                } else {
                    self._active_thread_focus_subscription = None;
                }
                None
            }
            BaseView::PaneItem { .. } | BaseView::Uninitialized => {
                self._thread_view_subscription = None;
                self._active_thread_focus_subscription = None;
                None
            }
        };
        self.serialize(cx);
    }

    fn visible_surface(&self) -> VisibleSurface<'_> {
        match &self.base_view {
            BaseView::Uninitialized => VisibleSurface::Uninitialized,
            BaseView::AgentThread { conversation_view } => {
                VisibleSurface::AgentThread(conversation_view)
            }
            BaseView::Terminal { terminal_id } => self
                .terminals
                .get(terminal_id)
                .map(|terminal| VisibleSurface::Terminal(&terminal.view))
                .unwrap_or(VisibleSurface::Uninitialized),
            BaseView::PaneItem { .. } => VisibleSurface::PaneItem,
        }
    }

    /// スレッド以外のタブを出しているときのツールバーの見出し = そのタブの名前。
    fn pane_item_toolbar_title(&self, cx: &App) -> SharedString {
        self.active_pane
            .read(cx)
            .active_item()
            .map(|item| item.tab_content_text(0, cx))
            .unwrap_or_else(|| SharedString::new_static("タブ"))
    }

    fn visible_font_size(&self) -> WhichFontSize {
        self.base_view.which_font_size_used()
    }

    fn subscribe_to_active_thread_view(
        server_view: &Entity<ConversationView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Subscription> {
        server_view.read(cx).root_thread_view().map(|tv| {
            cx.subscribe_in(
                &tv,
                window,
                |this, _view, event: &AcpThreadViewEvent, _window, cx| match event {
                    AcpThreadViewEvent::Interacted => {
                        let Some(thread_id) = this.active_thread_id(cx) else {
                            return;
                        };
                        // If the draft was the active thread, it has now been
                        // promoted to a real thread. Clear the ephemeral
                        // pointer; the ConversationView itself stays put as
                        // the active base view.
                        if this
                            .draft_thread
                            .as_ref()
                            .is_some_and(|draft| draft.read(cx).thread_id == thread_id)
                        {
                            this.draft_thread = None;
                            this._draft_editor_observation = None;
                        }
                        this.retained_threads.remove(&thread_id);
                        cx.emit(AgentPanelEvent::ThreadInteracted { thread_id });
                    }
                },
            )
        })
    }

    fn migrate_agent_server_from_extensions(&mut self, id: Arc<str>, cx: &mut Context<Self>) {
        self.project.update(cx, |project, cx| {
            project.agent_server_store().update(cx, |store, cx| {
                store.migrate_agent_server_from_extensions(id, project.fs().clone(), cx);
            });
        });
    }

    pub fn new_agent_thread_with_external_source_prompt(
        &mut self,
        external_source_prompt: Option<ExternalSourcePrompt>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.external_thread(
            None,
            None,
            None,
            None,
            external_source_prompt.map(AgentInitialContent::from),
            true,
            AgentThreadSource::AgentPanel,
            window,
            cx,
        );
    }

    pub fn load_agent_thread(
        &mut self,
        agent: Agent,
        thread_id: ThreadId,
        work_dirs: Option<PathList>,
        title: Option<SharedString>,
        focus: bool,
        source: AgentThreadSource,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(store) = ThreadMetadataStore::try_global(cx) {
            store.update(cx, |store, cx| {
                store.unarchive(thread_id, cx);
            });
        }

        // ADR-F36 Phase B: パネル自身のタブにあるなら、そのタブを前に出す。
        // (`ThreadItem::activate_existing` は `Workspace::activate_item` 経由
        // なのでドックのパネルのペインには届かない。)
        if let Some(conversation_view) = self.tabbed_threads.get(&thread_id).cloned() {
            self.set_base_view(
                BaseView::AgentThread { conversation_view },
                focus,
                window,
                cx,
            );
            return;
        }

        // ADR-F36: the user moved this thread into a tab. Activate that tab
        // instead of taking the thread back into the panel — the tab is where
        // they chose to keep it. 別のワークスペース (同じウィンドウの別
        // プロジェクト) のタブは相手のものなので触らない。
        if ThreadItem::is_open_in_workspace(thread_id, &self.workspace, cx)
            && ThreadItem::activate_existing(thread_id, cx)
        {
            if matches!(self.base_view, BaseView::Uninitialized) {
                self.activate_draft(false, source, window, cx);
            }
            return;
        }

        // Check if the active view already holds this thread.
        if let BaseView::AgentThread { conversation_view } = &self.base_view {
            if conversation_view.read(cx).thread_id == thread_id {
                cx.emit(AgentPanelEvent::ActiveViewChanged);
                return;
            }
        }

        // Check if the thread is already in memory — either as the
        // ephemeral draft pointer or in retained_threads. Either way we
        // can just reactivate without touching storage.
        if let Some(draft) = self.draft_thread.clone()
            && draft.read(cx).thread_id == thread_id
        {
            self.set_base_view(
                BaseView::AgentThread {
                    conversation_view: draft,
                },
                focus,
                window,
                cx,
            );
            return;
        }
        if let Some(conversation_view) = self.retained_threads.remove(&thread_id) {
            self.try_make_empty_draft_ephemeral(conversation_view.clone(), cx);
            self.set_base_view(
                BaseView::AgentThread { conversation_view },
                focus,
                window,
                cx,
            );
            return;
        }

        // Not in memory. Build a fresh ConversationView. For drafts we
        // also seed the message editor with any prompt text the user had
        // typed before closing the window (persisted in the scoped kvp
        // draft-prompt store).
        let is_draft = ThreadMetadataStore::try_global(cx)
            .and_then(|store| store.read(cx).entry(thread_id).map(|m| m.is_draft()))
            .unwrap_or(false);
        let initial_content = is_draft
            .then(|| crate::draft_prompt_store::read(thread_id, cx))
            .flatten()
            .map(|blocks| AgentInitialContent::ContentBlock {
                blocks,
                auto_submit: false,
            });

        self.external_thread(
            Some(agent),
            Some(thread_id),
            work_dirs,
            title,
            initial_content,
            focus,
            source,
            window,
            cx,
        );
    }

    pub(crate) fn create_agent_thread_with_server(
        &mut self,
        agent: Agent,
        server_override: Option<Rc<dyn AgentServer>>,
        resume_thread_id: Option<ThreadId>,
        work_dirs: Option<PathList>,
        title: Option<SharedString>,
        initial_content: Option<AgentInitialContent>,
        model_override: Option<String>,
        source: AgentThreadSource,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AgentThread {
        let resume_session_id = resume_thread_id.and_then(|tid| {
            ThreadMetadataStore::try_global(cx)
                .and_then(|store| store.read(cx).entry(tid).and_then(|m| m.session_id.clone()))
        });
        self.create_agent_thread_inner(
            agent,
            server_override,
            resume_thread_id,
            resume_session_id,
            work_dirs,
            title,
            initial_content,
            model_override,
            source,
            window,
            cx,
        )
    }

    /// Legacy entry that resumes a thread by raw ACP session id when no
    /// local [`ThreadMetadata`] row exists yet (share-link imports and
    /// clipboard imports).
    ///
    /// TODO(legacy-session-id): migrate remaining callers (share-link
    /// handler, clipboard import) to mint a [`ThreadId`] + seed metadata
    /// so they can route through [`create_agent_thread_with_server`] and
    /// this entry can be deleted.
    fn create_agent_thread_with_server_for_external_session(
        &mut self,
        agent: Agent,
        server_override: Option<Rc<dyn AgentServer>>,
        resume_session_id: acp::SessionId,
        work_dirs: Option<PathList>,
        title: Option<SharedString>,
        initial_content: Option<AgentInitialContent>,
        source: AgentThreadSource,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AgentThread {
        self.create_agent_thread_inner(
            agent,
            server_override,
            None,
            Some(resume_session_id),
            work_dirs,
            title,
            initial_content,
            None,
            source,
            window,
            cx,
        )
    }

    fn create_agent_thread_inner(
        &mut self,
        agent: Agent,
        server_override: Option<Rc<dyn AgentServer>>,
        resume_thread_id: Option<ThreadId>,
        resume_session_id: Option<acp::SessionId>,
        work_dirs: Option<PathList>,
        title: Option<SharedString>,
        initial_content: Option<AgentInitialContent>,
        model_override: Option<String>,
        source: AgentThreadSource,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AgentThread {
        let thread_id = resume_thread_id.unwrap_or_else(ThreadId::new);
        let workspace = self.workspace.clone();
        let project = self.project.clone();

        self.set_selected_agent_and_persist(agent.clone(), cx);

        let server = server_override
            .unwrap_or_else(|| agent.server(self.fs.clone(), self.thread_store.clone()));
        let thread_store = server
            .clone()
            .downcast::<agent::NativeAgentServer>()
            .is_some()
            .then(|| self.thread_store.clone());

        let connection_store = self.connection_store.clone();

        let conversation_view = cx.new(|cx| {
            crate::ConversationView::new(
                server,
                connection_store,
                agent,
                resume_session_id,
                Some(thread_id),
                work_dirs,
                title,
                initial_content,
                workspace.clone(),
                project,
                thread_store,
                source,
                window,
                cx,
            )
        });

        cx.observe_in(
            &conversation_view,
            window,
            |this, server_view, window, cx| {
                let is_active = this
                    .active_conversation_view()
                    .is_some_and(|active| active.entity_id() == server_view.entity_id());
                if is_active {
                    cx.emit(AgentPanelEvent::ActiveViewChanged);
                    this.serialize(cx);
                } else {
                    cx.emit(AgentPanelEvent::EntryChanged);
                }
                this.ensure_sibling_host_installed(&server_view, window, cx);
                cx.notify();
            },
        )
        .detach();

        // Try installing the host eagerly as well, in case the connection is
        // already established by the time the observe fires.
        self.ensure_sibling_host_installed(&conversation_view, window, cx);

        if let Some(model) = model_override {
            // The native thread is constructed asynchronously after the
            // connection establishes. Wait for the first `RootThreadUpdated`
            // event that yields a native thread, then apply the override once.
            let applied = Cell::new(false);
            cx.subscribe(
                &conversation_view,
                move |_this, view, _event: &RootThreadUpdated, cx| {
                    if applied.get() {
                        return;
                    }
                    let Some(native_thread) = view.read(cx).as_native_thread(cx) else {
                        return;
                    };
                    apply_native_model_override(&native_thread, &model, cx);
                    applied.set(true);
                },
            )
            .detach();
        }

        AgentThread { conversation_view }
    }

    fn ensure_sibling_host_installed(
        &self,
        conversation_view: &Entity<ConversationView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !cx.has_flag::<CreateThreadToolFeatureFlag>() {
            return;
        }
        let Some(native_connection) = conversation_view.read(cx).as_native_connection(cx) else {
            return;
        };
        let host = Rc::new(AgentPanelSiblingHost::new(
            cx.weak_entity(),
            window.window_handle(),
        )) as Rc<dyn agent::SiblingThreadHost>;
        native_connection.0.update(cx, |native_agent, _cx| {
            native_agent.set_sibling_thread_host(host);
        });
    }

    fn active_thread_has_messages(&self, cx: &App) -> bool {
        self.active_agent_thread(cx)
            .is_some_and(|thread| !thread.read(cx).entries().is_empty())
    }

    /// Whether the active view is in the **ephemeral** new-draft slot
    pub fn active_view_is_new_draft(&self, cx: &App) -> bool {
        self.draft_thread.as_ref().is_some_and(|draft| {
            draft.read(cx).is_draft(cx)
                && self
                    .active_conversation_view()
                    .is_some_and(|active| active.entity_id() == draft.entity_id())
        })
    }
    /// Whether the active thread is any kind of draft
    pub fn active_thread_is_draft(&self, cx: &App) -> bool {
        self.active_conversation_view()
            .is_some_and(|view| view.read(cx).is_draft(cx))
    }
}

/// Apply a `provider/model-id` model override to a freshly-created native thread.
/// Best-effort: logs an error and leaves the default model in place if the
/// string can't be parsed or the model isn't registered.
pub(crate) fn apply_native_model_override(
    thread: &Entity<agent::Thread>,
    model_id: &str,
    cx: &mut App,
) {
    let Some(selected) = parse_provider_slash_model(model_id) else {
        log::warn!(
            "create_thread: could not parse model override {model_id:?}; expected `provider/model-id`"
        );
        return;
    };
    let configured = LanguageModelRegistry::global(cx)
        .update(cx, |registry, cx| registry.select_model(&selected, cx));
    let Some(configured) = configured else {
        log::warn!(
            "create_thread: no model registered for {model_id:?}; using thread's default model"
        );
        return;
    };
    thread.update(cx, |thread, cx| {
        thread.set_model(configured.model, cx);
    });
}

fn parse_provider_slash_model(input: &str) -> Option<language_model::SelectedModel> {
    let (provider, model) = input.split_once('/')?;
    if provider.is_empty() || model.is_empty() {
        return None;
    }
    Some(language_model::SelectedModel {
        provider: language_model::LanguageModelProviderId::from(provider.to_string()),
        model: language_model::LanguageModelId::from(model.to_string()),
    })
}

/// Bridges agent-side `SiblingThreadHost` calls to `AgentPanel`. Constructed
/// and installed on a `NativeAgent` by the agent panel when a native-agent
/// thread is created.
pub(crate) struct AgentPanelSiblingHost {
    panel: WeakEntity<AgentPanel>,
    window: gpui::AnyWindowHandle,
}

impl AgentPanelSiblingHost {
    pub(crate) fn new(panel: WeakEntity<AgentPanel>, window: gpui::AnyWindowHandle) -> Self {
        Self { panel, window }
    }
}

impl agent::SiblingThreadHost for AgentPanelSiblingHost {
    fn create_sibling_thread(
        &self,
        request: agent::SiblingThreadRequest,
        cx: &mut gpui::AsyncApp,
    ) -> Task<Result<agent::SiblingThreadInfo>> {
        let panel = self.panel.clone();
        let window = self.window;
        cx.spawn(async move |cx| {
            let agent_choice = match request.agent_id.as_deref() {
                None => None,
                Some(id) if id == agent::ZED_AGENT_ID.as_ref() => Some(Agent::NativeAgent),
                Some(id) => {
                    // Reject unknown agent ids up front so the model gets a
                    // structured error pointing at `list_agents_and_models`,
                    // rather than a thread that silently fails to launch in
                    // the user's sidebar.
                    let known = panel
                        .read_with(cx, |panel, cx| {
                            let store = panel.project.read(cx).agent_server_store().clone();
                            store
                                .read(cx)
                                .external_agents()
                                .any(|known_id| known_id.0.as_ref() == id)
                        })
                        .unwrap_or(false);
                    if !known {
                        return Err(anyhow!(
                            "Unknown agent id {id:?}. Call `list_agents_and_models` \
                             to see the agents available for `create_thread`."
                        ));
                    }
                    Some(Agent::Custom {
                        id: project::AgentId(id.to_string().into()),
                    })
                }
            };

            let initial_content = AgentInitialContent::ContentBlock {
                blocks: vec![acp::ContentBlock::Text(acp::TextContent::new(
                    request.prompt.clone(),
                ))],
                auto_submit: true,
            };

            let title: SharedString = request.title.clone();
            let options = CreateThreadOptions {
                title: Some(title.clone()),
                initial_content: Some(initial_content),
                agent: agent_choice.clone(),
                model: request.model.clone(),
                work_dirs: None,
            };

            // If the caller asked for a fresh worktree, open a new workspace
            // backed by a linked git worktree of each git repo in the parent
            // project — the same flow the user gets when they pick "Create
            // worktree" from the worktree picker. The sibling thread is then
            // created inside the new workspace's agent panel, so it lives
            // alongside any threads the user would create there manually.
            let mut worktree_warning: Option<String> = None;
            let target_panel = if request.use_new_worktree {
                let workspace = panel.read_with(cx, |panel, _cx| panel.workspace.clone())?;
                let workspace = workspace
                    .upgrade()
                    .ok_or_else(|| anyhow!("Source workspace is no longer available"))?;
                // The branch target follows the existing UI semantics: when
                // `base_ref` is set, treat it as the ref to base off of
                // (resolved like `git switch --detach <ref>`); otherwise base
                // off the current HEAD. Either way the new worktrees are in
                // detached HEAD state — the agent can attach to a branch via
                // git afterwards.
                let branch_target = match request.base_ref.as_ref() {
                    Some(ref_name) => zed_actions::NewWorktreeBranchTarget::ExistingBranch {
                        name: ref_name.clone(),
                    },
                    None => zed_actions::NewWorktreeBranchTarget::CurrentBranch,
                };
                let action = zed_actions::CreateWorktree {
                    worktree_name: request.worktree_name.clone(),
                    branch_target,
                };
                let creation = window.update(cx, |_root, window, cx| {
                    workspace.update(cx, |workspace, cx| {
                        git_ui_core::worktree_service::create_worktree_workspace(
                            workspace, &action, window, None, cx,
                        )
                    })
                })?;
                let created = creation
                    .await
                    .context("failed to create worktree workspace")?;
                // The creation flow tells us when the project had multiple
                // worktrees of the same underlying repo, which it consolidates
                // into one new worktree — flag it so the calling agent knows
                // the result may not reflect every source worktree's state.
                if created.consolidated_worktrees {
                    worktree_warning = Some(
                        "The project contained multiple worktrees backed by the same git \
                         repository, so they were consolidated into a single new worktree. \
                         The new thread's worktree is based on one of them and may not \
                         reflect the exact state of the others."
                            .to_string(),
                    );
                }
                // Locate the agent panel on the new workspace. We rely on
                // the panel having registered by the time
                // `create_worktree_workspace` returns — `open_worktree_workspace`
                // explicitly awaits `take_panels_task` and the initial scan.
                created
                    .workspace
                    .read_with(cx, |workspace, cx| workspace.panel::<AgentPanel>(cx))
                    .ok_or_else(|| anyhow!("new workspace did not register an agent panel"))?
                    .downgrade()
            } else {
                panel.clone()
            };
            // Both the source panel and any newly-opened worktree workspace
            // live in the same OS window (the new workspace is a tab on the
            // existing MultiWorkspace), so the original window handle is
            // still the right context for the `create_thread_with_options`
            // call regardless of which panel ends up the target.
            let target_window = window;

            // We deliberately don't wait for the new thread's session to
            // become available here: there are currently no agent tools that
            // operate on sibling threads by session ID, so requiring one would
            // just introduce a race for no benefit.
            let resolved_agent_id = target_window.update(cx, |_root, window, cx| {
                target_panel.update(cx, |panel, cx| {
                    panel.create_thread_with_options(
                        options,
                        AgentThreadSource::AgentPanel,
                        window,
                        cx,
                    );
                    let resolved_agent = agent_choice
                        .clone()
                        .unwrap_or_else(|| panel.selected_agent.clone());
                    resolved_agent.id()
                })
            })??;

            Ok(agent::SiblingThreadInfo {
                title,
                agent_id: resolved_agent_id.0.to_string(),
                model: request.model,
                warning: worktree_warning,
            })
        })
    }

    fn list_available_agents(&self, cx: &mut App) -> Result<agent::AvailableAgents> {
        let panel = self
            .panel
            .upgrade()
            .ok_or_else(|| anyhow!("Agent panel is no longer available"))?;

        let mut agents = Vec::new();

        // Native Zed agent — always available, and we can enumerate models
        // directly from the language model registry.
        let native_models = {
            let registry = LanguageModelRegistry::read_global(cx);
            let default = registry.default_model();
            let mut models = Vec::new();
            for provider in registry.providers() {
                if !provider.is_authenticated(cx) {
                    continue;
                }
                let provider_id = provider.id();
                for model in provider.provided_models(cx) {
                    let id = format!("{}/{}", provider_id.0, model.id().0);
                    let is_default = default
                        .as_ref()
                        .map(|cm| cm.provider.id() == provider_id && cm.model.id() == model.id())
                        .unwrap_or(false);
                    models.push(agent::AvailableModel {
                        id,
                        name: model.name().0,
                        is_default,
                    });
                }
            }
            models
        };
        agents.push(agent::AvailableAgent {
            id: agent::ZED_AGENT_ID.to_string(),
            name: Agent::NativeAgent.label(),
            is_native: true,
            models: native_models,
        });

        let project = panel.read(cx).project.clone();
        let agent_server_store = project.read(cx).agent_server_store().clone();
        let store = agent_server_store.read(cx);
        for agent_id in store.external_agents() {
            let display = store
                .agent_display_name(agent_id)
                .unwrap_or_else(|| agent_id.0.clone());
            agents.push(agent::AvailableAgent {
                id: agent_id.0.to_string(),
                name: display,
                is_native: false,
                // External agents pick their own models dynamically; we don't
                // try to enumerate them ahead of time.
                models: Vec::new(),
            });
        }

        Ok(agent::AvailableAgents { agents })
    }
}

/// パネルの中のペインを 1 枚作る (ADR-F36 Phase B)。ブラウザーパネルの
/// `new_browser_pane` と同じ作りで、違うのは「+」が新しいスレッドになることと、
/// 分割を中央ではなくこのパネルの [`PaneGroup`] に向けること。
fn new_thread_pane(
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    window: &mut Window,
    cx: &mut Context<AgentPanel>,
) -> Entity<Pane> {
    let agent_panel = cx.entity();
    let pane = cx.new(|cx| {
        let mut pane = Pane::new(
            workspace.clone(),
            project.clone(),
            Default::default(),
            None,
            NewThread.boxed_clone(),
            false,
            window,
            cx,
        );
        pane.set_can_navigate(false, cx);
        pane.display_nav_history_buttons(None);
        // Evaluated on every frame of the pane, so `agent.panel_tab_bar` and
        // the state it depends on (the history list, how many panes the panel
        // holds) take effect as soon as they change.
        let tab_bar_panel = agent_panel.downgrade();
        pane.set_should_display_tab_bar(move |_window, cx| {
            tab_bar_panel
                .read_with(cx, |panel, cx| panel.tab_bar_visible(cx))
                .unwrap_or(true)
        });
        pane.set_zoom_out_on_close(false);

        let split_predicate_panel = agent_panel.downgrade();
        pane.set_can_split(Some(Arc::new(move |pane, dragged_item, _window, cx| {
            let Some(tab) = dragged_item.downcast_ref::<DraggedTab>() else {
                return false;
            };
            let is_current_pane = tab.pane == cx.entity();
            let Ok(can_drag_away) = split_predicate_panel.read_with(cx, |panel, _| {
                let panes = panel.center.panes();
                !panes.contains(&&tab.pane)
                    || panes.len() > 1
                    || (!is_current_pane || pane.items_len() > 1)
            }) else {
                return false;
            };
            if !can_drag_away {
                return false;
            }
            // 種類は問わない: IDE 内のどのタブでもここへ落とせる (ADR-F33)。
            if is_current_pane {
                pane.item_for_index(tab.ix).is_some()
            } else {
                tab.pane.read(cx).item_for_index(tab.ix).is_some()
            }
        })));

        // 端に落として分割するときは、ワークスペースの中央ではなくこのパネルの
        // `PaneGroup` を割る (`Workspace::split_pane` は中央しか割れない)。
        let split_handler_panel = agent_panel.downgrade();
        pane.set_tab_drop_split_handler(Some(Arc::new(
            move |pane_to_split, direction, window, cx| {
                let panel = split_handler_panel.upgrade()?;
                panel.update(cx, |panel, cx| {
                    if !panel.center.panes().contains(&pane_to_split) {
                        return None;
                    }
                    Some(panel.split_pane(pane_to_split, direction, window, cx))
                })
            },
        )));

        let buttons_panel = agent_panel.downgrade();
        pane.set_render_tab_bar_buttons(cx, move |pane, _window, cx| {
            let new_thread_panel = buttons_panel.clone();
            let split_panel = buttons_panel.clone();
            let focus_handle = pane.focus_handle(cx);
            (
                None,
                Some(
                    h_flex()
                        .gap(DynamicSpacing::Base02.rems(cx))
                        .child(
                            IconButton::new("agent-panel-new-thread", IconName::Plus)
                                .icon_size(IconSize::Small)
                                .tooltip(move |_, cx| {
                                    Tooltip::for_action_in(
                                        "新しいスレッド",
                                        &NewThread,
                                        &focus_handle,
                                        cx,
                                    )
                                })
                                .on_click(move |_, window, cx| {
                                    new_thread_panel
                                        .update(cx, |panel, cx| {
                                            // タブバーの「+」は必ずスレッド
                                            // (`NewThread` は直前に作ったのが
                                            // ターミナルならターミナルを作る)。
                                            panel.activate_new_thread(
                                                true,
                                                AgentThreadSource::AgentPanel,
                                                window,
                                                cx,
                                            )
                                        })
                                        .log_err();
                                }),
                        )
                        .child(
                            IconButton::new("agent-panel-split-right", IconName::Split)
                                .icon_size(IconSize::Small)
                                .tooltip(Tooltip::text("右に分割"))
                                .on_click(move |_, window, cx| {
                                    split_panel
                                        .update(cx, |panel, cx| {
                                            panel.split_active_thread_right(window, cx)
                                        })
                                        .log_err();
                                }),
                        )
                        .into_any_element(),
                ),
            )
        });

        // パネルのタブ (公式 CLI の端末など) でも検索できるように、ペインの
        // ツールバーに検索バーだけを載せる。パンくずを載せないのは、スレッドの
        // タブの見た目を変えないため: 検索できないアイテムでは検索バーが
        // `Hidden` を返し、ツールバーは何も描かない (ADR-F53)。
        let toolbar = pane.toolbar().clone();
        let languages = project.read(cx).languages().clone();
        let search_bar = cx.new(|cx| BufferSearchBar::new(Some(languages), window, cx));
        toolbar.update(cx, |toolbar, cx| {
            toolbar.add_item(search_bar, window, cx);
        });

        pane
    });

    cx.subscribe_in(&pane, window, AgentPanel::handle_pane_event)
        .detach();
    cx.observe(&pane, |_, _, cx| cx.notify()).detach();

    pane
}

impl Focusable for AgentPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

fn agent_panel_dock_position(cx: &App) -> DockPosition {
    AgentSettings::get_global(cx).dock.into()
}

pub enum AgentPanelEvent {
    ActiveViewChanged,
    ActiveViewFocused,
    EntryChanged,
    TerminalCloseRequested { metadata: TerminalThreadMetadata },
    ThreadInteracted { thread_id: ThreadId },
}

impl EventEmitter<PanelEvent> for AgentPanel {}
impl EventEmitter<AgentPanelEvent> for AgentPanel {}

impl Panel for AgentPanel {
    fn persistent_name() -> &'static str {
        "AgentPanel"
    }

    fn panel_key() -> &'static str {
        AGENT_PANEL_KEY
    }

    fn activation_focus_handle(&self, cx: &App) -> FocusHandle {
        match self.visible_surface() {
            VisibleSurface::Uninitialized => self.focus_handle.clone(),
            VisibleSurface::AgentThread(conversation_view) => {
                conversation_view.read(cx).activation_focus_handle(cx)
            }
            VisibleSurface::Terminal(terminal_view) => terminal_view.focus_handle(cx),
            VisibleSurface::PaneItem => self
                .active_pane
                .read(cx)
                .active_item()
                .map(|item| item.item_focus_handle(cx))
                .unwrap_or_else(|| self.focus_handle.clone()),
        }
    }

    fn position(&self, _window: &Window, cx: &App) -> DockPosition {
        agent_panel_dock_position(cx)
    }

    /// パネルの中がタブ列になったので、下ドックでも実用になる (ADR-F36 Phase B)。
    fn position_is_valid(&self, _position: DockPosition) -> bool {
        true
    }

    fn set_position(&mut self, position: DockPosition, _: &mut Window, cx: &mut Context<Self>) {
        let side = match position {
            DockPosition::Left => "left",
            DockPosition::Right => "right",
            DockPosition::Bottom => "bottom",
        };
        telemetry::event!("Agent Panel Side Changed", side = side);
        settings::update_settings_file(self.fs.clone(), cx, move |settings, _| {
            settings
                .agent
                .get_or_insert_default()
                .set_dock(position.into());
        });
    }

    fn default_size(&self, window: &Window, cx: &App) -> Pixels {
        let settings = AgentSettings::get_global(cx);
        match self.position(window, cx) {
            DockPosition::Left | DockPosition::Right => settings.default_width,
            DockPosition::Bottom => settings.default_height,
        }
    }

    fn min_size(&self, window: &Window, cx: &App) -> Option<Pixels> {
        match self.position(window, cx) {
            DockPosition::Left | DockPosition::Right => Some(MIN_PANEL_WIDTH),
            DockPosition::Bottom => None,
        }
    }

    fn supports_flexible_size(&self) -> bool {
        true
    }

    fn has_flexible_size(&self, window: &Window, cx: &App) -> bool {
        // 可変 (比率) サイズは左右のドックの話 (設定の説明もそう書いてある)。
        AgentSettings::get_global(cx).flexible && self.position(window, cx) != DockPosition::Bottom
    }

    fn set_flexible_size(&mut self, flexible: bool, _window: &mut Window, cx: &mut Context<Self>) {
        settings::update_settings_file(self.fs.clone(), cx, move |settings, _| {
            settings
                .agent
                .get_or_insert_default()
                .set_flexible_size(flexible);
        });
    }

    fn starts_open(&self, _window: &Window, cx: &App) -> bool {
        AgentSettings::get_global(cx).starts_open
    }

    fn set_active(&mut self, active: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.is_active = active;
        if active {
            self.ensure_thread_initialized(window, cx);
        }
    }

    fn remote_id() -> Option<proto::PanelId> {
        Some(proto::PanelId::AssistantPanel)
    }

    fn icon(&self, _window: &Window, cx: &App) -> Option<IconName> {
        (self.enabled(cx) && AgentSettings::get_global(cx).button).then_some(IconName::ZedAssistant)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("エージェントパネル")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        0
    }

    fn enabled(&self, cx: &App) -> bool {
        AgentSettings::get_global(cx).enabled(cx)
    }

    fn is_agent_panel(&self) -> bool {
        true
    }

    fn pane(&self) -> Option<Entity<Pane>> {
        Some(self.active_pane.clone())
    }

    fn panes(&self) -> Vec<Entity<Pane>> {
        self.panel_panes()
    }

    fn hide_button_setting(&self, _: &App) -> Option<workspace::HideStatusItem> {
        Some(workspace::HideStatusItem::new(|settings| {
            settings.agent.get_or_insert_default().button = Some(false);
        }))
    }

    fn is_zoomed(&self, _window: &Window, _cx: &App) -> bool {
        self.zoomed
    }

    fn set_zoomed(&mut self, zoomed: bool, _window: &mut Window, cx: &mut Context<Self>) {
        self.zoomed = zoomed;
        for pane in self.center.panes() {
            pane.update(cx, |pane, cx| pane.set_zoomed(zoomed, cx));
        }
        cx.notify();
    }
}

/// A configured external agent as the `+` menu and the model selector's
/// エージェント section show it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ExternalAgentMenuItem {
    pub(crate) id: AgentId,
    pub(crate) display_name: SharedString,
    pub(crate) icon_path: Option<SharedString>,
}

/// The configured external agents, by display name ignoring case. Shared by the
/// `+` menu and the model selector so that both name an agent the same way.
pub(crate) fn external_agent_menu_items(
    agent_server_store: &project::AgentServerStore,
    cx: &App,
) -> Vec<ExternalAgentMenuItem> {
    let registry_store = project::AgentRegistryStore::try_global(cx);
    let registry_store = registry_store.as_ref().map(|store| store.read(cx));
    agent_server_store
        .external_agents()
        .map(|agent_id| {
            let registry_agent = registry_store.and_then(|store| store.agent(agent_id));
            let display_name = agent_server_store
                .agent_display_name(agent_id)
                .or_else(|| registry_agent.map(|agent| agent.name().clone()))
                .unwrap_or_else(|| agent_id.0.clone());
            let icon_path = agent_server_store
                .agent_icon(agent_id)
                .or_else(|| registry_agent.and_then(|agent| agent.icon_path().cloned()));
            ExternalAgentMenuItem {
                id: agent_id.clone(),
                display_name,
                icon_path,
            }
        })
        .sorted_unstable_by_key(|item| item.display_name.to_lowercase())
        .collect()
}

impl AgentPanel {
    fn ensure_thread_initialized(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.base_view, BaseView::Uninitialized) {
            if self.pending_terminal_spawn.is_some() {
                return;
            }
            if self.should_create_terminal_for_new_entry(cx) {
                let terminal_id = TerminalId::new();
                self.pending_terminal_spawn = Some(terminal_id);
                cx.defer_in(window, move |this, window, cx| {
                    if matches!(this.base_view, BaseView::Uninitialized)
                        && this.pending_terminal_spawn == Some(terminal_id)
                        && this.should_create_terminal_for_new_entry(cx)
                    {
                        this.create_initial_terminal(
                            terminal_id,
                            AgentThreadSource::AgentPanel,
                            window,
                            cx,
                        );
                    } else if this.pending_terminal_spawn == Some(terminal_id) {
                        this.pending_terminal_spawn = None;
                    }
                });
            } else {
                self.activate_draft(false, AgentThreadSource::AgentPanel, window, cx);
            }
        }
    }

    fn create_initial_terminal(
        &mut self,
        terminal_id: TerminalId,
        source: AgentThreadSource,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.supports_terminal(cx) {
            if self.pending_terminal_spawn == Some(terminal_id) {
                self.pending_terminal_spawn = None;
            }
            return;
        }
        let working_directory = self.terminal_working_directory(None, cx);
        self.spawn_initial_terminal(terminal_id, working_directory, source, window, cx);
    }

    #[cfg(not(test))]
    fn spawn_initial_terminal(
        &mut self,
        terminal_id: TerminalId,
        working_directory: Option<PathBuf>,
        source: AgentThreadSource,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.spawn_terminal(
            terminal_id,
            working_directory,
            None,
            None,
            None,
            true,
            false,
            true,
            source,
            window,
            cx,
        );
    }

    #[cfg(test)]
    fn spawn_initial_terminal(
        &mut self,
        terminal_id: TerminalId,
        working_directory: Option<PathBuf>,
        source: AgentThreadSource,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Err(error) = self.insert_display_only_terminal(
            terminal_id,
            working_directory,
            None,
            None,
            None,
            true,
            false,
            true,
            source,
            window,
            cx,
        ) {
            log::error!("failed to spawn test agent panel terminal: {error:#}");
            if self.pending_terminal_spawn == Some(terminal_id) {
                self.pending_terminal_spawn = None;
                cx.notify();
            }
        }
    }

    fn destination_has_meaningful_state(&self, cx: &App) -> bool {
        if !self.retained_threads.is_empty() || !self.terminals.is_empty() {
            return true;
        }
        // パネルはいつも 1 枚は下書きのタブを持っているので、「タブがある」
        // ことは中身がある証拠にならない。2 枚目以降だけを数える。
        if self.tabbed_threads.len() > 1 {
            return true;
        }

        match &self.base_view {
            BaseView::Uninitialized => false,
            BaseView::Terminal { .. } | BaseView::PaneItem { .. } => true,
            BaseView::AgentThread { conversation_view } => {
                let has_entries = conversation_view
                    .read(cx)
                    .root_thread_view()
                    .is_some_and(|tv| !tv.read(cx).thread.read(cx).entries().is_empty());
                if has_entries {
                    return true;
                }

                conversation_view
                    .read(cx)
                    .root_thread_view()
                    .is_some_and(|thread_view| {
                        let thread_view = thread_view.read(cx);
                        thread_view
                            .thread
                            .read(cx)
                            .draft_prompt()
                            .is_some_and(|draft| !draft.is_empty())
                            || !thread_view
                                .message_editor
                                .read(cx)
                                .text(cx)
                                .trim()
                                .is_empty()
                    })
            }
        }
    }

    fn active_initial_content(&self, cx: &App) -> Option<AgentInitialContent> {
        let thread_view = self.active_thread_view(cx)?;
        let thread_view = thread_view.read(cx);
        let saved = thread_view
            .thread
            .read(cx)
            .draft_prompt()
            .map(|blocks| blocks.to_vec())
            .filter(|blocks| !blocks.is_empty());
        let blocks = saved.unwrap_or_else(|| {
            thread_view
                .message_editor
                .read(cx)
                .draft_content_blocks_snapshot(cx)
        });
        if blocks.is_empty() {
            return None;
        }
        Some(AgentInitialContent::ContentBlock {
            blocks,
            auto_submit: false,
        })
    }

    fn source_panel_initialization(
        source_workspace: &WeakEntity<Workspace>,
        cx: &App,
    ) -> Option<SourcePanelInitialization> {
        let source_workspace = source_workspace.upgrade()?;
        let source_panel = source_workspace.read(cx).panel::<AgentPanel>(cx)?;
        let source_panel = source_panel.read(cx);
        Some(SourcePanelInitialization {
            agent: source_panel.selected_agent(cx),
            initial_content: source_panel.active_initial_content(cx),
        })
    }

    pub fn initialize_from_source_workspace_if_needed(
        &mut self,
        source_workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.has_open_project(cx) {
            return false;
        }

        if self.destination_has_meaningful_state(cx) {
            return false;
        }

        let Some(initialization) = Self::source_panel_initialization(&source_workspace, cx) else {
            return false;
        };

        let mut initialized = false;
        if self.selected_agent != initialization.agent {
            self.selected_agent = initialization.agent.clone();
            self.serialize(cx);
            initialized = true;
        }

        if let Some(initial_content) = initialization.initial_content {
            let thread = self.create_agent_thread_with_server(
                initialization.agent,
                None,
                None,
                None,
                None,
                Some(initial_content),
                None,
                AgentThreadSource::AgentPanel,
                window,
                cx,
            );
            self.draft_thread = Some(thread.conversation_view.clone());
            self.observe_draft_editor(&thread.conversation_view, cx);
            self.set_base_view(thread.into(), false, window, cx);
            true
        } else {
            if initialized
                && matches!(
                    &self.base_view,
                    BaseView::AgentThread { conversation_view }
                        if self.draft_thread.as_ref().is_some_and(|draft| {
                            draft.entity_id() == conversation_view.entity_id()
                        })
                )
            {
                self.activate_draft(false, AgentThreadSource::AgentPanel, window, cx);
            } else if initialized {
                cx.notify();
            }
            initialized
        }
    }

    fn is_title_editor_focused(&self, window: &Window, cx: &Context<Self>) -> bool {
        match self.visible_surface() {
            VisibleSurface::AgentThread(conversation_view) => conversation_view
                .read(cx)
                .root_thread_view()
                .is_some_and(|view| view.read(cx).title_editor.read(cx).is_focused(window)),
            VisibleSurface::Terminal(_) => self
                .active_terminal_id()
                .and_then(|id| self.terminals.get(&id))
                .and_then(|terminal| terminal.title_editor.as_ref())
                .is_some_and(|editor| editor.read(cx).is_focused(window)),
            _ => false,
        }
    }

    fn should_show_title_edit(&self, window: &Window, cx: &Context<Self>) -> bool {
        matches!(
            self.visible_surface(),
            VisibleSurface::AgentThread(_) | VisibleSurface::Terminal(_)
        ) && self.has_open_project(cx)
            && !self.is_title_editor_focused(window, cx)
    }

    fn render_title_view(&self, window: &mut Window, cx: &Context<Self>) -> AnyElement {
        let content = match self.visible_surface() {
            VisibleSurface::AgentThread(conversation_view) => {
                let server_view_ref = conversation_view.read(cx);
                let native_thread = server_view_ref.as_native_thread(cx);
                let is_generating_title = native_thread
                    .as_ref()
                    .is_some_and(|thread| thread.read(cx).is_generating_title());
                let title_generation_error = native_thread
                    .as_ref()
                    .and_then(|thread| thread.read(cx).title_generation_error());

                if let Some(title_editor) = server_view_ref
                    .root_thread_view()
                    .map(|r| r.read(cx).title_editor.clone())
                {
                    if is_generating_title {
                        Label::new(server_view_ref.title(cx))
                            .color(Color::Muted)
                            .truncate()
                            .with_animation(
                                "generating_title",
                                Animation::new(Duration::from_secs(2))
                                    .repeat()
                                    .with_easing(pulsating_between(0.4, 0.8)),
                                |label, delta| label.alpha(delta),
                            )
                            .into_any_element()
                    } else {
                        let editable_title = div()
                            .flex_1()
                            .on_action({
                                let conversation_view = conversation_view.downgrade();
                                move |_: &menu::Confirm, window, cx| {
                                    if let Some(conversation_view) = conversation_view.upgrade() {
                                        conversation_view
                                            .read(cx)
                                            .activation_focus_handle(cx)
                                            .focus(window, cx);
                                    }
                                }
                            })
                            .on_action({
                                let conversation_view = conversation_view.downgrade();
                                move |_: &editor::actions::Cancel, window, cx| {
                                    if let Some(conversation_view) = conversation_view.upgrade() {
                                        conversation_view
                                            .read(cx)
                                            .activation_focus_handle(cx)
                                            .focus(window, cx);
                                    }
                                }
                            })
                            .child(title_editor);

                        if let Some(title_generation_error) = title_generation_error {
                            h_flex()
                                .w_full()
                                .gap_1()
                                .child(editable_title)
                                .child(
                                    IconButton::new("retry-thread-title", IconName::XCircle)
                                        .icon_color(Color::Error)
                                        .icon_size(IconSize::Small)
                                        .tooltip(move |_window, cx| {
                                            Tooltip::with_meta(
                                                "タイトルの生成に失敗しました。クリックすると再試行します。",
                                                None,
                                                title_generation_error.clone(),
                                                cx,
                                            )
                                        })
                                        .on_click({
                                            let conversation_view = conversation_view.clone();
                                            let workspace = self.workspace.clone();
                                            move |_event, _window, cx| {
                                                Self::handle_regenerate_thread_title(
                                                    conversation_view.clone(),
                                                    workspace.clone(),
                                                    cx,
                                                );
                                            }
                                        }),
                                )
                                .into_any_element()
                        } else {
                            editable_title.w_full().into_any_element()
                        }
                    }
                } else {
                    Label::new(conversation_view.read(cx).title(cx))
                        .color(Color::Muted)
                        .truncate()
                        .into_any_element()
                }
            }
            VisibleSurface::Terminal(_) => {
                if let Some((terminal_id, title_editor, title)) =
                    self.active_terminal_id().and_then(|terminal_id| {
                        self.terminals.get(&terminal_id).map(|terminal| {
                            (
                                terminal_id,
                                terminal.title_editor.clone(),
                                terminal.title(cx),
                            )
                        })
                    })
                {
                    if let Some(title_editor) = title_editor {
                        div()
                            .flex_1()
                            .on_action(cx.listener(move |this, _: &menu::Confirm, window, cx| {
                                this.stop_editing_terminal_title(terminal_id, true, window, cx);
                            }))
                            .on_action(cx.listener(
                                move |this, _: &editor::actions::Cancel, window, cx| {
                                    this.stop_editing_terminal_title(terminal_id, true, window, cx);
                                },
                            ))
                            .child(title_editor)
                            .into_any_element()
                    } else {
                        div()
                            .id("terminal-title")
                            .flex_1()
                            .cursor_text()
                            .overflow_x_scroll()
                            .child(Label::new(title).color(Color::Muted).single_line())
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.edit_terminal_title(terminal_id, window, cx);
                            }))
                            .into_any_element()
                    }
                } else {
                    Label::new("ターミナル").into_any_element()
                }
            }
            VisibleSurface::PaneItem => Label::new(self.pane_item_toolbar_title(cx))
                .color(Color::Muted)
                .truncate()
                .into_any_element(),

            VisibleSurface::Uninitialized => {
                Label::new("エージェント").truncate().into_any_element()
            }
        };

        let toolbar_bg = cx.theme().colors().tab_bar_background;
        let gradient_overlay = GradientFade::new(toolbar_bg, toolbar_bg, toolbar_bg)
            .width(px(64.0))
            .right(px(0.0))
            .gradient_stop(0.75);
        // The fade gradient renders as a visible patch on transparent windows
        // (the title already truncates).
        let opaque_window =
            cx.theme().window_background_appearance() == gpui::WindowBackgroundAppearance::Opaque;

        h_flex()
            .key_context("TitleEditor")
            .group("title_editor")
            .flex_grow_1()
            .w_full()
            .min_w_0()
            .max_w_full()
            .overflow_x_hidden()
            .child(content)
            .when(self.should_show_title_edit(window, cx), |this| {
                this.when(opaque_window, |this| this.child(gradient_overlay))
                    .child(
                        h_flex()
                            .visible_on_hover("title_editor")
                            .absolute()
                            .right_0()
                            .h_full()
                            .bg(cx.theme().colors().tab_bar_background)
                            .child(
                                IconButton::new("edit_tile", IconName::Pencil)
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text("スレッドタイトルを編集")),
                            ),
                    )
            })
            .into_any()
    }

    fn show_no_thread_summary_model_toast(workspace: Entity<Workspace>, cx: &mut App) {
        workspace.update(cx, |workspace, cx| {
            let toast = StatusToast::new(
                "スレッドタイトルの要約に使用するモデルが構成されていません。",
                cx,
                |this, _cx| {
                    this.icon(
                        Icon::new(IconName::Warning)
                            .size(IconSize::Small)
                            .color(Color::Warning),
                    )
                    .dismiss_button(true)
                },
            );
            workspace.toggle_status_toast(toast, cx);
        });
    }

    fn handle_regenerate_thread_title(
        conversation_view: Entity<ConversationView>,
        workspace: WeakEntity<Workspace>,
        cx: &mut App,
    ) {
        match Self::regenerate_conversation_thread_title(conversation_view, cx) {
            ThreadTitleRegenerationResult::NoModel => {
                if let Some(workspace) = workspace.upgrade() {
                    Self::show_no_thread_summary_model_toast(workspace, cx);
                }
            }
            ThreadTitleRegenerationResult::NotOpen
            | ThreadTitleRegenerationResult::Started
            | ThreadTitleRegenerationResult::AlreadyGenerating => {}
        }
    }

    fn render_panel_options_menu(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let focus_handle = self.focus_handle(cx);
        // Resolve menu shortcuts at the thread root; the active editor can
        // shadow panel-level commands such as ManageSkills.
        let menu_action_context = match &self.base_view {
            BaseView::AgentThread { conversation_view } => conversation_view
                .read(cx)
                .active_thread()
                .map(|thread| thread.read(cx).focus_handle.clone())
                .unwrap_or_else(|| focus_handle.clone()),
            _ => focus_handle.clone(),
        };
        let showing_terminal = matches!(self.visible_surface(), VisibleSurface::Terminal(_));
        let fs = self.fs.clone();

        let conversation_view = match &self.base_view {
            BaseView::AgentThread { conversation_view } => Some(conversation_view.clone()),
            _ => None,
        };

        let can_regenerate_thread_title =
            conversation_view.as_ref().is_some_and(|conversation_view| {
                let conversation_view = conversation_view.read(cx);
                conversation_view.has_user_submitted_prompt(cx)
                    && conversation_view
                        .as_native_thread(cx)
                        .is_some_and(|thread| !thread.read(cx).is_generating_title())
            });

        let has_thread_messages = conversation_view.as_ref().is_some_and(|conversation_view| {
            conversation_view.read(cx).has_user_submitted_prompt(cx)
        });

        let has_auth_methods = match &self.base_view {
            BaseView::AgentThread { conversation_view } => {
                conversation_view.read(cx).has_auth_methods()
            }
            _ => false,
        };
        let supports_logout = self
            .active_conversation_view()
            .is_some_and(|conversation_view| conversation_view.read(cx).supports_logout());

        let project_agents_md_path = project_agents_md_path(&self.project, true, cx);

        let global_agents_md_loaded = UserAgentsMd::global(cx)
            .and_then(|md| md.content())
            .is_some();

        let workspace = self.workspace.clone();

        PopoverMenu::new("agent-options-menu")
            .trigger_with_tooltip(
                IconButton::new("agent-options-menu", IconName::Ellipsis)
                    .icon_size(IconSize::Small),
                move |_window, cx| {
                    Tooltip::for_action_in(
                        "エージェントメニューの表示切り替え",
                        &ToggleOptionsMenu,
                        &focus_handle,
                        cx,
                    )
                },
            )
            .anchor(Anchor::TopRight)
            .with_handle(self.agent_panel_menu_handle.clone())
            .menu({
                move |window, cx| {
                    Some(ContextMenu::build(window, cx, |mut menu, _window, cx| {
                        menu = menu.context(menu_action_context.clone());

                        // ADR-F36: 見ているスレッドをタブへ出す導線。パネルは
                        // 空の下書きに戻り、以降はタブ機構 (分割 / 別パネルへの
                        // ドラッグ / 別ウィンドウ) が効く。
                        if conversation_view.is_some() {
                            menu = menu
                                .header("表示")
                                .action("スレッドをタブで開く", Box::new(OpenThreadInTab))
                                .action("スレッドを右に分割して開く", Box::new(OpenThreadInSplit))
                                .separator();
                        }

                        // ADR-F49 決定 19 (8c): 動いているスレッドを外へ移す
                        // 入口。確認 (新しいセッションになる旨を含む) はビュー側。
                        if let Some(conversation_view) = conversation_view.as_ref()
                            && conversation_view.read(cx).can_restart_outside_sandbox(cx)
                        {
                            let conversation_view = conversation_view.downgrade();
                            menu = menu
                                .header("サンドボックス")
                                .entry(
                                    crate::conversation_view::RESTART_OUTSIDE_SANDBOX_LABEL,
                                    None,
                                    move |window, cx| {
                                        conversation_view
                                            .update(cx, |view, cx| {
                                                view.restart_outside_sandbox(window, cx)
                                            })
                                            .log_err();
                                    },
                                )
                                .separator();
                        }

                        if has_thread_messages {
                            menu = menu.header("現在のスレッド");

                            if let Some(conversation_view) = conversation_view.as_ref() {
                                if can_regenerate_thread_title {
                                    menu =
                                        menu.entry("スレッドタイトルを再生成", None, {
                                            let conversation_view = conversation_view.clone();
                                            let workspace = workspace.clone();
                                            move |_, cx| {
                                                Self::handle_regenerate_thread_title(
                                                    conversation_view.clone(),
                                                    workspace.clone(),
                                                    cx,
                                                );
                                            }
                                        });
                                }

                                let root_thread_view =
                                    conversation_view.read(cx).root_thread_view();
                                if let Some(thread_view) = root_thread_view {
                                    let workspace = workspace.clone();
                                    menu = menu.entry("スレッドを Markdown で開く", None, {
                                        move |window, cx| {
                                            if let Some(workspace) = workspace.upgrade() {
                                                thread_view.update(cx, |thread_view, cx| {
                                                    thread_view
                                                        .open_thread_as_markdown(
                                                            workspace, window, cx,
                                                        )
                                                        .detach_and_log_err(cx);
                                                });
                                            }
                                        }
                                    });
                                }

                                menu = menu.separator();
                            }
                        }

                        if !showing_terminal {
                            menu = menu
                                .header("MCP サーバー")
                                .action(
                                    "サーバーを追加…",
                                    Box::new(zed_actions::OpenSettingsAt {
                                        path: "context_servers".to_string(),
                                        target: None,
                                    }),
                                )
                                .action(
                                    "新しいサーバーをインストール…",
                                    Box::new(zed_actions::Extensions {
                                        category_filter: Some(
                                            zed_actions::ExtensionCategoryFilter::ContextServers,
                                        ),
                                        id: None,
                                    }),
                                )
                                .separator()
                                .header("コンテキスト")
                                .action("スキル", Box::new(ManageSkills));

                            if project_agents_md_path.is_some() || global_agents_md_loaded {
                                if global_agents_md_loaded {
                                    let workspace = workspace.clone();

                                    menu = menu.custom_entry(
                                        |_window, _cx| {
                                            h_flex()
                                                .w_full()
                                                .gap_1()
                                                .child(Label::new("グローバルルールを開く"))
                                                .child(
                                                    Label::new("(AGENTS.md)")
                                                        .color(Color::Muted)
                                                        .size(LabelSize::Small),
                                                )
                                                .into_any_element()
                                        },
                                        move |window, cx| {
                                            workspace
                                                .update(cx, |workspace, cx| {
                                                    open_global_rules(workspace, window, cx);
                                                })
                                                .log_err();
                                        },
                                    );
                                }

                                if project_agents_md_path.is_some() {
                                    let workspace = workspace.clone();
                                    menu = menu.custom_entry(
                                        |_window, _cx| {
                                            h_flex()
                                                .w_full()
                                                .gap_1()
                                                .child(Label::new("プロジェクトルールを開く"))
                                                .child(
                                                    Label::new("(AGENTS.md)")
                                                        .color(Color::Muted)
                                                        .size(LabelSize::Small),
                                                )
                                                .into_any_element()
                                        },
                                        move |window, cx| {
                                            workspace
                                                .update(cx, |workspace, cx| {
                                                    open_project_rules(workspace, window, cx);
                                                })
                                                .log_err();
                                        },
                                    );
                                }
                            }

                            menu = menu
                                .separator()
                                .action("プロファイル", Box::new(ManageProfiles::default()));
                        }

                        menu = menu
                            .action("設定", Box::new(OpenSettings))
                            .separator()
                            .action(
                                "パネル内のスレッド履歴の切り替え",
                                Box::new(TogglePanelThreadHistory),
                            )
                            .submenu("タブバー", {
                                let fs = fs.clone();
                                move |mut menu, _window, cx| {
                                    let current = AgentSettings::get_global(cx).panel_tab_bar;
                                    for (mode, label) in [
                                        (PanelTabBar::Auto, "自動"),
                                        (PanelTabBar::Always, "常に表示"),
                                        (PanelTabBar::Never, "表示しない"),
                                    ] {
                                        let fs = fs.clone();
                                        menu = menu.toggleable_entry(
                                            label,
                                            current == mode,
                                            IconPosition::Start,
                                            None,
                                            move |_window, cx| {
                                                update_settings_file(
                                                    fs.clone(),
                                                    cx,
                                                    move |settings, _| {
                                                        settings
                                                            .agent
                                                            .get_or_insert_default()
                                                            .set_panel_tab_bar(mode);
                                                    },
                                                );
                                            },
                                        );
                                    }
                                    menu
                                }
                            })
                            .when(AgentSettings::get_global(cx).sidebar_enabled, |menu| {
                                menu.action(
                                    "スレッドサイドバーの切り替え",
                                    Box::new(ToggleWorkspaceSidebar),
                                )
                            });

                        if has_auth_methods || supports_logout {
                            menu = menu.separator()
                        }
                        if has_auth_methods {
                            menu = menu.action("再認証", Box::new(ReauthenticateAgent))
                        }
                        if supports_logout {
                            menu = menu.action("ログアウト", Box::new(LogoutAgent))
                        }

                        menu
                    }))
                }
            })
    }

    fn render_no_project_state(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let focus_handle = self.focus_handle(cx);

        ProjectEmptyState::new(
            "エージェントパネル",
            focus_handle.clone(),
            KeyBinding::for_action_in(&workspace::Open::default(), &focus_handle, cx),
        )
        .on_open_project(|_, window, cx| {
            telemetry::event!("Agent Panel Add Project Clicked");
            window.dispatch_action(workspace::Open::default().boxed_clone(), cx);
        })
        .on_clone_repo(|_, window, cx| {
            telemetry::event!("Agent Panel Clone Repo Clicked");
            window.dispatch_action(git::Clone.boxed_clone(), cx);
        })
    }

    fn render_toolbar(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let agent_server_store = self.project.read(cx).agent_server_store().clone();

        let focus_handle = self.focus_handle(cx);

        let can_create_entries = self.has_open_project(cx);
        let supports_terminal = self.supports_terminal(cx);
        let showing_terminal = matches!(self.visible_surface(), VisibleSurface::Terminal(_));
        let showing_pane_item = matches!(self.visible_surface(), VisibleSurface::PaneItem);

        let (selected_agent_custom_icon, selected_agent_label) = if showing_terminal {
            (None, SharedString::from("ターミナル"))
        } else if showing_pane_item {
            (None, self.pane_item_toolbar_title(cx))
        } else if let Agent::Custom { id, .. } = &self.selected_agent {
            let store = agent_server_store.read(cx);
            let icon = store.agent_icon(&id);

            let label = store
                .agent_display_name(&id)
                .unwrap_or_else(|| self.selected_agent.label());
            (icon, label)
        } else {
            (None, self.selected_agent.label())
        };

        let new_thread_menu_builder: Rc<
            dyn Fn(&mut Window, &mut App) -> Option<Entity<ContextMenu>>,
        > = {
            let selected_agent = self.selected_agent.clone();
            let is_agent_selected = move |agent: Agent| selected_agent == agent;

            let workspace = self.workspace.clone();
            let is_via_collab = workspace
                .update(cx, |workspace, cx| {
                    workspace.project().read(cx).is_via_collab()
                })
                .unwrap_or_default();

            let focus_handle = focus_handle.clone();
            let agent_server_store = agent_server_store;

            Rc::new(move |window, cx| {
                Some(ContextMenu::build(window, cx, |menu, _window, cx| {
                    menu.context(focus_handle.clone())
                        .item(
                            ContextMenuEntry::new("Zed Agent")
                                .when(
                                    !showing_terminal && is_agent_selected(Agent::NativeAgent),
                                    |this| this.action(Box::new(NewThread)),
                                )
                                .icon(IconName::ZedAgent)
                                .icon_color(Color::Muted)
                                .handler({
                                    let workspace = workspace.clone();
                                    move |window, cx| {
                                        if let Some(workspace) = workspace.upgrade() {
                                            workspace.update(cx, |workspace, cx| {
                                                if let Some(panel) =
                                                    workspace.panel::<AgentPanel>(cx)
                                                {
                                                    panel.update(cx, |panel, cx| {
                                                        panel.selected_agent = Agent::NativeAgent;
                                                        panel.activate_new_thread(
                                                            true,
                                                            AgentThreadSource::AgentPanel,
                                                            window,
                                                            cx,
                                                        );
                                                    });
                                                }
                                            });
                                        }
                                    }
                                }),
                        )
                        .when(supports_terminal, |menu| {
                            menu.item(
                                ContextMenuEntry::new("ターミナル")
                                    .when(showing_terminal, |this| this.action(Box::new(NewThread)))
                                    .when(!showing_terminal, |this| {
                                        this.action(Box::new(NewTerminalThread))
                                    })
                                    .icon(IconName::Terminal)
                                    .icon_color(Color::Muted)
                                    .handler({
                                        let workspace = workspace.clone();
                                        move |window, cx| {
                                            if let Some(workspace) = workspace.upgrade() {
                                                workspace.update(cx, |workspace, cx| {
                                                    if let Some(panel) =
                                                        workspace.panel::<AgentPanel>(cx)
                                                    {
                                                        panel.update(cx, |panel, cx| {
                                                            panel.new_terminal(
                                                                Some(workspace),
                                                                AgentThreadSource::AgentPanel,
                                                                window,
                                                                cx,
                                                            );
                                                        });
                                                    }
                                                });
                                            }
                                        }
                                    }),
                            )
                            // 公式 CLI はこのパネルのタブで動く (ADR-F53)。
                            // 起動は `LaunchClaudeCode` に任せる: ここでパネルを
                            // 直接触るとペインのリースが二重になる。
                            .item(
                                ContextMenuEntry::new("Claude Code を起動")
                                    .action(Box::new(
                                        zed_actions::terminal::LaunchClaudeCode::default(),
                                    ))
                                    .icon(IconName::Terminal)
                                    .icon_color(Color::Muted)
                                    .documentation_aside(
                                        crate::ui::documentation_aside_side(cx),
                                        |_| {
                                            Label::new(
                                                "公式の Claude Code CLI をこのパネルのタブで\
                                                 起動します。ログインは CLI の画面で行います。",
                                            )
                                            .into_any_element()
                                        },
                                    )
                                    .handler(move |window, cx| {
                                        window.dispatch_action(
                                            Box::new(
                                                zed_actions::terminal::LaunchClaudeCode::default(),
                                            ),
                                            cx,
                                        );
                                    }),
                            )
                            // ADR-F49 決定 19: サンドボックスが使える環境でも、
                            // 外で動かしたい人の入口。確認はアクション側で毎回
                            // (設定で常に外にしていれば出さない)。
                            .item(
                                ContextMenuEntry::new("Claude Code をサンドボックスの外で起動…")
                                    .action(Box::new(
                                        zed_actions::terminal::LaunchClaudeCodeOutsideSandbox::default(),
                                    ))
                                    .icon(IconName::Terminal)
                                    .icon_color(Color::Muted)
                                    .documentation_aside(
                                        crate::ui::documentation_aside_side(cx),
                                        |_| {
                                            Label::new(
                                                "書き込みの制限がなくなり、通信の制限は\
                                                 プロキシを読むものにだけ効きます。\n\
                                                 ログインは普段の Claude Code と共有します。",
                                            )
                                            .into_any_element()
                                        },
                                    )
                                    .handler(move |window, cx| {
                                        window.dispatch_action(
                                            Box::new(
                                                zed_actions::terminal::LaunchClaudeCodeOutsideSandbox::default(),
                                            ),
                                            cx,
                                        );
                                    }),
                            )
                        })
                        .map(|mut menu| {
                            let agent_items =
                                external_agent_menu_items(agent_server_store.read(cx), cx);

                            if !agent_items.is_empty() {
                                menu = menu.separator().header("外部エージェント");
                            }
                            for item in &agent_items {
                                let mut entry = ContextMenuEntry::new(item.display_name.clone());

                                if let Some(icon_path) = item.icon_path.clone() {
                                    entry = entry.custom_icon_svg(icon_path);
                                } else {
                                    entry = entry.icon(IconName::Sparkle);
                                }

                                entry = entry
                                    .when(
                                        !showing_terminal
                                            && is_agent_selected(Agent::Custom {
                                                id: item.id.clone(),
                                            }),
                                        |this| this.action(Box::new(NewThread)),
                                    )
                                    .icon_color(Color::Muted)
                                    .disabled(is_via_collab)
                                    .handler({
                                        let workspace = workspace.clone();
                                        let agent_id = item.id.clone();
                                        move |window, cx| {
                                            if let Some(workspace) = workspace.upgrade() {
                                                workspace.update(cx, |workspace, cx| {
                                                    if let Some(panel) =
                                                        workspace.panel::<AgentPanel>(cx)
                                                    {
                                                        panel.update(cx, |panel, cx| {
                                                            panel.new_external_agent_thread(
                                                                &NewExternalAgentThread {
                                                                    agent: agent_id.clone(),
                                                                },
                                                                window,
                                                                cx,
                                                            );
                                                        });
                                                    }
                                                });
                                            }
                                        }
                                    });

                                menu = menu.item(entry);
                            }

                            menu
                        })
                        .separator()
                        .item(
                            ContextMenuEntry::new("エージェントを追加")
                                .icon(IconName::Plus)
                                .icon_color(Color::Muted)
                                .handler({
                                    move |window, cx| {
                                        window
                                            .dispatch_action(Box::new(zed_actions::AcpRegistry), cx)
                                    }
                                }),
                        )
                }))
            })
        };

        let is_thread_loading = self
            .active_conversation_view()
            .map(|thread| thread.read(cx).is_loading())
            .unwrap_or(false);

        let has_custom_icon = selected_agent_custom_icon.is_some();
        let selected_agent_builtin_icon = if showing_terminal {
            Some(IconName::Terminal)
        } else if showing_pane_item {
            // タブ自身のアイコンはタブバーが出しているので重ねない。ここに
            // エージェントのアイコンを出すと、スレッドを見ているように見える。
            None
        } else {
            self.selected_agent.icon()
        };
        let selected_agent_label_for_tooltip = selected_agent_label.clone();

        let selected_agent = div()
            .id("selected_agent_icon")
            .px_0p5()
            .when_some(selected_agent_custom_icon, |this, icon_path| {
                this.child(
                    Icon::from_external_svg(icon_path)
                        .color(Color::Muted)
                        .size(IconSize::Small),
                )
            })
            .when(!has_custom_icon, |this| {
                this.when_some(selected_agent_builtin_icon, |this, icon| {
                    this.child(Icon::new(icon).color(Color::Muted))
                })
            })
            .tooltip(move |_, cx| {
                Tooltip::with_meta(
                    selected_agent_label_for_tooltip.clone(),
                    None,
                    "選択中のエージェント",
                    cx,
                )
            });

        let selected_agent = if is_thread_loading {
            selected_agent
                .with_animation(
                    "pulsating-icon",
                    Animation::new(Duration::from_secs(1))
                        .repeat()
                        .with_easing(pulsating_between(0.2, 0.6)),
                    |icon, delta| icon.opacity(delta),
                )
                .into_any_element()
        } else {
            selected_agent.into_any_element()
        };

        enum ToolbarMode {
            Terminal,
            /// スレッドではないタブ。見出しはそのタブの名前。
            PaneItem,
            EmptyThread,
            ActiveThread,
        }

        let mode = if matches!(self.base_view, BaseView::Terminal { .. }) {
            ToolbarMode::Terminal
        } else if matches!(self.base_view, BaseView::PaneItem { .. }) {
            ToolbarMode::PaneItem
        } else if self.active_thread_has_messages(cx) {
            ToolbarMode::ActiveThread
        } else {
            ToolbarMode::EmptyThread
        };

        let is_full_screen = self.is_zoomed(window, cx);
        let (icon_name, tooltip_text) = if is_full_screen {
            (IconName::Minimize, "全画面表示を解除")
        } else {
            (IconName::Maximize, "全画面表示")
        };
        let full_screen_button = IconButton::new("toggle-full-screen", icon_name)
            .icon_size(IconSize::Small)
            .toggle_state(is_full_screen)
            .tooltip(move |_, cx| Tooltip::for_action(tooltip_text, &ToggleZoom, cx))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.toggle_zoom(&ToggleZoom, window, cx);
            }));

        let max_content_width = AgentSettings::get_global(cx).max_content_width;

        // 閉じた一覧の戻り道は、開いていたときの « と同じ左上に置く
        // (ADR-F53)。右側の同じトグルはそのあいだ隠して導線を 1 つにする。
        // 中身を `max_content_width` で中央に寄せる枠の外に置くのは、広い
        // パネルでもパネルの左端から動かさないため。
        let show_thread_history_open_button = self.thread_history_open_button_visible(cx);
        let thread_history_open_button = show_thread_history_open_button.then(|| {
            div()
                .debug_selector(|| "PANEL_THREAD_HISTORY_OPEN_BUTTON".into())
                .flex_none()
                .pl(DynamicSpacing::Base04.rems(cx))
                .child(
                    IconButton::new(
                        "open-panel-thread-history",
                        IconName::ThreadsSidebarLeftClosed,
                    )
                    .icon_size(IconSize::Small)
                    .tooltip(|_window, cx| {
                        Tooltip::for_action("スレッドの一覧を開く", &TogglePanelThreadHistory, cx)
                    })
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.toggle_panel_thread_history(&TogglePanelThreadHistory, window, cx);
                    })),
                )
        });

        let base_container = h_flex()
            .debug_selector(|| "AGENT_PANEL_TOOLBAR_CONTENT".into())
            .h_full()
            .flex_1()
            .min_w_0()
            .when(
                matches!(mode, ToolbarMode::EmptyThread | ToolbarMode::ActiveThread),
                |this| this.when_some(max_content_width, |this, max_w| this.max_w(max_w).mx_auto()),
            )
            .justify_between();

        let empty_thread_title = matches!(mode, ToolbarMode::EmptyThread).then(|| {
            Label::new(format!("{} の新規スレッド", selected_agent_label))
                .color(Color::Muted)
                .truncate()
                .into_any_element()
        });

        let toolbar_content = {
            let new_thread_menu = PopoverMenu::new("new_thread_menu")
                .trigger_with_tooltip(
                    IconButton::new("new_thread_menu_btn", IconName::Plus)
                        .icon_size(IconSize::Small),
                    {
                        move |_window, cx| {
                            Tooltip::for_action_in(
                                "新規スレッド\u{2026}",
                                &ToggleNewThreadMenu,
                                &focus_handle,
                                cx,
                            )
                        }
                    },
                )
                .anchor(Anchor::TopRight)
                .with_handle(self.new_thread_menu_handle.clone())
                .menu(move |window, cx| new_thread_menu_builder(window, cx));

            let sandbox_status = self
                .active_conversation_view()
                .and_then(|conversation_view| conversation_view.read(cx).root_thread_view())
                .and_then(|thread_view| {
                    thread_view.update(cx, |thread_view, cx| thread_view.render_sandbox_status(cx))
                });

            let thread_history_enabled = self.thread_history_enabled(cx);
            let thread_history_button = IconButton::new(
                "toggle-panel-thread-history",
                if thread_history_enabled {
                    IconName::ThreadsSidebarLeftOpen
                } else {
                    IconName::ThreadsSidebarLeftClosed
                },
            )
            .icon_size(IconSize::Small)
            .toggle_state(thread_history_enabled)
            .tooltip(move |_window, cx| {
                Tooltip::for_action(
                    if thread_history_enabled {
                        "パネル内のスレッド履歴を非表示"
                    } else {
                        "パネル内のスレッド履歴を表示"
                    },
                    &TogglePanelThreadHistory,
                    cx,
                )
            })
            .on_click(cx.listener(|this, _, window, cx| {
                this.toggle_panel_thread_history(&TogglePanelThreadHistory, window, cx);
            }));

            base_container
                .child(
                    h_flex()
                        .relative()
                        .h_full()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .gap(DynamicSpacing::Base04.rems(cx))
                        .pl(DynamicSpacing::Base04.rems(cx))
                        .child(selected_agent.into_any_element())
                        .child(match empty_thread_title {
                            Some(title) => title,
                            None => self.render_title_view(window, cx),
                        }),
                )
                .child(
                    h_flex()
                        .px_1()
                        .h_full()
                        .flex_none()
                        .gap_1()
                        .children(sandbox_status)
                        .when(
                            can_create_entries && !show_thread_history_open_button,
                            |this| {
                                this.child(
                                    div()
                                        .debug_selector(|| "PANEL_THREAD_HISTORY_TOGGLE".into())
                                        .child(thread_history_button),
                                )
                            },
                        )
                        .when(can_create_entries, |this| this.child(new_thread_menu))
                        .child(full_screen_button)
                        .child(self.render_panel_options_menu(window, cx)),
                )
                .into_any_element()
        };

        h_flex()
            .id("agent-panel-toolbar")
            .debug_selector(|| "AGENT_PANEL_TOOLBAR".into())
            .h(Tab::container_height(cx))
            .flex_shrink_0()
            .max_w_full()
            .bg(cx.theme().colors().tab_bar_background)
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .children(thread_history_open_button)
            .child(toolbar_content)
    }

    fn should_render_trial_end_upsell(&self, cx: &mut Context<Self>) -> bool {
        if TrialEndUpsell::dismissed(cx) {
            return false;
        }

        match &self.base_view {
            BaseView::AgentThread { .. } => {
                if LanguageModelRegistry::global(cx)
                    .read(cx)
                    .default_model()
                    .is_some_and(|model| {
                        model.provider.id() != language_model::ZED_CLOUD_PROVIDER_ID
                    })
                {
                    return false;
                }
            }
            BaseView::Terminal { .. } | BaseView::PaneItem { .. } | BaseView::Uninitialized => {
                return false;
            }
        }

        let plan = self.user_store.read(cx).plan();
        let has_previous_trial = self.user_store.read(cx).trial_started_at().is_some();

        plan.is_some_and(|plan| plan == Plan::ZedFree) && has_previous_trial
    }

    fn dismiss_ai_onboarding(&mut self, cx: &mut Context<Self>) {
        self.new_user_onboarding_upsell_dismissed
            .store(true, Ordering::Release);
        OnboardingUpsell::set_dismissed(true, cx);
        cx.notify();
    }

    fn should_render_new_user_onboarding(&mut self, cx: &mut Context<Self>) -> bool {
        if self
            .new_user_onboarding_upsell_dismissed
            .load(Ordering::Acquire)
        {
            return false;
        }

        let user_store = self.user_store.read(cx);

        if user_store.plan().is_some_and(|plan| plan == Plan::ZedPro)
            && user_store
                .subscription_period()
                .and_then(|period| period.0.checked_add_days(chrono::Days::new(1)))
                .is_some_and(|date| date < chrono::Utc::now())
        {
            if !self
                .new_user_onboarding_upsell_dismissed
                .load(Ordering::Acquire)
            {
                self.dismiss_ai_onboarding(cx);
            }
            return false;
        }

        let has_configured_non_zed_providers = LanguageModelRegistry::read_global(cx)
            .visible_providers()
            .iter()
            .any(|provider| {
                provider.is_authenticated(cx)
                    && provider.id() != language_model::ZED_CLOUD_PROVIDER_ID
            });

        match &self.base_view {
            BaseView::Uninitialized | BaseView::Terminal { .. } | BaseView::PaneItem { .. } => {
                false
            }
            BaseView::AgentThread { conversation_view } => {
                if conversation_view.read(cx).as_native_thread(cx).is_some() {
                    let history_is_empty = ThreadStore::global(cx).read(cx).is_empty();
                    history_is_empty || !has_configured_non_zed_providers
                } else {
                    false
                }
            }
        }
    }

    fn render_new_user_onboarding(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        if !self.should_render_new_user_onboarding(cx) {
            return None;
        }

        Some(
            div()
                .bg(cx.theme().colors().editor_background)
                .child(self.new_user_onboarding.clone()),
        )
    }

    fn render_trial_end_upsell(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        if !self.should_render_trial_end_upsell(cx) {
            return None;
        }

        Some(
            v_flex()
                .absolute()
                .inset_0()
                .size_full()
                .bg(cx.theme().colors().panel_background)
                .opacity(0.85)
                .block_mouse_except_scroll()
                .child(EndTrialUpsell::new(Arc::new({
                    let this = cx.entity();
                    move |_, cx| {
                        this.update(cx, |_this, cx| {
                            TrialEndUpsell::set_dismissed(true, cx);
                            cx.notify();
                        });
                    }
                }))),
        )
    }

    /// ファイルを落として文脈に足すための受け皿。
    ///
    /// `accepts_tabs` はタブ列 (ペイングループ) を出していないときだけ真。
    /// 出しているときにここでタブを取ると、パネルの中のペインへタブを移す
    /// 操作 (ADR-F36 Phase B) がこの覆いに全部吸われる。
    fn render_drag_target(&self, accepts_tabs: bool, cx: &Context<Self>) -> Div {
        let is_local = self.project.read(cx).is_local();
        div()
            .invisible()
            .absolute()
            .top_0()
            .right_0()
            .bottom_0()
            .left_0()
            .bg(cx.theme().colors().drop_target_background)
            .when(accepts_tabs, |this| {
                this.drag_over::<DraggedTab>(|this, _, _, _| this.visible())
                    .on_drop(cx.listener(move |this, tab: &DraggedTab, window, cx| {
                        let item = tab.pane.read(cx).item_for_index(tab.ix);
                        let project_paths = item
                            .and_then(|item| item.project_path(cx))
                            .into_iter()
                            .collect::<Vec<_>>();
                        this.handle_drop(project_paths, vec![], window, cx);
                    }))
            })
            .drag_over::<DraggedSelection>(|this, _, _, _| this.visible())
            .when(is_local, |this| {
                this.drag_over::<ExternalPaths>(|this, _, _, _| this.visible())
            })
            .on_drop(
                cx.listener(move |this, selection: &DraggedSelection, window, cx| {
                    let project_paths = selection
                        .items()
                        .filter_map(|item| this.project.read(cx).path_for_entry(item.entry_id, cx))
                        .collect::<Vec<_>>();
                    this.handle_drop(project_paths, vec![], window, cx);
                }),
            )
            .on_drop(cx.listener(move |this, paths: &ExternalPaths, window, cx| {
                this.handle_external_paths_drop(paths, window, cx);
            }))
    }

    fn handle_external_paths_drop(
        &mut self,
        paths: &ExternalPaths,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if matches!(&self.base_view, BaseView::Terminal { .. }) {
            // Terminal drops should match normal terminal views by pasting raw OS paths.
            // The agent-thread path below converts paths to project paths, which can add
            // worktrees and is only needed when attaching files to a conversation.
            self.paste_external_paths_into_active_terminal(paths, window, cx);
            return;
        }

        let BaseView::AgentThread { conversation_view } = &self.base_view else {
            return;
        };
        let conversation_view = conversation_view.clone();
        let tasks = paths
            .paths()
            .iter()
            .map(|path| Workspace::project_path_for_path(self.project.clone(), path, false, cx))
            .collect::<Vec<_>>();
        cx.spawn_in(window, async move |_this, cx| {
            let mut paths = vec![];
            let mut added_worktrees = vec![];
            let opened_paths = futures::future::join_all(tasks).await;
            for entry in opened_paths {
                if let Some((worktree, project_path)) = entry.log_err() {
                    added_worktrees.push(worktree);
                    paths.push(project_path);
                }
            }
            conversation_view
                .update_in(cx, |conversation_view, window, cx| {
                    conversation_view.insert_dragged_files(paths, added_worktrees, window, cx);
                })
                .log_err();
        })
        .detach();
    }

    fn paste_external_paths_into_active_terminal(
        &mut self,
        paths: &ExternalPaths,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let BaseView::Terminal { terminal_id } = &self.base_view else {
            return;
        };

        if !self.project.read(cx).is_local() {
            return;
        }

        let Some(terminal_view) = self
            .terminals
            .get(terminal_id)
            .map(|terminal| terminal.view.clone())
        else {
            return;
        };

        terminal_view.update(cx, |terminal_view, cx| {
            terminal_view.add_paths_to_terminal(paths.paths(), window, cx);
        });
    }

    fn handle_drop(
        &mut self,
        paths: Vec<ProjectPath>,
        added_worktrees: Vec<Entity<Worktree>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match &self.base_view {
            BaseView::AgentThread { conversation_view } => {
                conversation_view.update(cx, |conversation_view, cx| {
                    conversation_view.insert_dragged_files(paths, added_worktrees, window, cx);
                });
            }
            BaseView::Terminal { terminal_id } => {
                let paths = {
                    let project = self.project.read(cx);
                    paths
                        .iter()
                        .filter_map(|project_path| project.absolute_path(project_path, cx))
                        .collect::<Vec<_>>()
                };

                if paths.is_empty() {
                    return;
                }

                if let Some(terminal_view) = self
                    .terminals
                    .get(terminal_id)
                    .map(|terminal| terminal.view.clone())
                {
                    terminal_view.update(cx, |terminal_view, cx| {
                        terminal_view.add_paths_to_terminal(&paths, window, cx);
                    });
                }
            }
            // スレッドではないタブには覆いを置いていないので、落とし物は
            // そのアイテム自身に届く (ここには来ない)。
            BaseView::PaneItem { .. } | BaseView::Uninitialized => {}
        }
    }

    fn key_context(&self) -> KeyContext {
        let mut key_context = KeyContext::new_with_defaults();
        key_context.add("AgentPanel");
        key_context
    }

    /// Puts the thread history list to the left of whatever surface the panel
    /// is showing. When the list is hidden the surface is returned untouched,
    /// so its left edge meets the dock's edge like any other panel's; the way
    /// back is the button at the left end of the toolbar.
    fn render_surface_with_thread_history(
        &self,
        surface: AnyElement,
        show_thread_history: bool,
    ) -> AnyElement {
        if !show_thread_history {
            return surface;
        }

        h_flex()
            .size_full()
            .min_h_0()
            .child(self.thread_history.clone())
            .child(v_flex().flex_1().min_w_0().size_full().child(surface))
            .into_any_element()
    }

    /// パネルの中のタブ列 (ADR-F36 Phase B)。ワークスペースが取れないときだけ
    /// `None` (ウィンドウを閉じている最中)。
    fn render_pane_group(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        self.workspace
            .update(cx, |workspace, cx| {
                let follower_states = Default::default();
                let workspace_handle = workspace.weak_handle();
                self.center
                    .render(
                        workspace.zoomed_item(),
                        None,
                        &workspace::PaneRenderContext {
                            follower_states: &follower_states,
                            active_call: workspace.active_call(),
                            active_pane: &self.active_pane,
                            app_state: workspace.app_state(),
                            project: workspace.project(),
                            workspace: &workspace_handle,
                        },
                        window,
                        cx,
                    )
                    .into_any_element()
            })
            .ok()
    }

    /// Records the panel's own width so the next render can tell whether the
    /// history list still fits. Measuring in an element keeps this correct for
    /// flexible docks and zoomed panels, where the stored dock size lies.
    fn render_width_probe(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let measured_panel_width = self.measured_panel_width.clone();
        let panel = cx.entity().downgrade();

        canvas(
            move |bounds: Bounds<Pixels>, _window, cx: &mut App| {
                let width = bounds.size.width;
                if measured_panel_width.get() == Some(width) {
                    return;
                }
                measured_panel_width.set(Some(width));
                cx.defer(move |cx| {
                    panel.update(cx, |_panel, cx| cx.notify()).ok();
                });
            },
            |_bounds, _state, _window, _cx| {},
        )
        .absolute()
        .size_full()
        .top_0()
        .left_0()
    }
}

impl Render for AgentPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // WARNING: Changes to this element hierarchy can have
        // non-obvious implications to the layout of children.
        //
        // If you need to change it, please confirm:
        // - The message editor expands (cmd-option-esc) correctly
        // - When expanded, the buttons at the bottom of the panel are displayed correctly
        // - Font size works as expected and can be changed with cmd-+/cmd-
        // - Scrolling in all views works as expected
        // - Files can be dropped into the panel
        let thread_history_fit = self.thread_history_fit();
        let show_thread_history = self.thread_history_visible(cx);
        let active_thread_id = self.active_thread_id(cx);
        let open_tabs = self.open_tab_rows(window, cx);
        self.thread_history.update(cx, |thread_history, cx| {
            thread_history.set_active_thread_id(active_thread_id, cx);
            thread_history.set_slim(thread_history_fit == ThreadHistoryFit::Slim, cx);
            thread_history.set_open_tabs(open_tabs, cx);
        });

        // 検索バーの操作 (次を検索・閉じるなど) をパネルの中で受け止める。
        // これが無いと、ワークスペース既定のハンドラーが中央のペインを
        // 相手にしてしまう (ADR-F53)。
        let root =
            search::buffer_search::register_pane_search_actions(v_flex(), self.active_pane.clone());
        let content = root
            .key_context(self.key_context())
            .relative()
            .size_full()
            .justify_between()
            .track_focus(&self.focus_handle)
            .bg(cx.theme().colors().panel_background)
            // 触ったペインを宛先にする。捕捉フェーズで拾うのは、タブや「+」が
            // クリックを消費してもここまで届かせるため (何も止めない)。
            .capture_any_mouse_down(cx.listener(|this, event: &MouseDownEvent, window, cx| {
                this.activate_pane_at(event.position, window, cx);
            }))
            .on_action(cx.listener(|this, action: &NewThread, window, cx| {
                this.new_thread(action, window, cx);
            }))
            .on_action(cx.listener(|this, _: &NewTerminalThread, window, cx| {
                cx.stop_propagation();
                this.new_terminal(None, AgentThreadSource::AgentPanel, window, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenSettings, window, cx| {
                this.open_configuration(window, cx);
            }))
            .on_action(cx.listener(Self::open_active_thread_as_markdown))
            .on_action(cx.listener(Self::manage_skills))
            .on_action(cx.listener(Self::toggle_options_menu))
            .on_action(cx.listener(Self::toggle_panel_thread_history))
            .on_action(cx.listener(Self::toggle_panel_tab_bar))
            .on_action(cx.listener(Self::increase_font_size))
            .on_action(cx.listener(Self::decrease_font_size))
            .on_action(cx.listener(Self::reset_font_size))
            .on_action(cx.listener(Self::toggle_zoom))
            .on_action(cx.listener(Self::toggle_terminal_thread_search))
            .on_action(cx.listener(|this, _: &ReauthenticateAgent, window, cx| {
                if let Some(conversation_view) = this.active_conversation_view() {
                    conversation_view.update(cx, |conversation_view, cx| {
                        conversation_view.reauthenticate(window, cx)
                    })
                }
            }))
            .on_action(cx.listener(|this, _: &LogoutAgent, window, cx| {
                if let Some(conversation_view) = this.active_conversation_view() {
                    conversation_view.update(cx, |conversation_view, cx| {
                        conversation_view.logout(window, cx)
                    })
                }
            }))
            .child(self.render_toolbar(window, cx))
            .children(self.render_new_user_onboarding(window, cx))
            .map(|parent| match self.visible_surface() {
                VisibleSurface::Uninitialized if !self.has_open_project(cx) => {
                    parent.child(self.render_no_project_state(cx))
                }
                VisibleSurface::Uninitialized => parent,
                // スレッドはパネルの中のペイングループが描く (ADR-F36 Phase B)。
                // `conversation_view` 自体は `ThreadItem` の中にある。
                VisibleSurface::AgentThread(_) => parent
                    .children(self.render_pane_group(window, cx).map(|pane_group| {
                        self.render_surface_with_thread_history(pane_group, show_thread_history)
                    }))
                    .child(self.render_drag_target(false, cx)),
                // スレッドではないタブ (公式 CLI の端末など) も同じペイン
                // グループが描く。落とし物の受け皿は**重ねない**: 端末のタブは
                // 自分でパスの貼り付けを受けるので、覆いを置くとそれを奪う。
                VisibleSurface::PaneItem => {
                    parent.children(self.render_pane_group(window, cx).map(|pane_group| {
                        self.render_surface_with_thread_history(pane_group, show_thread_history)
                    }))
                }
                VisibleSurface::Terminal(terminal_view) => {
                    let search_bar = self
                        .active_terminal_id()
                        .and_then(|terminal_id| self.terminals.get(&terminal_id))
                        .and_then(|terminal| terminal.search_bar.clone());
                    let terminal_content = v_flex()
                        .debug_selector(|| "PANEL_TERMINAL_SURFACE".into())
                        .size_full()
                        .when_some(search_bar, |this, search_bar| {
                            this.when(!search_bar.read(cx).is_dismissed(), |this| {
                                this.child(
                                    v_flex()
                                        .group("toolbar")
                                        .relative()
                                        .py(DynamicSpacing::Base06.rems(cx))
                                        .px(DynamicSpacing::Base08.rems(cx))
                                        .border_b_1()
                                        .border_color(cx.theme().colors().border_variant)
                                        .bg(cx.theme().colors().toolbar_background)
                                        .child(search_bar),
                                )
                            })
                        })
                        .child(terminal_view.clone());

                    parent
                        .child(self.render_surface_with_thread_history(
                            terminal_content.into_any_element(),
                            show_thread_history,
                        ))
                        .child(self.render_drag_target(true, cx))
                }
            })
            .child(self.render_width_probe(cx))
            .children(self.render_trial_end_upsell(window, cx));

        match self.visible_font_size() {
            WhichFontSize::AgentFont => {
                let theme_settings = ThemeSettings::get_global(cx);
                WithRemSize::new(theme_settings.agent_ui_font_size(cx))
                    .size_full()
                    .font_family(theme_settings.agent_ui_font_family().clone())
                    .child(content)
                    .into_any()
            }
            _ => content.into_any(),
        }
    }
}

struct OnboardingUpsell;

impl Dismissable for OnboardingUpsell {
    const KEY: &'static str = "dismissed-trial-upsell";
}

struct TrialEndUpsell;

impl Dismissable for TrialEndUpsell {
    const KEY: &'static str = "dismissed-trial-end-upsell";
}

/// Test-only helper methods
#[cfg(any(test, feature = "test-support"))]
impl AgentPanel {
    pub fn test_new(workspace: &Workspace, window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::new(workspace, window, cx)
    }

    /// Drops a thread's `ConversationView` wherever this panel is keeping it
    /// — the tab-less retained slot, or the panel's own tab, which is where a
    /// parked draft lives since ADR-F36 Phase B — without deleting its
    /// metadata or kvp state. Simulates the state right after a restart, in
    /// which only what is on disk is left.
    pub fn test_unload_retained_thread(
        &mut self,
        id: ThreadId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.retained_threads.remove(&id).is_some() {
            return true;
        }
        self.take_thread_from_tab(id, window, cx).is_some()
    }

    /// Points the panel at an agent without going through the toolbar's
    /// draft-rebuilding logic, so tests can pin it to `Agent::Stub`.
    pub fn test_set_selected_agent(&mut self, agent: Agent) {
        self.selected_agent = agent;
    }

    /// このパネルがそのスレッドを生かしているか: パネルのタブに載っている
    /// (ADR-F36 Phase B) か、タブを持たない保持枠にあるか。
    pub fn test_retains_thread(&self, id: ThreadId) -> bool {
        self.retained_threads.contains_key(&id) || self.tabbed_threads.contains_key(&id)
    }

    /// そのスレッドを「裏で」生かしているか (= 生かしているが、いま表に
    /// 出しているスレッドではない)。Phase B より前の `retained_threads` に
    /// 相当する。
    pub fn test_thread_is_in_background(&self, id: ThreadId) -> bool {
        let Some(view) = self.test_background_view(id) else {
            return false;
        };
        self.test_visible_view_id() != Some(view.entity_id())
    }

    /// 裏で生きているスレッドの数。
    pub fn test_background_thread_count(&self) -> usize {
        self.background_threads().count()
    }

    /// 裏でも表でも、このパネルが持っているスレッドのビュー。
    pub fn test_background_view(&self, id: ThreadId) -> Option<Entity<ConversationView>> {
        self.retained_threads
            .get(&id)
            .or_else(|| self.tabbed_threads.get(&id))
            .cloned()
    }

    fn test_visible_view_id(&self) -> Option<EntityId> {
        match &self.base_view {
            BaseView::AgentThread { conversation_view } => Some(conversation_view.entity_id()),
            _ => None,
        }
    }

    /// パネルのタブを 1 枚閉じる。閉じたスレッドが生成中なら、通常どおり
    /// 保持枠へ返る (返却は非同期なので `run_until_parked` が要る)。
    /// パネルのタブを全部閉じる。再起動を模して同じワークスペースにもう 1 つ
    /// パネルを作るテストで、古いパネルにタブを手放させるために使う。
    pub fn test_close_all_thread_tabs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // 最後の 1 枚を閉じたあとに新しい下書きを作らせない (作ると保存済みの
        // 「最後に見ていたスレッド」を上書きしてしまう)。旗を戻すのは
        // ペインのイベントを捌いたあと = `defer` で。
        self.restoring_layout = true;
        for thread_id in self.tabbed_threads.keys().copied().collect::<Vec<_>>() {
            self.test_close_thread_tab(thread_id, window, cx);
        }
        let panel = cx.entity();
        cx.defer(move |cx| {
            panel.update(cx, |panel, _| panel.restoring_layout = false);
        });
    }

    pub fn test_close_thread_tab(
        &mut self,
        id: ThreadId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(conversation_view) = self.take_thread_from_tab(id, window, cx) else {
            return false;
        };
        if self.test_visible_view_id() == Some(conversation_view.entity_id()) {
            self.base_view = BaseView::Uninitialized;
        }
        self.retain_running_thread(BaseView::AgentThread { conversation_view }, cx);
        true
    }

    /// Opens an external thread using an arbitrary AgentServer.
    ///
    /// This is a test-only helper that allows visual tests and integration tests
    /// to inject a stub server without modifying production code paths.
    /// Not compiled into production builds.
    pub fn open_external_thread_with_server(
        &mut self,
        server: Rc<dyn AgentServer>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ext_agent = Agent::Custom {
            id: server.agent_id(),
        };

        let thread = self.create_agent_thread_with_server(
            ext_agent,
            Some(server),
            None,
            None,
            None,
            None,
            None,
            AgentThreadSource::AgentPanel,
            window,
            cx,
        );
        self.set_base_view(thread.into(), true, window, cx);
    }

    /// Opens a restored external thread with an arbitrary AgentServer and
    /// a specific `resume_session_id` — as if we just restored from the KVP.
    ///
    /// Test-only helper. Not compiled into production builds.
    pub fn open_restored_thread_with_server(
        &mut self,
        server: Rc<dyn AgentServer>,
        resume_session_id: acp::SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ext_agent = Agent::Custom {
            id: server.agent_id(),
        };

        // The panel addresses threads by `ThreadId` after the draft work;
        // map the test-provided `session_id` back through the metadata
        // store so this helper still resumes the right thread.
        let resume_thread_id = ThreadMetadataStore::try_global(cx).and_then(|store| {
            store
                .read(cx)
                .entry_by_session(&resume_session_id)
                .map(|m| m.thread_id)
        });

        let thread = self.create_agent_thread_with_server(
            ext_agent,
            Some(server),
            resume_thread_id,
            None,
            None,
            None,
            None,
            AgentThreadSource::AgentPanel,
            window,
            cx,
        );
        self.set_base_view(thread.into(), true, window, cx);
    }

    /// Returns the currently active thread view, if any.
    ///
    /// This is a test-only accessor that exposes the private `active_thread_view()`
    /// method for test assertions. Not compiled into production builds.
    pub fn active_thread_view_for_tests(&self) -> Option<&Entity<ConversationView>> {
        self.active_conversation_view()
    }

    /// Creates a draft thread using a stub server and sets it as the active view.
    #[cfg(any(test, feature = "test-support"))]
    pub fn open_draft_with_server(
        &mut self,
        server: Rc<dyn AgentServer>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ext_agent = Agent::Custom {
            id: server.agent_id(),
        };
        let thread = self.create_agent_thread_with_server(
            ext_agent,
            Some(server),
            None,
            None,
            None,
            None,
            None,
            AgentThreadSource::AgentPanel,
            window,
            cx,
        );
        self.draft_thread = Some(thread.conversation_view.clone());
        self.set_base_view(thread.into(), true, window, cx);
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn insert_test_terminal(
        &mut self,
        title: impl Into<String>,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<TerminalId> {
        let terminal_id = TerminalId::new();
        self.set_last_created_entry_kind_from_user_action(AgentPanelEntryKind::Terminal, cx);
        self.insert_display_only_terminal(
            terminal_id,
            None,
            Some(SharedString::from(title.into())),
            None,
            None,
            focus,
            focus,
            true,
            AgentThreadSource::AgentPanel,
            window,
            cx,
        )?;
        Ok(terminal_id)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn restore_test_terminal(
        &mut self,
        metadata: TerminalThreadMetadata,
        focus: bool,
        source: AgentThreadSource,
        workspace: Option<&Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        if self.has_terminal(metadata.terminal_id) {
            self.activate_terminal(metadata.terminal_id, focus, window, cx);
            return Ok(());
        }

        if !self.supports_terminal(cx) {
            return Ok(());
        }

        let working_directory = self.terminal_restore_working_directory(&metadata, workspace, cx);
        let initial_title = Self::terminal_restore_initial_title(&metadata);
        self.insert_display_only_terminal(
            metadata.terminal_id,
            working_directory,
            metadata.custom_title.clone(),
            initial_title,
            Some(metadata.created_at),
            true,
            focus,
            Self::runs_init_command_on_restore(cx),
            source,
            window,
            cx,
        )
    }

    #[cfg(any(test, feature = "test-support"))]
    fn insert_display_only_terminal(
        &mut self,
        terminal_id: TerminalId,
        working_directory: Option<PathBuf>,
        custom_title: Option<SharedString>,
        initial_title: Option<SharedString>,
        created_at: Option<DateTime<Utc>>,
        select: bool,
        focus: bool,
        run_init_command: bool,
        source: AgentThreadSource,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        let init_command = Self::terminal_init_command(run_init_command, cx);
        let settings = TerminalSettings::get_global(cx).clone();
        let path_style = self.project.read(cx).path_style(cx);
        let builder = terminal::TerminalBuilder::new_display_only(
            settings.cursor_shape,
            settings.alternate_scroll,
            settings.max_scroll_history_lines,
            cx.entity_id().as_u64(),
            cx.background_executor(),
            path_style,
        );
        let terminal = cx.new(|cx| builder.subscribe(cx));
        let terminal_for_init_command = terminal.clone();
        let terminal_view = cx.new(|cx| {
            let mut view = TerminalView::new(
                terminal,
                self.workspace.clone(),
                self.workspace_id,
                self.project.downgrade(),
                window,
                cx,
            );
            view.set_show_workspace_actions(false, cx);
            view
        });
        self.insert_terminal(
            terminal_id,
            terminal_view,
            working_directory,
            custom_title,
            initial_title,
            created_at,
            select,
            focus,
            source,
            window,
            cx,
        );
        self.deliver_terminal_init_command(&terminal_for_init_command, init_command, window, cx);
        Ok(())
    }

    /// 表示専用の端末を 1 枚、このパネルの**ペインのタブ**として置く
    /// (ADR-F53 の置き場を、実 PTY を立てずに試すため)。
    #[cfg(any(test, feature = "test-support"))]
    pub fn add_test_terminal_tab(
        &mut self,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<TerminalView> {
        let settings = TerminalSettings::get_global(cx).clone();
        let path_style = self.project.read(cx).path_style(cx);
        let builder = terminal::TerminalBuilder::new_display_only(
            settings.cursor_shape,
            settings.alternate_scroll,
            settings.max_scroll_history_lines,
            cx.entity_id().as_u64(),
            cx.background_executor(),
            path_style,
        );
        let terminal = cx.new(|cx| builder.subscribe(cx));
        let terminal_view = cx.new(|cx| {
            TerminalView::new(
                terminal,
                self.workspace.clone(),
                self.workspace_id,
                self.project.downgrade(),
                window,
                cx,
            )
        });
        self.add_terminal_tab(terminal_view.clone(), focus, window, cx);
        terminal_view
    }

    /// `base_view` が「スレッド以外のタブ」を指しているか、そのアイテムの id。
    #[cfg(any(test, feature = "test-support"))]
    pub fn test_visible_pane_item_id(&self) -> Option<EntityId> {
        match &self.base_view {
            BaseView::PaneItem { item_id } => Some(*item_id),
            _ => None,
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn emit_test_terminal_bell(&mut self, terminal_id: TerminalId, cx: &mut Context<Self>) {
        let Some(terminal_entity) = self
            .terminals
            .get(&terminal_id)
            .map(|terminal| terminal.view.read(cx).terminal().clone())
        else {
            return;
        };
        terminal_entity.update(cx, |_terminal, cx| {
            cx.emit(TerminalEvent::Bell);
        });
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn emit_test_terminal_close(&mut self, terminal_id: TerminalId, cx: &mut Context<Self>) {
        let Some(terminal_entity) = self
            .terminals
            .get(&terminal_id)
            .map(|terminal| terminal.view.read(cx).terminal().clone())
        else {
            return;
        };
        terminal_entity.update(cx, |_terminal, cx| {
            cx.emit(TerminalEvent::CloseTerminal);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NewWorktreeBranchTarget;
    use crate::conversation_view::tests::{StubAgentServer, init_test};
    use crate::test_support::{
        active_session_id, active_thread_id, open_thread_with_connection,
        open_thread_with_custom_connection, register_test_sidebar, send_message,
    };
    use acp_thread::{AgentConnection, StubAgentConnection, ThreadStatus};
    use action_log::ActionLog;
    use anyhow::{Result, anyhow};
    use feature_flags::FeatureFlagAppExt;
    use fs::FakeFs;
    use gpui::{App, Modifiers, TestAppContext, UpdateGlobal, VisualTestContext, px, size};
    use parking_lot::Mutex;
    use project::{Project, WorktreePaths};
    use settings::{SettingsStore, WorkingDirectory};
    use std::any::Any;

    use serde_json::json;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::Instant;

    /// ADR-F56: a CLI tab that is not connected to Zed's IDE gets the CLI's own
    /// `@<path>#L<a>-<b>` references (`#L<a>` for one line), and a selection of
    /// whole lines does not count the line it ends on at column 0.
    #[gpui::test]
    async fn test_a_selection_becomes_the_clis_own_line_references(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            util::path!("/project"),
            json!({ "src": { "main.rs": "one\ntwo\nthree\nfour\n" } }),
        )
        .await;
        let project = Project::test(fs, [Path::new(util::path!("/project"))], cx).await;
        let buffer = project
            .update(cx, |project, cx| {
                project.open_local_buffer(util::path!("/project/src/main.rs"), cx)
            })
            .await
            .expect("the file opens");
        let range = |start: language::Point, end: language::Point, cx: &mut TestAppContext| {
            buffer.read_with(cx, |buffer, _| {
                buffer.anchor_before(start)..buffer.anchor_after(end)
            })
        };
        let whole_lines = range(language::Point::new(1, 0), language::Point::new(3, 0), cx);
        let within_a_line = range(language::Point::new(0, 1), language::Point::new(0, 2), cx);
        let selection = AgentContextSelection::Editor(vec![
            (buffer.clone(), whole_lines),
            (buffer.clone(), within_a_line),
        ]);

        let text = cx.update(|cx| {
            mention_selection_in_terminal(
                &selection,
                &project,
                Some(Path::new(util::path!("/project"))),
                None,
                true,
                cx,
            )
        });
        let path = if cfg!(windows) {
            "src\\main.rs"
        } else {
            "src/main.rs"
        };
        assert_eq!(text, format!("@{path}#L2-3 @{path}#L1 "));

        // A tab running another agent keeps the plain mention, and nothing goes
        // through the host's IDE server to whichever Claude Code is connected there.
        let other = cx.update(|cx| {
            mention_selection_in_terminal(
                &selection,
                &project,
                Some(Path::new(util::path!("/project"))),
                None,
                false,
                cx,
            )
        });
        assert_eq!(other, format!("{path}:2-4 {path}:1 "));

        assert_eq!(cli_line_range_suffix(0, 0), "#L1");
        assert_eq!(cli_line_range_suffix(4, 9), "#L5-10");
        let terminal_text = AgentContextSelection::Terminal(vec!["ls".to_string()]);
        assert_eq!(
            cx.update(|cx| {
                mention_selection_in_terminal(&terminal_text, &project, None, None, true, cx)
            }),
            "ls",
            "a terminal selection is pasted as it is"
        );
    }

    #[test]
    fn test_is_known_terminal_agent_command() {
        assert!(is_known_terminal_agent_command("claude"));
        assert!(is_known_terminal_agent_command("codex"));
        assert!(!is_known_terminal_agent_command("cargo"));
        assert!(!is_known_terminal_agent_command("internal-agent"));
    }

    #[test]
    fn test_terminal_program_reports_known_agent_transitions() {
        let mut last_observed_program = None;

        assert_eq!(
            terminal_program_to_report(&mut last_observed_program, Some("codex".to_string())),
            Some("codex".to_string())
        );
        assert_eq!(
            terminal_program_to_report(&mut last_observed_program, Some("codex".to_string())),
            None
        );
        assert_eq!(
            terminal_program_to_report(&mut last_observed_program, Some("zsh".to_string())),
            None
        );
        assert_eq!(
            terminal_program_to_report(
                &mut last_observed_program,
                Some("customer-data-export".to_string())
            ),
            None
        );
        assert_eq!(
            terminal_program_to_report(&mut last_observed_program, Some("codex".to_string())),
            Some("codex".to_string())
        );
        assert_eq!(
            terminal_program_to_report(&mut last_observed_program, None),
            None
        );
        assert_eq!(
            terminal_program_to_report(&mut last_observed_program, Some("codex".to_string())),
            Some("codex".to_string())
        );
    }

    #[derive(Clone, Default)]
    struct SessionTrackingConnection {
        next_session_number: Arc<Mutex<usize>>,
        sessions: Arc<Mutex<HashSet<acp::SessionId>>>,
    }

    impl SessionTrackingConnection {
        fn new() -> Self {
            Self::default()
        }

        fn create_session(
            self: Rc<Self>,
            session_id: acp::SessionId,
            project: Entity<Project>,
            work_dirs: PathList,
            title: Option<SharedString>,
            cx: &mut App,
        ) -> Entity<AcpThread> {
            self.sessions.lock().insert(session_id.clone());

            let action_log = cx.new(|_| ActionLog::new(project.clone()));
            cx.new(|cx| {
                AcpThread::new(
                    None,
                    title,
                    Some(work_dirs),
                    self,
                    project,
                    action_log,
                    session_id,
                    watch::Receiver::constant(
                        acp::PromptCapabilities::new()
                            .image(true)
                            .audio(true)
                            .embedded_context(true),
                    ),
                    cx,
                )
            })
        }
    }

    impl AgentConnection for SessionTrackingConnection {
        fn agent_id(&self) -> AgentId {
            agent::ZED_AGENT_ID.clone()
        }

        fn telemetry_id(&self) -> SharedString {
            "session-tracking-test".into()
        }

        fn new_session(
            self: Rc<Self>,
            project: Entity<Project>,
            work_dirs: PathList,
            cx: &mut App,
        ) -> Task<Result<Entity<AcpThread>>> {
            let session_id = {
                let mut next_session_number = self.next_session_number.lock();
                let session_id = acp::SessionId::new(format!(
                    "session-tracking-session-{}",
                    *next_session_number
                ));
                *next_session_number += 1;
                session_id
            };
            let thread = self.create_session(session_id, project, work_dirs, None, cx);
            Task::ready(Ok(thread))
        }

        fn supports_load_session(&self) -> bool {
            true
        }

        fn load_session(
            self: Rc<Self>,
            session_id: acp::SessionId,
            project: Entity<Project>,
            work_dirs: PathList,
            title: Option<SharedString>,
            cx: &mut App,
        ) -> Task<Result<Entity<AcpThread>>> {
            let thread = self.create_session(session_id, project, work_dirs, title, cx);
            thread.update(cx, |thread, cx| {
                thread
                    .handle_session_update(
                        acp::SessionUpdate::UserMessageChunk(acp::ContentChunk::new(
                            "Restored user message".into(),
                        )),
                        cx,
                    )
                    .expect("restored user message should be applied");
                thread
                    .handle_session_update(
                        acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                            "Restored assistant message".into(),
                        )),
                        cx,
                    )
                    .expect("restored assistant message should be applied");
            });
            Task::ready(Ok(thread))
        }

        fn supports_close_session(&self) -> bool {
            true
        }

        fn close_session(
            self: Rc<Self>,
            session_id: &acp::SessionId,
            _cx: &mut App,
        ) -> Task<Result<()>> {
            self.sessions.lock().remove(session_id);
            Task::ready(Ok(()))
        }

        fn auth_methods(&self) -> &[acp::AuthMethod] {
            &[]
        }

        fn authenticate(&self, _method_id: acp::AuthMethodId, _cx: &mut App) -> Task<Result<()>> {
            Task::ready(Ok(()))
        }

        fn prompt(
            &self,
            params: acp::PromptRequest,
            _cx: &mut App,
        ) -> Task<Result<acp::PromptResponse>> {
            if !self.sessions.lock().contains(&params.session_id) {
                return Task::ready(Err(anyhow!("Session not found")));
            }

            Task::ready(Ok(acp::PromptResponse::new(acp::StopReason::EndTurn)))
        }

        fn cancel(&self, _session_id: &acp::SessionId, _cx: &mut App) {}

        fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
            self
        }
    }

    #[gpui::test]
    async fn test_clicking_tool_call_output_keeps_agent_panel_focused_and_zoomed(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        cx.update(|cx| {
            agent::ThreadStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);
        });

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let multi_workspace =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace
            .read_with(cx, |multi_workspace, _cx| {
                multi_workspace.workspace().clone()
            })
            .unwrap();
        let cx = &mut VisualTestContext::from_window(multi_workspace.into(), cx);
        cx.simulate_resize(size(px(900.), px(700.)));

        let connection = StubAgentConnection::new();
        let panel = workspace.update_in(cx, |workspace, window, cx| {
            let panel = cx.new(|cx| AgentPanel::new(workspace, window, cx));
            workspace.add_panel(panel.clone(), window, cx);
            workspace.focus_panel::<AgentPanel>(window, cx);
            panel
        });
        open_thread_with_connection(&panel, connection.clone(), cx);

        let session_id = active_session_id(&panel, cx);
        let tool_call_id = acp::ToolCallId::new("tool-call-output-focus-regression");
        cx.update(|_window, cx| {
            connection.send_update(
                session_id.clone(),
                acp::SessionUpdate::ToolCall(
                    acp::ToolCall::new(tool_call_id.clone(), "Read file")
                        .kind(acp::ToolKind::Fetch)
                        .status(acp::ToolCallStatus::InProgress),
                ),
                cx,
            );
            connection.send_update(
                session_id,
                acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
                    tool_call_id.clone(),
                    acp::ToolCallUpdateFields::new()
                        .status(acp::ToolCallStatus::Completed)
                        .content(vec![acp::ToolCallContent::Content(acp::Content::new(
                            acp::ContentBlock::Text(acp::TextContent::new(
                                "tool output text".to_string(),
                            )),
                        ))]),
                )),
                cx,
            );
        });
        cx.run_until_parked();

        let thread_view = panel.read_with(cx, |panel, cx| panel.active_thread_view(cx).unwrap());
        thread_view.update(cx, |thread_view, cx| {
            thread_view.entry_view_state.update(cx, |state, _cx| {
                state.expand_tool_call(tool_call_id);
            });
            cx.notify();
        });

        panel.update_in(cx, |panel, window, cx| {
            panel.toggle_zoom(&ToggleZoom, window, cx);
        });

        // ADR-F36 Phase B: スレッドはパネルの中の `Pane` (キャッシュされた
        // ビュー) の下にあるので、キャッシュを使い回したフレームでは
        // `debug_selector` が記録されない。読む前に 1 回描き直させる。
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();

        // The thread receives only tool-call updates, so entry index 0 should remain stable.
        let output_bounds = cx
            .debug_bounds("tool-call-output-0-0")
            .expect("tool call output should be rendered");
        cx.simulate_click(output_bounds.center(), Modifiers::default());
        cx.run_until_parked();

        panel.update_in(cx, |panel, window, cx| {
            assert!(
                panel.focus_handle(cx).contains_focused(window, cx),
                "clicking tool call output should keep focus within the agent panel"
            );
            assert!(
                panel.is_zoomed(window, cx),
                "clicking tool call output should not close Zen Mode"
            );
        });

        let title_editor_focus_handle = panel.read_with(cx, |panel, cx| {
            panel
                .active_thread_view(cx)
                .expect("active thread view should be present")
                .read(cx)
                .title_editor
                .focus_handle(cx)
        });
        cx.update(|window, cx| {
            title_editor_focus_handle.focus(window, cx);
        });
        cx.run_until_parked();

        panel.update_in(cx, |panel, window, cx| {
            assert!(
                panel.focus_handle(cx).contains_focused(window, cx),
                "focusing the thread title editor should keep focus within the agent panel"
            );
            assert!(
                panel.is_zoomed(window, cx),
                "focusing the thread title editor should not close Zen Mode"
            );
        });
    }

    #[gpui::test]
    async fn test_resubmitting_edited_message_keeps_zoomed_agent_panel_open(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        cx.update(|cx| {
            agent::ThreadStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);
        });

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let multi_workspace =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace
            .read_with(cx, |multi_workspace, _cx| {
                multi_workspace.workspace().clone()
            })
            .unwrap();
        let cx = &mut VisualTestContext::from_window(multi_workspace.into(), cx);
        cx.simulate_resize(size(px(900.), px(700.)));

        let connection = StubAgentConnection::new();
        connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
            acp::ContentChunk::new("Response".into()),
        )]);
        let panel = workspace.update_in(cx, |workspace, window, cx| {
            let panel = cx.new(|cx| AgentPanel::new(workspace, window, cx));
            workspace.add_panel(panel.clone(), window, cx);
            workspace.focus_panel::<AgentPanel>(window, cx);
            panel
        });
        open_thread_with_connection(&panel, connection.clone(), cx);
        send_message(&panel, cx);

        panel.update_in(cx, |panel, window, cx| {
            panel.toggle_zoom(&ToggleZoom, window, cx);
        });
        cx.run_until_parked();

        let thread_view = panel.read_with(cx, |panel, cx| panel.active_thread_view(cx).unwrap());
        let user_message_editor = thread_view.read_with(cx, |thread_view, cx| {
            thread_view
                .entry_view_state
                .read(cx)
                .entry(0)
                .unwrap()
                .message_editor()
                .unwrap()
                .clone()
        });
        cx.focus(&user_message_editor);
        cx.run_until_parked();
        user_message_editor.update_in(cx, |editor, window, cx| {
            editor.set_text("Edited message", window, cx);
        });

        connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
            acp::ContentChunk::new("New response".into()),
        )]);
        user_message_editor.update_in(cx, |_editor, window, cx| {
            window.dispatch_action(Box::new(zed_actions::agent::Chat), cx);
        });
        cx.update(|window, _cx| window.blur());
        cx.run_until_parked();

        workspace.update_in(cx, |workspace, window, cx| {
            let position = panel.read(cx).position(window, cx);
            assert!(
                workspace.dock_at_position(position).read(cx).is_open(),
                "resubmitting an edited message should not hide the agent panel"
            );
            assert!(
                panel.read(cx).is_zoomed(window, cx),
                "resubmitting an edited message should not close Zen Mode"
            );
            assert!(
                panel.read(cx).focus_handle(cx).contains_focused(window, cx),
                "focus should stay within the agent panel after resubmitting"
            );
        });
    }

    #[gpui::test]
    async fn test_active_thread_serialize_and_load_round_trip(cx: &mut TestAppContext) {
        init_test(cx);
        cx.update(|cx| {
            agent::ThreadStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);
        });

        // Create a MultiWorkspace window with two workspaces.
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/project_a", json!({ "file.txt": "" }))
            .await;
        let project_a = Project::test(fs.clone(), [Path::new("/project_a")], cx).await;
        let project_b = Project::test(fs, [], cx).await;

        let multi_workspace =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));

        let workspace_a = multi_workspace
            .read_with(cx, |multi_workspace, _cx| {
                multi_workspace.workspace().clone()
            })
            .unwrap();

        let workspace_b = multi_workspace
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.test_add_workspace(project_b.clone(), window, cx)
            })
            .unwrap();

        workspace_a.update(cx, |workspace, _cx| {
            workspace.set_random_database_id();
        });
        workspace_b.update(cx, |workspace, _cx| {
            workspace.set_random_database_id();
        });

        let cx = &mut VisualTestContext::from_window(multi_workspace.into(), cx);

        // Set up workspace A: with an active thread.
        let panel_a = workspace_a.update_in(cx, |workspace, window, cx| {
            cx.new(|cx| AgentPanel::new(workspace, window, cx))
        });

        panel_a.update_in(cx, |panel, window, cx| {
            panel.open_external_thread_with_server(
                Rc::new(StubAgentServer::default_response()),
                window,
                cx,
            );
        });

        cx.run_until_parked();

        panel_a.read_with(cx, |panel, cx| {
            assert!(
                panel.active_agent_thread(cx).is_some(),
                "workspace A should have an active thread after connection"
            );
        });

        send_message(&panel_a, cx);

        let agent_type_a = panel_a.read_with(cx, |panel, _cx| panel.selected_agent.clone());

        // Set up workspace B: ClaudeCode, no active thread.
        let panel_b = workspace_b.update_in(cx, |workspace, window, cx| {
            cx.new(|cx| AgentPanel::new(workspace, window, cx))
        });

        panel_b.update(cx, |panel, _cx| {
            panel.selected_agent = Agent::Custom {
                id: "claude-acp".into(),
            };
        });

        // Serialize both panels.
        panel_a.update(cx, |panel, cx| panel.serialize(cx));
        panel_b.update(cx, |panel, cx| panel.serialize(cx));
        cx.run_until_parked();

        let workspace_a_id = workspace_a
            .read_with(cx, |workspace, _cx| workspace.database_id())
            .expect("workspace A should have a database id");
        let kvp = cx.update(|_window, cx| KeyValueStore::global(cx));
        let serialized_a: SerializedAgentPanel = cx
            .background_spawn(async move { read_serialized_panel(workspace_a_id, &kvp) })
            .await
            .expect("workspace A should serialize panel state");
        assert!(
            serialized_a.last_active_thread.is_some(),
            "active thread should be the thread restore target"
        );
        assert!(
            serialized_a.last_active_terminal_id.is_none(),
            "active thread serialization should not also include a terminal restore target"
        );

        cx.update(|_window, cx| {
            ThreadMetadataStore::init_global(cx);
        });

        // Load fresh panels for each workspace and verify independent state.
        let async_cx = cx.update(|window, cx| window.to_async(cx));
        let loaded_a = AgentPanel::load(workspace_a.downgrade(), async_cx)
            .await
            .expect("panel A load should succeed");
        cx.run_until_parked();

        let async_cx = cx.update(|window, cx| window.to_async(cx));
        let loaded_b = AgentPanel::load(workspace_b.downgrade(), async_cx)
            .await
            .expect("panel B load should succeed");
        cx.run_until_parked();

        // Workspace A should restore its thread and agent type
        loaded_a.read_with(cx, |panel, _cx| {
            assert_eq!(
                panel.selected_agent, agent_type_a,
                "workspace A agent type should be restored"
            );
            assert!(
                panel.active_conversation_view().is_some(),
                "workspace A should have its active thread restored"
            );
        });

        // Workspace B should restore its own agent type but have no active thread.
        loaded_b.read_with(cx, |panel, _cx| {
            assert_eq!(
                panel.selected_agent,
                Agent::Custom {
                    id: "claude-acp".into()
                },
                "workspace B agent type should be restored"
            );
            assert!(
                panel.active_conversation_view().is_none(),
                "workspace B should have no active thread when it had no prior conversation"
            );
        });
    }

    #[gpui::test]
    async fn test_active_terminal_serialize_and_load_round_trip(cx: &mut TestAppContext) {
        init_test(cx);
        cx.update(|cx| {
            agent::ThreadStore::init_global(cx);
            TerminalThreadMetadataStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/project", json!({ "file.txt": "" })).await;
        cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;

        let multi_workspace =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace
            .read_with(cx, |multi_workspace, _cx| {
                multi_workspace.workspace().clone()
            })
            .unwrap();
        workspace.update(cx, |workspace, _cx| {
            workspace.set_random_database_id();
        });

        let cx = &mut VisualTestContext::from_window(multi_workspace.into(), cx);
        let panel = workspace.update_in(cx, |workspace, window, cx| {
            cx.new(|cx| AgentPanel::new(workspace, window, cx))
        });

        panel.update_in(cx, |panel, window, cx| {
            panel.activate_new_thread(false, AgentThreadSource::AgentPanel, window, cx);
        });
        let terminal_id = panel
            .update_in(cx, |panel, window, cx| {
                panel.insert_test_terminal("Dev Server", true, window, cx)
            })
            .expect("test terminal should be inserted");
        panel.update(cx, |panel, cx| panel.serialize(cx));
        cx.run_until_parked();

        let workspace_id = workspace
            .read_with(cx, |workspace, _cx| workspace.database_id())
            .expect("workspace should have a database id");
        let kvp = cx.update(|_window, cx| KeyValueStore::global(cx));
        let serialized: SerializedAgentPanel = cx
            .background_spawn(async move { read_serialized_panel(workspace_id, &kvp) })
            .await
            .expect("workspace should serialize panel state");
        assert_eq!(
            serialized.last_active_terminal_id,
            Some(terminal_id.to_key_string())
        );
        assert!(
            serialized.last_active_thread.is_none(),
            "active terminal serialization should not also include a thread restore target"
        );

        cx.update(|_window, cx| {
            TerminalThreadMetadataStore::init_global(cx);
        });
        let async_cx = cx.update(|window, cx| window.to_async(cx));
        let loaded = AgentPanel::load(workspace.downgrade(), async_cx)
            .await
            .expect("panel load should succeed");
        for _ in 0..8 {
            cx.run_until_parked();
        }

        loaded.read_with(cx, |panel, cx| {
            assert_eq!(panel.active_terminal_id(), Some(terminal_id));
            assert!(
                panel.active_conversation_view().is_none(),
                "the restored terminal should remain active instead of falling back to a draft"
            );
            assert!(
                panel
                    .terminals(cx)
                    .into_iter()
                    .any(|terminal| terminal.id == terminal_id),
                "active terminal metadata should be restored into the loaded panel"
            );
        });
    }

    #[gpui::test]
    async fn test_terminal_restore_working_directory_does_not_read_leased_workspace(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        cx.update(|cx| {
            agent::ThreadStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);

            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings
                        .terminal
                        .get_or_insert_default()
                        .project
                        .working_directory = Some(WorkingDirectory::AlwaysHome);
                });
            });
        });

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        project.update(cx, |project, _cx| {
            project.mark_as_collab_for_testing();
        });
        project.read_with(cx, |project, _cx| {
            assert!(project.is_remote());
        });

        let multi_workspace =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace
            .read_with(cx, |multi_workspace, _cx| {
                multi_workspace.workspace().clone()
            })
            .expect("multi workspace should have an active workspace");
        let cx = &mut VisualTestContext::from_window(multi_workspace.into(), cx);
        let panel = workspace.update_in(cx, |workspace, window, cx| {
            cx.new(|cx| AgentPanel::new(workspace, window, cx))
        });

        assert_eq!(
            workspace.read_with(cx, |workspace, cx| {
                terminal_view::default_working_directory(workspace, cx)
            }),
            None
        );

        let metadata = TerminalThreadMetadata {
            terminal_id: TerminalId::new(),
            title: "Dev Server".into(),
            custom_title: None,
            created_at: Utc::now(),
            worktree_paths: project.read_with(cx, |project, cx| project.worktree_paths(cx)),
            remote_connection: None,
            working_directory: None,
        };
        assert_eq!(metadata.working_directory, None);

        let working_directory = workspace.update_in(cx, |workspace, _window, cx| {
            panel
                .read(cx)
                .terminal_restore_working_directory(&metadata, Some(workspace), cx)
        });

        assert_eq!(working_directory, None);
    }

    #[gpui::test]
    async fn test_pending_terminal_restore_prevents_initial_terminal_creation(
        cx: &mut TestAppContext,
    ) {
        let (panel, mut cx) = setup_panel(cx).await;

        panel.update_in(&mut cx, |panel, window, cx| {
            panel.last_created_entry_kind = AgentPanelEntryKind::Terminal;
            panel.pending_terminal_spawn = Some(TerminalId::new());
            panel.set_active(true, window, cx);
        });
        for _ in 0..4 {
            cx.run_until_parked();
        }

        panel.read_with(&cx, |panel, cx| {
            assert!(
                panel.terminals(cx).is_empty(),
                "activation while a terminal restore is pending should not create a second terminal"
            );
            assert!(
                panel.active_conversation_view().is_none(),
                "activation while a terminal restore is pending should not fall back to a draft"
            );
        });
    }

    #[gpui::test]
    async fn test_repeated_activation_only_creates_one_initial_terminal(cx: &mut TestAppContext) {
        let (panel, mut cx) = setup_panel(cx).await;

        panel.update_in(&mut cx, |panel, window, cx| {
            panel.last_created_entry_kind = AgentPanelEntryKind::Terminal;
            panel.set_active(true, window, cx);
            panel.set_active(true, window, cx);
        });
        for _ in 0..8 {
            cx.run_until_parked();
        }

        panel.read_with(&cx, |panel, cx| {
            assert_eq!(
                panel.terminals(cx).len(),
                1,
                "repeated activation should only enqueue one initial terminal"
            );
            assert!(
                panel.active_terminal_id().is_some(),
                "the single initial terminal should become active"
            );
        });
    }

    #[test]
    fn test_terminal_agent_started_by() {
        for (command, agent) in [
            ("claude", "claude"),
            (" claude --resume ", "claude"),
            ("codex exec", "codex"),
            ("/usr/local/bin/claude", "claude"),
            (r"C:\Users\u\AppData\Roaming\npm\claude.cmd", "claude"),
            ("CLAUDE.EXE --continue", "claude"),
            (r#""C:\Program Files\nodejs\gemini.cmd" --yolo"#, "gemini"),
            (r"& 'C:\tools\claude.ps1'", "claude"),
            ("call aider.bat", "aider"),
            (r#"start "" claude"#, "claude"),
            (r#"start "Agent" /b codex"#, "codex"),
            (r#"cmd /c "claude --resume""#, "claude"),
            ("cmd.exe /S /K claude", "claude"),
            (r#"pwsh -NoProfile -c "gemini""#, "gemini"),
            (r#"powershell.exe -Command "& aider""#, "aider"),
            ("env -i FOO=1 claude", "claude"),
            ("ANTHROPIC_MODEL=x claude", "claude"),
            ("echo hi\nclaude", "claude"),
            ("cd /work && codex", "codex"),
            ("git pull; claude", "claude"),
        ] {
            assert_eq!(terminal_agent_started_by(command), Some(agent), "{command}");
        }
        for command in [
            "",
            "cargo run",
            "echo claude",
            "claudette",
            "./script/claude-setup",
            "cmd /c echo claude",
            "start notepad",
            "pwsh -NoLogo",
            "env",
            "FOO=claude make",
            "git commit -m 'claude && codex'",
        ] {
            assert_eq!(terminal_agent_started_by(command), None, "{command}");
        }
    }

    /// ADR-F49: a restored terminal thread does not start an agent CLI on its
    /// own; in this shell it would run outside the sandbox.
    #[gpui::test]
    async fn test_restored_terminal_does_not_start_an_agent_cli(cx: &mut TestAppContext) {
        let (panel, mut cx) = setup_panel(cx).await;
        cx.update(|_, cx| {
            let mut settings = AgentSettings::get_global(cx).clone();
            settings.terminal_init_command = Some(" claude --resume ".to_string());
            AgentSettings::override_global(settings, cx);
        });
        let metadata = TerminalThreadMetadata {
            terminal_id: TerminalId::new(),
            title: "Restored Terminal".into(),
            custom_title: None,
            created_at: Utc::now(),
            worktree_paths: WorktreePaths::from_folder_paths(&PathList::new(&[PathBuf::from(
                "/project",
            )])),
            remote_connection: None,
            working_directory: None,
        };
        let terminal_id = metadata.terminal_id;
        panel
            .update_in(&mut cx, |panel, window, cx| {
                panel.restore_test_terminal(
                    metadata,
                    true,
                    AgentThreadSource::AgentPanel,
                    None,
                    window,
                    cx,
                )
            })
            .expect("test terminal should be restored");
        cx.run_until_parked();

        assert!(!cx.has_pending_prompt(), "a restore does not even ask");
        let terminal = panel.read_with(&cx, |panel, cx| {
            panel
                .terminals
                .get(&terminal_id)
                .expect("terminal should exist")
                .view
                .read(cx)
                .terminal()
                .clone()
        });
        let input_log = terminal.update(&mut cx, |terminal, _| terminal.take_input_log());
        assert!(input_log.is_empty(), "{input_log:?}");
    }

    /// ADR-F49: a new terminal thread asks, every time, before it types in an
    /// init command that starts an agent CLI.
    #[gpui::test]
    async fn test_new_terminal_asks_before_starting_an_agent_cli(cx: &mut TestAppContext) {
        let (panel, mut cx) = setup_panel(cx).await;
        cx.update(|_, cx| {
            let mut settings = AgentSettings::get_global(cx).clone();
            settings.terminal_init_command = Some("codex".to_string());
            AgentSettings::override_global(settings, cx);
        });

        for (answer, expected) in [
            ("キャンセル", Vec::<Vec<u8>>::new()),
            ("起動 (今回のみ)", vec![b"codex\r".to_vec()]),
        ] {
            let terminal_id = TerminalId::new();
            panel
                .update_in(&mut cx, |panel, window, cx| {
                    panel.insert_display_only_terminal(
                        terminal_id,
                        None,
                        None,
                        None,
                        None,
                        true,
                        true,
                        true,
                        AgentThreadSource::AgentPanel,
                        window,
                        cx,
                    )
                })
                .expect("the terminal opens");
            cx.run_until_parked();
            let (message, detail) = cx
                .pending_prompt()
                .unwrap_or_else(|| panic!("it asks before `{answer}`"));
            assert!(message.contains("`codex`"), "it names the CLI: {message}");
            assert!(detail.contains("この PC の `codex`"), "{detail}");
            cx.simulate_prompt_answer(answer);
            cx.run_until_parked();

            let terminal = panel.read_with(&cx, |panel, cx| {
                panel
                    .terminals
                    .get(&terminal_id)
                    .expect("terminal should exist")
                    .view
                    .read(cx)
                    .terminal()
                    .clone()
            });
            let input_log = terminal.update(&mut cx, |terminal, _| terminal.take_input_log());
            assert_eq!(input_log, expected, "after `{answer}`");
        }
    }

    #[gpui::test]
    async fn test_restored_terminal_runs_init_command_once(cx: &mut TestAppContext) {
        let (panel, mut cx) = setup_panel(cx).await;
        cx.update(|_, cx| {
            let mut settings = AgentSettings::get_global(cx).clone();
            settings.terminal_init_command = Some(" echo resumed ".to_string());
            AgentSettings::override_global(settings, cx);
        });

        let metadata = TerminalThreadMetadata {
            terminal_id: TerminalId::new(),
            title: "Restored Terminal".into(),
            custom_title: None,
            created_at: Utc::now(),
            worktree_paths: WorktreePaths::from_folder_paths(&PathList::new(&[PathBuf::from(
                "/project",
            )])),
            remote_connection: None,
            working_directory: None,
        };
        let terminal_id = metadata.terminal_id;
        panel
            .update_in(&mut cx, |panel, window, cx| {
                panel.restore_test_terminal(
                    metadata.clone(),
                    true,
                    AgentThreadSource::AgentPanel,
                    None,
                    window,
                    cx,
                )
            })
            .expect("test terminal should be restored");
        cx.run_until_parked();

        let terminal = panel.read_with(&cx, |panel, cx| {
            panel
                .terminals
                .get(&terminal_id)
                .expect("terminal should exist")
                .view
                .read(cx)
                .terminal()
                .clone()
        });
        let input_log = terminal.update(&mut cx, |terminal, _| terminal.take_input_log());
        assert_eq!(input_log, vec![b" echo resumed \r".to_vec()]);
        assert!(
            !terminal.read_with(&cx, |terminal, _| terminal.keyboard_input_sent()),
            "writing the init command must not mark the terminal as having received \
             user keyboard input, otherwise a shell that fails to spawn would be \
             auto-closed before the user can see the error"
        );

        panel
            .update_in(&mut cx, |panel, window, cx| {
                panel.restore_test_terminal(
                    metadata,
                    true,
                    AgentThreadSource::AgentPanel,
                    None,
                    window,
                    cx,
                )
            })
            .expect("restoring an existing test terminal should succeed");
        cx.run_until_parked();

        let input_log = terminal.update(&mut cx, |terminal, _| terminal.take_input_log());
        assert!(
            input_log.is_empty(),
            "activating an already-restored terminal should not re-run the init command, got {input_log:?}"
        );
    }

    /// Exercises the real `spawn_terminal` path with a genuine shell PTY (not the
    /// display-only test terminal, where `write_to_pty` is a no-op) to verify the
    /// init command is actually delivered to the shell and executed.
    #[cfg(unix)]
    #[gpui::test]
    async fn test_spawn_terminal_runs_init_command_in_real_shell(cx: &mut TestAppContext) {
        let (panel, mut cx) = setup_panel(cx).await;
        cx.executor().allow_parking();
        cx.update(|_, cx| {
            let mut settings = AgentSettings::get_global(cx).clone();
            // `init_ran_42` is the command's output, not its echoed text, so finding
            // it proves the shell executed the command rather than just echoing it.
            settings.terminal_init_command = Some("printf 'init_ran_%s\\n' 42".to_string());
            AgentSettings::override_global(settings, cx);

            // Force a known POSIX shell so the test doesn't depend on the developer's login shell.
            let mut terminal_settings = TerminalSettings::get_global(cx).clone();
            terminal_settings.shell = task::Shell::Program("/bin/sh".to_string());
            TerminalSettings::override_global(terminal_settings, cx);
        });

        let terminal_id = TerminalId::new();
        panel.update_in(&mut cx, |panel, window, cx| {
            panel.spawn_terminal(
                terminal_id,
                // No working directory: the FakeFs project path doesn't exist on
                // the real filesystem the shell process runs against.
                None,
                None,
                None,
                None,
                true,
                true,
                true,
                AgentThreadSource::AgentPanel,
                window,
                cx,
            );
        });

        // The shell spawns on a background thread and produces output
        // asynchronously, so poll (with a deadline) rather than using a fixed
        // sleep, matching the real-PTY test in `acp_thread`.
        let deadline = Instant::now() + Duration::from_secs(10);
        let terminal = loop {
            cx.run_until_parked();
            let terminal = panel.read_with(&cx, |panel, cx| {
                panel
                    .terminals
                    .get(&terminal_id)
                    .map(|terminal| terminal.view.read(cx).terminal().clone())
            });
            if let Some(terminal) = &terminal
                && terminal
                    .read_with(&cx, |terminal, _| terminal.get_content())
                    .contains("init_ran_42")
            {
                break terminal.clone();
            }
            if Instant::now() >= deadline {
                let terminal_created = terminal.is_some();
                let (content, input_log) = if let Some(terminal) = terminal {
                    let content = terminal.read_with(&cx, |terminal, _| terminal.get_content());
                    let input_log =
                        terminal.update(&mut cx, |terminal, _| terminal.take_input_log());
                    (content, input_log)
                } else {
                    (String::new(), Vec::new())
                };
                panic!(
                    "init command output never appeared in the terminal; terminal_created={terminal_created}, content={content:?}, input_log={input_log:?}"
                );
            }
            cx.executor().timer(Duration::from_millis(50)).await;
        };

        let input_log = terminal.update(&mut cx, |terminal, _| terminal.take_input_log());
        assert_eq!(
            input_log,
            vec![b"printf 'init_ran_%s\\n' 42\r".to_vec()],
            "init command should be written only after terminal startup has settled"
        );
        assert!(
            !terminal.read_with(&cx, |terminal, _| terminal.keyboard_input_sent()),
            "writing the init command must not mark the terminal as having received \
             user keyboard input"
        );
    }

    #[gpui::test]
    async fn test_restored_terminal_does_not_update_global_entry_kind(cx: &mut TestAppContext) {
        let (panel, mut cx) = setup_panel(cx).await;
        cx.update(|_, cx| {
            TerminalThreadMetadataStore::init_global(cx);
        });

        panel.update_in(&mut cx, |panel, window, cx| {
            panel.activate_new_thread(false, AgentThreadSource::AgentPanel, window, cx);
        });
        cx.run_until_parked();
        cx.update(|_, cx| {
            assert_eq!(
                read_global_last_created_entry_kind(&KeyValueStore::global(cx)),
                Some(AgentPanelEntryKind::Thread)
            );
        });

        let metadata = TerminalThreadMetadata {
            terminal_id: TerminalId::new(),
            title: "Restored Terminal".into(),
            custom_title: None,
            created_at: Utc::now(),
            worktree_paths: WorktreePaths::from_folder_paths(&PathList::new(&[PathBuf::from(
                "/project",
            )])),
            remote_connection: None,
            working_directory: None,
        };
        panel
            .update_in(&mut cx, |panel, window, cx| {
                panel.restore_test_terminal(
                    metadata,
                    true,
                    AgentThreadSource::AgentPanel,
                    None,
                    window,
                    cx,
                )
            })
            .expect("test terminal should be restored");
        cx.run_until_parked();

        cx.update(|_, cx| {
            assert_eq!(
                read_global_last_created_entry_kind(&KeyValueStore::global(cx)),
                Some(AgentPanelEntryKind::Thread),
                "restoring a terminal should not change the global new-entry default"
            );
        });
    }

    #[gpui::test]
    async fn test_new_workspace_load_uses_global_terminal_entry_kind(cx: &mut TestAppContext) {
        init_test(cx);
        cx.update(|cx| {
            agent::ThreadStore::init_global(cx);
            TerminalThreadMetadataStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/project-a", json!({ "file.txt": "" }))
            .await;
        fs.insert_tree("/project-b", json!({ "file.txt": "" }))
            .await;
        cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

        let project_a = Project::test(fs.clone(), [Path::new("/project-a")], cx).await;
        let project_b = Project::test(fs.clone(), [Path::new("/project-b")], cx).await;
        let multi_workspace =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));
        let multi_workspace_entity = multi_workspace.root(cx).unwrap();
        let workspace_a = multi_workspace
            .read_with(cx, |multi_workspace, _cx| {
                multi_workspace.workspace().clone()
            })
            .unwrap();
        workspace_a.update(cx, |workspace, _cx| {
            workspace.set_random_database_id();
        });

        let cx = &mut VisualTestContext::from_window(multi_workspace.into(), cx);
        let panel_a = workspace_a.update_in(cx, |workspace, window, cx| {
            cx.new(|cx| AgentPanel::new(workspace, window, cx))
        });
        panel_a
            .update_in(cx, |panel, window, cx| {
                panel.insert_test_terminal("Dev Server", true, window, cx)
            })
            .expect("test terminal should be inserted");
        cx.run_until_parked();

        cx.update(|_window, cx| {
            assert_eq!(
                read_global_last_created_entry_kind(&KeyValueStore::global(cx)),
                Some(AgentPanelEntryKind::Terminal)
            );
        });

        let workspace_b = multi_workspace_entity.update_in(cx, |multi_workspace, window, cx| {
            multi_workspace.test_add_workspace(project_b.clone(), window, cx)
        });
        workspace_b.update(cx, |workspace, _cx| {
            workspace.set_random_database_id();
        });

        let async_cx = cx.update(|window, cx| window.to_async(cx));
        let loaded = AgentPanel::load(workspace_b.downgrade(), async_cx)
            .await
            .expect("panel load should succeed");
        workspace_b.update_in(cx, |workspace, window, cx| {
            workspace.add_panel(loaded.clone(), window, cx);
        });
        loaded.update_in(cx, |panel, window, cx| {
            panel.set_active(true, window, cx);
        });
        for _ in 0..8 {
            cx.run_until_parked();
        }

        loaded.read_with(cx, |panel, cx| {
            assert!(
                panel.active_terminal_id().is_some(),
                "new workspace should initialize to a terminal when terminal was the globally last used entry kind"
            );
            assert!(
                panel.active_conversation_view().is_none(),
                "new workspace should not initialize to a draft when terminal is the global entry kind"
            );
            assert!(panel.should_create_terminal_for_new_entry(cx));
        });
    }

    #[gpui::test]
    async fn test_non_native_thread_without_metadata_is_not_restored(cx: &mut TestAppContext) {
        init_test(cx);
        cx.update(|cx| {
            agent::ThreadStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);
        });

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;

        let multi_workspace =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));

        let workspace = multi_workspace
            .read_with(cx, |multi_workspace, _cx| {
                multi_workspace.workspace().clone()
            })
            .unwrap();

        workspace.update(cx, |workspace, _cx| {
            workspace.set_random_database_id();
        });

        let cx = &mut VisualTestContext::from_window(multi_workspace.into(), cx);

        let panel = workspace.update_in(cx, |workspace, window, cx| {
            cx.new(|cx| AgentPanel::new(workspace, window, cx))
        });

        panel.update_in(cx, |panel, window, cx| {
            panel.open_external_thread_with_server(
                Rc::new(StubAgentServer::default_response()),
                window,
                cx,
            );
        });

        cx.run_until_parked();

        panel.read_with(cx, |panel, cx| {
            assert!(
                panel.active_agent_thread(cx).is_some(),
                "should have an active thread after connection"
            );
        });

        // Serialize without ever sending a message, so no thread metadata exists.
        panel.update(cx, |panel, cx| panel.serialize(cx));
        cx.run_until_parked();

        let async_cx = cx.update(|window, cx| window.to_async(cx));
        let loaded = AgentPanel::load(workspace.downgrade(), async_cx)
            .await
            .expect("panel load should succeed");
        cx.run_until_parked();

        loaded.read_with(cx, |panel, _cx| {
            assert!(
                panel.active_conversation_view().is_none(),
                "thread without metadata should not be restored; the panel should have no active thread"
            );
        });
    }

    #[gpui::test]
    async fn test_serialize_preserves_session_id_in_load_error(cx: &mut TestAppContext) {
        use crate::conversation_view::tests::FlakyAgentServer;
        use crate::thread_metadata_store::{ThreadId, ThreadMetadata};
        use chrono::Utc;
        use project::{AgentId as ProjectAgentId, WorktreePaths};

        init_test(cx);
        cx.update(|cx| {
            agent::ThreadStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);
        });

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;

        let multi_workspace =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace
            .read_with(cx, |mw, _cx| mw.workspace().clone())
            .unwrap();
        workspace.update(cx, |workspace, _cx| {
            workspace.set_random_database_id();
        });
        let workspace_id = workspace
            .read_with(cx, |workspace, _cx| workspace.database_id())
            .expect("workspace should have a database id");

        let cx = &mut VisualTestContext::from_window(multi_workspace.into(), cx);

        // Simulate a previous run that persisted metadata for this session.
        let resume_session_id = acp::SessionId::new("persistent-session");
        cx.update(|_window, cx| {
            ThreadMetadataStore::global(cx).update(cx, |store, cx| {
                store.save(
                    ThreadMetadata {
                        thread_id: ThreadId::new(),
                        session_id: Some(resume_session_id.clone()),
                        agent_id: ProjectAgentId::new("Flaky"),
                        title: Some("Persistent chat".into()),
                        title_override: None,
                        updated_at: Utc::now(),
                        created_at: Some(Utc::now()),
                        interacted_at: None,
                        worktree_paths: WorktreePaths::from_folder_paths(&PathList::default()),
                        remote_connection: None,
                        archived: false,
                    },
                    cx,
                );
            });
        });

        let panel = workspace.update_in(cx, |workspace, window, cx| {
            cx.new(|cx| AgentPanel::new(workspace, window, cx))
        });

        // Open a restored thread using a flaky server so the initial connect
        // fails and the view lands in LoadError — mirroring the cold-start
        // race against a custom agent over SSH.
        let (server, _fail) =
            FlakyAgentServer::new(StubAgentConnection::new().with_supports_load_session(true));
        panel.update_in(cx, |panel, window, cx| {
            panel.open_restored_thread_with_server(
                Rc::new(server),
                resume_session_id.clone(),
                window,
                cx,
            );
        });
        cx.run_until_parked();

        // Sanity: the view couldn't connect, so no live AcpThread exists.
        panel.read_with(cx, |panel, cx| {
            assert!(
                panel.active_agent_thread(cx).is_none(),
                "active_agent_thread should be None while the flaky server is failing"
            );
            let conversation_view = panel
                .active_conversation_view()
                .expect("panel should still have an active ConversationView");
            assert_eq!(
                conversation_view.read(cx).root_session_id.as_ref(),
                Some(&resume_session_id),
                "ConversationView should still hold the restored session id"
            );
        });

        // Serialize while in LoadError. Before the fix this wrote
        // `session_id=None` to the KVP and permanently lost the session.
        panel.update(cx, |panel, cx| panel.serialize(cx));
        cx.run_until_parked();

        let kvp = cx.update(|_window, cx| KeyValueStore::global(cx));
        let serialized: Option<SerializedAgentPanel> = cx
            .background_spawn(async move { read_serialized_panel(workspace_id, &kvp) })
            .await;
        let serialized_session_id = serialized
            .as_ref()
            .and_then(|p| p.last_active_thread.as_ref())
            .and_then(|t| t.session_id.clone());
        assert_eq!(
            serialized_session_id,
            Some(resume_session_id.0.to_string()),
            "serialize() must preserve the restored session id even while the \
             ConversationView is in LoadError; otherwise the bug survives a \
             restart because the KVP has been wiped"
        );
    }

    /// Extracts the text from a Text content block, panicking if it's not Text.
    fn expect_text_block(block: &acp::ContentBlock) -> &str {
        match block {
            acp::ContentBlock::Text(t) => t.text.as_str(),
            other => panic!("expected Text block, got {:?}", other),
        }
    }

    /// Extracts the (text_content, uri) from a Resource content block, panicking
    /// if it's not a TextResourceContents resource.
    fn expect_resource_block(block: &acp::ContentBlock) -> (&str, &str) {
        match block {
            acp::ContentBlock::Resource(r) => match &r.resource {
                acp::EmbeddedResourceResource::TextResourceContents(t) => {
                    (t.text.as_str(), t.uri.as_str())
                }
                other => panic!("expected TextResourceContents, got {:?}", other),
            },
            other => panic!("expected Resource block, got {:?}", other),
        }
    }

    #[gpui::test]
    async fn test_draft_prompt_blocks_use_current_editor_snapshot(cx: &mut TestAppContext) {
        init_test(cx);
        cx.update(|cx| {
            agent::ThreadStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/project", json!({ "file.txt": "" })).await;
        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;

        let multi_workspace =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace
            .read_with(cx, |multi_workspace, _cx| {
                multi_workspace.workspace().clone()
            })
            .unwrap();
        let cx = &mut VisualTestContext::from_window(multi_workspace.into(), cx);
        let panel = workspace.update_in(cx, |workspace, window, cx| {
            let panel = cx.new(|cx| AgentPanel::new(workspace, window, cx));
            workspace.add_panel(panel.clone(), window, cx);
            panel
        });

        let _stub_connection =
            crate::test_support::set_stub_agent_connection(StubAgentConnection::new());
        panel.update_in(cx, |panel, window, cx| {
            panel.selected_agent = Agent::Stub;
            panel.activate_draft(true, AgentThreadSource::AgentPanel, window, cx);
        });
        cx.run_until_parked();

        let thread_id = active_thread_id(&panel, cx);
        let thread = panel.read_with(cx, |panel, cx| {
            panel
                .active_agent_thread(cx)
                .expect("draft thread should be active")
        });
        let message_editor = panel.read_with(cx, |panel, cx| {
            panel
                .active_thread_view(cx)
                .expect("draft thread view should be active")
                .read(cx)
                .message_editor
                .clone()
        });

        thread.update(cx, |thread, cx| {
            thread.set_draft_prompt(
                Some(vec![acp::ContentBlock::Text(acp::TextContent::new(
                    "stale prompt",
                ))]),
                cx,
            );
        });
        message_editor.update_in(cx, |editor, window, cx| {
            editor.set_text("fresh prompt", window, cx);
        });
        let blocks = panel.read_with(cx, |panel, cx| {
            panel
                .draft_prompt_blocks_if_in_memory(thread_id, cx)
                .expect("draft should be in memory")
        });
        assert_eq!(blocks.len(), 1);
        assert_eq!(expect_text_block(&blocks[0]), "fresh prompt");

        thread.update(cx, |thread, cx| {
            thread.set_draft_prompt(
                Some(vec![acp::ContentBlock::Text(acp::TextContent::new(
                    "stale prompt after clear",
                ))]),
                cx,
            );
        });
        message_editor.update_in(cx, |editor, window, cx| {
            editor.set_text("", window, cx);
        });
        let blocks = panel.read_with(cx, |panel, cx| {
            panel
                .draft_prompt_blocks_if_in_memory(thread_id, cx)
                .expect("draft should be in memory")
        });
        assert!(
            blocks.is_empty(),
            "cleared editor snapshot should override stale saved draft prompt"
        );
    }

    #[gpui::test]
    async fn test_draft_has_user_content_checks_all_live_copies(cx: &mut TestAppContext) {
        init_test(cx);
        cx.update(|cx| {
            agent::ThreadStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/project_a", json!({ "file.txt": "" }))
            .await;
        fs.insert_tree("/project_b", json!({ "file.txt": "" }))
            .await;
        let project_a = Project::test(fs.clone(), [Path::new("/project_a")], cx).await;
        let project_b = Project::test(fs.clone(), [Path::new("/project_b")], cx).await;

        let multi_workspace =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));
        let workspace_a = multi_workspace
            .read_with(cx, |multi_workspace, _cx| {
                multi_workspace.workspace().clone()
            })
            .unwrap();
        let workspace_b = multi_workspace
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.test_add_workspace(project_b.clone(), window, cx)
            })
            .unwrap();

        let cx = &mut VisualTestContext::from_window(multi_workspace.into(), cx);
        let panel_a = workspace_a.update_in(cx, |workspace, window, cx| {
            let panel = cx.new(|cx| AgentPanel::new(workspace, window, cx));
            workspace.add_panel(panel.clone(), window, cx);
            panel
        });
        let panel_b = workspace_b.update_in(cx, |workspace, window, cx| {
            let panel = cx.new(|cx| AgentPanel::new(workspace, window, cx));
            workspace.add_panel(panel.clone(), window, cx);
            panel
        });

        let _stub_connection =
            crate::test_support::set_stub_agent_connection(StubAgentConnection::new());
        panel_a.update_in(cx, |panel, window, cx| {
            panel.selected_agent = Agent::Stub;
            panel.activate_draft(true, AgentThreadSource::AgentPanel, window, cx);
        });
        cx.run_until_parked();
        let thread_id = active_thread_id(&panel_a, cx);

        panel_b.update_in(cx, |panel, window, cx| {
            panel.load_agent_thread(
                Agent::Stub,
                thread_id,
                Some(PathList::new(&[PathBuf::from("/project_b")])),
                None,
                false,
                AgentThreadSource::AgentPanel,
                window,
                cx,
            );
        });
        cx.run_until_parked();

        crate::test_support::type_draft_prompt(&panel_b, "content in second panel", cx);
        let panel_a_blocks = panel_a.read_with(cx, |panel, cx| {
            panel
                .draft_prompt_blocks_if_in_memory(thread_id, cx)
                .expect("draft should be live in first panel")
        });
        assert!(
            panel_a_blocks.is_empty(),
            "first live draft copy should be empty"
        );

        let has_user_content = cx.update(|_, cx| {
            crate::draft_prompt_store::draft_has_user_content(
                thread_id,
                [&workspace_a, &workspace_b],
                cx,
            )
        });
        assert!(
            has_user_content,
            "a later live draft copy with content should keep the draft"
        );
    }

    #[test]
    fn test_build_conflict_resolution_prompt_single_conflict() {
        let conflicts = vec![ConflictContent {
            file_path: "src/main.rs".to_string(),
            conflict_text: "<<<<<<< HEAD\nlet x = 1;\n=======\nlet x = 2;\n>>>>>>> feature"
                .to_string(),
            ours_branch_name: "HEAD".to_string(),
            theirs_branch_name: "feature".to_string(),
        }];

        let blocks = build_conflict_resolution_prompt(&conflicts);
        // 2 Text blocks + 1 ResourceLink + 1 Resource for the conflict
        assert_eq!(
            blocks.len(),
            4,
            "expected 2 text + 1 resource link + 1 resource block"
        );

        let intro_text = expect_text_block(&blocks[0]);
        assert!(
            intro_text.contains("Please resolve the following merge conflict in"),
            "prompt should include single-conflict intro text"
        );

        match &blocks[1] {
            acp::ContentBlock::ResourceLink(link) => {
                assert!(
                    link.uri.contains("file://"),
                    "resource link URI should use file scheme"
                );
                assert!(
                    link.uri.contains("main.rs"),
                    "resource link URI should reference file path"
                );
            }
            other => panic!("expected ResourceLink block, got {:?}", other),
        }

        let body_text = expect_text_block(&blocks[2]);
        assert!(
            body_text.contains("`HEAD` (ours)"),
            "prompt should mention ours branch"
        );
        assert!(
            body_text.contains("`feature` (theirs)"),
            "prompt should mention theirs branch"
        );
        assert!(
            body_text.contains("editing the file directly"),
            "prompt should instruct the agent to edit the file"
        );

        let (resource_text, resource_uri) = expect_resource_block(&blocks[3]);
        assert!(
            resource_text.contains("<<<<<<< HEAD"),
            "resource should contain the conflict text"
        );
        assert!(
            resource_uri.contains("merge-conflict"),
            "resource URI should use the merge-conflict scheme"
        );
        assert!(
            resource_uri.contains("main.rs"),
            "resource URI should reference the file path"
        );
    }

    #[test]
    fn test_build_conflict_resolution_prompt_multiple_conflicts_same_file() {
        let conflicts = vec![
            ConflictContent {
                file_path: "src/lib.rs".to_string(),
                conflict_text: "<<<<<<< main\nfn a() {}\n=======\nfn a_v2() {}\n>>>>>>> dev"
                    .to_string(),
                ours_branch_name: "main".to_string(),
                theirs_branch_name: "dev".to_string(),
            },
            ConflictContent {
                file_path: "src/lib.rs".to_string(),
                conflict_text: "<<<<<<< main\nfn b() {}\n=======\nfn b_v2() {}\n>>>>>>> dev"
                    .to_string(),
                ours_branch_name: "main".to_string(),
                theirs_branch_name: "dev".to_string(),
            },
        ];

        let blocks = build_conflict_resolution_prompt(&conflicts);
        // 1 Text instruction + 2 Resource blocks
        assert_eq!(blocks.len(), 3, "expected 1 text + 2 resource blocks");

        let text = expect_text_block(&blocks[0]);
        assert!(
            text.contains("all 2 merge conflicts"),
            "prompt should mention the total count"
        );
        assert!(
            text.contains("`main` (ours)"),
            "prompt should mention ours branch"
        );
        assert!(
            text.contains("`dev` (theirs)"),
            "prompt should mention theirs branch"
        );
        // Single file, so "file" not "files"
        assert!(
            text.contains("file directly"),
            "single file should use singular 'file'"
        );

        let (resource_a, _) = expect_resource_block(&blocks[1]);
        let (resource_b, _) = expect_resource_block(&blocks[2]);
        assert!(
            resource_a.contains("fn a()"),
            "first resource should contain first conflict"
        );
        assert!(
            resource_b.contains("fn b()"),
            "second resource should contain second conflict"
        );
    }

    #[test]
    fn test_build_conflict_resolution_prompt_multiple_conflicts_different_files() {
        let conflicts = vec![
            ConflictContent {
                file_path: "src/a.rs".to_string(),
                conflict_text: "<<<<<<< main\nA\n=======\nB\n>>>>>>> dev".to_string(),
                ours_branch_name: "main".to_string(),
                theirs_branch_name: "dev".to_string(),
            },
            ConflictContent {
                file_path: "src/b.rs".to_string(),
                conflict_text: "<<<<<<< main\nC\n=======\nD\n>>>>>>> dev".to_string(),
                ours_branch_name: "main".to_string(),
                theirs_branch_name: "dev".to_string(),
            },
        ];

        let blocks = build_conflict_resolution_prompt(&conflicts);
        // 1 Text instruction + 2 Resource blocks
        assert_eq!(blocks.len(), 3, "expected 1 text + 2 resource blocks");

        let text = expect_text_block(&blocks[0]);
        assert!(
            text.contains("files directly"),
            "multiple files should use plural 'files'"
        );

        let (_, uri_a) = expect_resource_block(&blocks[1]);
        let (_, uri_b) = expect_resource_block(&blocks[2]);
        assert!(
            uri_a.contains("a.rs"),
            "first resource URI should reference a.rs"
        );
        assert!(
            uri_b.contains("b.rs"),
            "second resource URI should reference b.rs"
        );
    }

    #[test]
    fn test_build_conflicted_files_resolution_prompt_file_paths_only() {
        let file_paths = vec![
            "src/main.rs".to_string(),
            "src/lib.rs".to_string(),
            "tests/integration.rs".to_string(),
        ];

        let blocks = build_conflicted_files_resolution_prompt(&file_paths);
        // 1 instruction Text block + (ResourceLink + newline Text) per file
        assert_eq!(
            blocks.len(),
            1 + (file_paths.len() * 2),
            "expected instruction text plus resource links and separators"
        );

        let text = expect_text_block(&blocks[0]);
        assert!(
            text.contains("unresolved merge conflicts"),
            "prompt should describe the task"
        );
        assert!(
            text.contains("conflict markers"),
            "prompt should mention conflict markers"
        );

        for (index, path) in file_paths.iter().enumerate() {
            let link_index = 1 + (index * 2);
            let newline_index = link_index + 1;

            match &blocks[link_index] {
                acp::ContentBlock::ResourceLink(link) => {
                    assert!(
                        link.uri.contains("file://"),
                        "resource link URI should use file scheme"
                    );
                    assert!(
                        link.uri.contains(path),
                        "resource link URI should reference file path: {path}"
                    );
                }
                other => panic!(
                    "expected ResourceLink block at index {}, got {:?}",
                    link_index, other
                ),
            }

            let separator = expect_text_block(&blocks[newline_index]);
            assert_eq!(
                separator, "\n",
                "expected newline separator after each file"
            );
        }
    }

    #[test]
    fn test_build_conflict_resolution_prompt_empty_conflicts() {
        let blocks = build_conflict_resolution_prompt(&[]);
        assert!(
            blocks.is_empty(),
            "empty conflicts should produce no blocks, got {} blocks",
            blocks.len()
        );
    }

    #[test]
    fn test_build_conflicted_files_resolution_prompt_empty_paths() {
        let blocks = build_conflicted_files_resolution_prompt(&[]);
        assert!(
            blocks.is_empty(),
            "empty paths should produce no blocks, got {} blocks",
            blocks.len()
        );
    }

    #[test]
    fn test_conflict_resource_block_structure() {
        let conflict = ConflictContent {
            file_path: "src/utils.rs".to_string(),
            conflict_text: "<<<<<<< HEAD\nold code\n=======\nnew code\n>>>>>>> branch".to_string(),
            ours_branch_name: "HEAD".to_string(),
            theirs_branch_name: "branch".to_string(),
        };

        let block = conflict_resource_block(&conflict);
        let (text, uri) = expect_resource_block(&block);

        assert_eq!(
            text, conflict.conflict_text,
            "resource text should be the raw conflict"
        );
        assert!(
            uri.starts_with("zed:///agent/merge-conflict"),
            "URI should use the zed merge-conflict scheme, got: {uri}"
        );
        assert!(uri.contains("utils.rs"), "URI should encode the file path");
    }

    fn open_generating_thread_with_loadable_connection(
        panel: &Entity<AgentPanel>,
        connection: &StubAgentConnection,
        cx: &mut VisualTestContext,
    ) -> (acp::SessionId, ThreadId) {
        let previous = panel.read_with(cx, |panel, cx| panel.active_thread_id(cx));
        open_thread_with_custom_connection(panel, connection.clone(), cx);
        // ADR-F36 Phase B: 別のスレッドを開いてもタブは残るので、「前のスレッド
        // から離れた」を作るにはそのタブを閉じる。
        if let Some(previous) = previous {
            close_panel_tab(panel, previous, cx);
        }
        let session_id = active_session_id(panel, cx);
        let thread_id = active_thread_id(panel, cx);
        send_message(panel, cx);
        cx.update(|_, cx| {
            connection.send_update(
                session_id.clone(),
                acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new("done".into())),
                cx,
            );
        });
        cx.run_until_parked();
        (session_id, thread_id)
    }

    fn open_idle_thread_with_non_loadable_connection(
        panel: &Entity<AgentPanel>,
        connection: &StubAgentConnection,
        cx: &mut VisualTestContext,
    ) -> (acp::SessionId, ThreadId) {
        let previous = panel.read_with(cx, |panel, cx| panel.active_thread_id(cx));
        open_thread_with_custom_connection(panel, connection.clone(), cx);
        if let Some(previous) = previous {
            close_panel_tab(panel, previous, cx);
        }
        let session_id = active_session_id(panel, cx);
        let thread_id = active_thread_id(panel, cx);

        connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
            acp::ContentChunk::new("done".into()),
        )]);
        send_message(panel, cx);

        (session_id, thread_id)
    }

    /// パネルのタブを閉じて、そのスレッドを保持枠 (`retained_threads`) へ
    /// 追い出す。ADR-F36 Phase B からは「別のスレッドを開く」だけではタブが
    /// 残るので、保持枠の挙動を試すテストは明示的に閉じる。
    fn close_panel_tab(
        panel: &Entity<AgentPanel>,
        thread_id: ThreadId,
        cx: &mut VisualTestContext,
    ) {
        panel.update_in(cx, |panel, window, cx| {
            panel.test_close_thread_tab(thread_id, window, cx);
        });
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn test_draft_promotion_creates_metadata_and_new_session_on_reload(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        cx.update(|cx| {
            agent::ThreadStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/project", json!({ "file.txt": "" })).await;
        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;

        let multi_workspace =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));

        let workspace = multi_workspace
            .read_with(cx, |mw, _cx| mw.workspace().clone())
            .unwrap();

        workspace.update(cx, |workspace, _cx| {
            workspace.set_random_database_id();
        });

        let cx = &mut VisualTestContext::from_window(multi_workspace.into(), cx);

        let panel = workspace.update_in(cx, |workspace, window, cx| {
            let panel = cx.new(|cx| AgentPanel::new(workspace, window, cx));
            workspace.add_panel(panel.clone(), window, cx);
            panel
        });

        // Register a shared stub connection and use Agent::Stub so the draft
        // (and any reloaded draft) uses it.
        let stub_connection =
            crate::test_support::set_stub_agent_connection(StubAgentConnection::new());
        stub_connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
            acp::ContentChunk::new("Response".into()),
        )]);
        panel.update_in(cx, |panel, window, cx| {
            panel.selected_agent = Agent::Stub;
            panel.activate_draft(true, AgentThreadSource::AgentPanel, window, cx);
        });
        cx.run_until_parked();

        // Verify the thread is considered a draft.
        panel.read_with(cx, |panel, cx| {
            assert!(
                panel.active_thread_is_draft(cx),
                "thread should be a draft before any message is sent"
            );
            assert!(
                panel.draft_thread.is_some(),
                "draft_thread field should be set"
            );
        });
        let draft_session_id = active_session_id(&panel, cx);
        let thread_id = active_thread_id(&panel, cx);

        // A draft thread is persisted with session_id: None.
        cx.update(|_window, cx| {
            let store = ThreadMetadataStore::global(cx).read(cx);
            let entry = store
                .entry(thread_id)
                .expect("draft thread should have a metadata row");
            assert!(
                entry.is_draft(),
                "draft thread metadata should have session_id=None, got {:?}",
                entry.session_id,
            );
        });

        // Type into the message editor; the editor observer pushes the text
        // into `AcpThread.draft_prompt`, which emits `PromptUpdated` and
        // persists the prompt to the kvp store.
        crate::test_support::type_draft_prompt(&panel, "Hello from draft", cx);
        panel.update(cx, |panel, cx| panel.serialize(cx));
        cx.executor().advance_clock(Duration::from_millis(200));
        cx.run_until_parked();

        // 再起動なら古いパネルは居ない。ADR-F36 Phase B ではスレッドは
        // パネルのタブが持つので、手放させて、ワークスペースからも外してから
        // 読み直す。
        panel.update_in(cx, |panel, window, cx| {
            panel.test_close_all_thread_tabs(window, cx);
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.remove_panel::<AgentPanel>(&panel, window, cx);
        });
        cx.run_until_parked();

        let async_cx = cx.update(|window, cx| window.to_async(cx));
        let reloaded_panel = AgentPanel::load(workspace.downgrade(), async_cx)
            .await
            .expect("panel load with draft should succeed");
        cx.run_until_parked();

        reloaded_panel.read_with(cx, |panel, cx| {
            assert!(
                panel.active_thread_is_draft(cx),
                "reloaded panel should still show the draft as active"
            );
            assert!(
                panel.active_view_is_new_draft(cx),
                "reloaded draft should still occupy the new-draft slot: \
                 what's in the new-draft slot stays there across restarts, \
                 regardless of whether it's also the active view"
            );
            let active_entity = panel.active_conversation_view().map(|v| v.entity_id());
            let draft_entity = panel.draft_thread.as_ref().map(|v| v.entity_id());
            assert!(
                active_entity.is_some() && active_entity == draft_entity,
                "active view and draft slot should share a single ConversationView entity \
                 (active={active_entity:?}, draft={draft_entity:?})"
            );
        });

        // Thread identity is stable across reload — the metadata row we wrote
        // pre-reload maps back to the same ConversationView.
        let reloaded_thread_id = active_thread_id(&reloaded_panel, cx);
        assert_eq!(
            reloaded_thread_id, thread_id,
            "reloaded draft should preserve its ThreadId"
        );

        // ACP session_id is NOT preserved: drafts don't persist a session id,
        // so the reloaded ConversationView opens a fresh ACP session.
        let reloaded_session_id = active_session_id(&reloaded_panel, cx);
        assert_ne!(
            reloaded_session_id, draft_session_id,
            "reloaded draft should have a fresh ACP session ID"
        );

        let restored_text =
            reloaded_panel.read_with(cx, |panel, cx| panel.editor_text(reloaded_thread_id, cx));
        assert_eq!(
            restored_text.as_deref(),
            Some("Hello from draft"),
            "draft prompt text should be restored from the draft-prompt kvp store"
        );

        // Send a message on the reloaded panel — this promotes the draft to a
        // real thread. `ThreadId` stays the same; `session_id` is populated.
        let panel = reloaded_panel;
        let promoted_session_id = reloaded_session_id;
        send_message(&panel, cx);

        panel.read_with(cx, |panel, cx| {
            assert!(
                !panel.active_thread_is_draft(cx),
                "thread should no longer be a draft after sending a message"
            );
            assert!(
                panel.draft_thread.is_none(),
                "draft_thread should be None after promotion"
            );
            assert_eq!(
                panel.active_thread_id(cx),
                Some(thread_id),
                "same ThreadId should remain active after promotion"
            );
        });

        cx.update(|_window, cx| {
            let store = ThreadMetadataStore::global(cx).read(cx);
            let metadata = store
                .entry(thread_id)
                .expect("promoted thread should have metadata");
            assert!(
                !metadata.is_draft(),
                "promoted thread metadata should no longer be a draft"
            );
            assert_eq!(
                metadata.session_id.as_ref(),
                Some(&promoted_session_id),
                "metadata session_id should match the thread's ACP session"
            );
        });

        // Serialize the panel, then reload it again.
        panel.update(cx, |panel, cx| panel.serialize(cx));
        cx.executor().advance_clock(Duration::from_millis(200));
        cx.run_until_parked();

        panel.update_in(cx, |panel, window, cx| {
            panel.test_close_all_thread_tabs(window, cx);
        });
        cx.run_until_parked();

        let async_cx = cx.update(|window, cx| window.to_async(cx));
        let loaded_panel = AgentPanel::load(workspace.downgrade(), async_cx)
            .await
            .expect("panel load should succeed");
        cx.run_until_parked();

        // The second load should restore the promoted real thread, keyed by
        // its session_id.
        loaded_panel.read_with(cx, |panel, cx| {
            let active_id = panel.active_thread_id(cx);
            assert_eq!(
                active_id,
                Some(thread_id),
                "loaded panel should restore the promoted thread"
            );
            assert!(
                !panel.active_thread_is_draft(cx),
                "restored thread should not be a draft"
            );
        });
    }

    #[gpui::test]
    async fn test_new_draft_survives_reload_when_real_thread_is_active(cx: &mut TestAppContext) {
        init_test(cx);
        cx.update(|cx| {
            agent::ThreadStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/project", json!({ "file.txt": "" })).await;
        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;

        let multi_workspace =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace
            .read_with(cx, |mw, _cx| mw.workspace().clone())
            .unwrap();
        workspace.update(cx, |workspace, _cx| workspace.set_random_database_id());

        let cx = &mut VisualTestContext::from_window(multi_workspace.into(), cx);
        let panel = workspace.update_in(cx, |workspace, window, cx| {
            let panel = cx.new(|cx| AgentPanel::new(workspace, window, cx));
            workspace.add_panel(panel.clone(), window, cx);
            panel
        });

        // Register a shared stub connection under `Agent::Stub` so every
        // ConversationView the panel creates in this test (including any
        // post-reload rehydrations) reaches Connected synchronously.
        let stub_connection =
            crate::test_support::set_stub_agent_connection(StubAgentConnection::new());
        stub_connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
            acp::ContentChunk::new("ok".into()),
        )]);

        // 1. Create a real thread by sending a message.
        panel.update_in(cx, |panel, window, cx| {
            panel.selected_agent = Agent::Stub;
            panel.activate_draft(true, AgentThreadSource::AgentPanel, window, cx);
        });
        cx.run_until_parked();
        crate::test_support::send_message(&panel, cx);
        let real_thread_id = crate::test_support::active_thread_id(&panel, cx);
        let real_session_id = crate::test_support::active_session_id(&panel, cx);
        cx.run_until_parked();

        // 2. Open a draft, type into it, then press Cmd-N again to
        //    park it into retained_threads as a *retained* draft.
        panel.update_in(cx, |panel, window, cx| {
            panel.activate_draft(true, AgentThreadSource::AgentPanel, window, cx);
        });
        cx.run_until_parked();
        let retained_draft_id = crate::test_support::active_thread_id(&panel, cx);
        crate::test_support::type_draft_prompt(&panel, "retained draft text", cx);

        panel.update_in(cx, |panel, window, cx| {
            panel.new_thread(&NewThread, window, cx);
        });
        cx.run_until_parked();

        // The pre-existing draft is now in retained_threads (parked),
        // and a fresh empty ephemeral new-draft is active.
        panel.read_with(cx, |panel, cx| {
            assert!(
                panel.test_thread_is_in_background(retained_draft_id),
                "first draft with content should be parked into retained_threads"
            );
            assert_ne!(
                panel.active_thread_id(cx),
                Some(retained_draft_id),
                "active view should be a fresh ephemeral draft, not the retained one"
            );
        });

        // 3. Type into the new ephemeral draft.
        let draft_thread_id = crate::test_support::active_thread_id(&panel, cx);
        crate::test_support::type_draft_prompt(&panel, "in-flight draft text", cx);

        // Sanity-check: both drafts' text has been persisted to the kvp
        // store via the editor observer / PromptUpdated chain.
        let (ephemeral_kvp, retained_kvp) = cx.update(|_, cx| {
            (
                crate::draft_prompt_store::read(draft_thread_id, cx),
                crate::draft_prompt_store::read(retained_draft_id, cx),
            )
        });
        assert!(
            ephemeral_kvp.is_some(),
            "ephemeral draft's prompt should be in the kvp store"
        );
        assert!(
            retained_kvp.is_some(),
            "retained draft's prompt should be in the kvp store"
        );

        assert_ne!(real_thread_id, draft_thread_id);
        assert_ne!(retained_draft_id, draft_thread_id);
        panel.read_with(cx, |panel, cx| {
            assert!(
                panel.active_view_is_new_draft(cx),
                "draft should currently occupy the new-draft slot"
            );
        });

        // 4. Switch the active view back to the real thread. The ephemeral
        //    draft has content, so it gets parked into `retained_threads`
        //    immediately (the `draft_thread` slot is cleared).
        panel.update_in(cx, |panel, window, cx| {
            panel.load_agent_thread(
                Agent::Stub,
                real_thread_id,
                None,
                None,
                false,
                AgentThreadSource::AgentPanel,
                window,
                cx,
            );
        });
        cx.run_until_parked();

        panel.read_with(cx, |panel, cx| {
            assert_eq!(panel.active_thread_id(cx), Some(real_thread_id));
            assert!(!panel.active_view_is_new_draft(cx));
        });

        // 5. Serialize + reload.
        panel.update(cx, |panel, cx| panel.serialize(cx));
        // パネル内レイアウトの保存は 50 ms のデバウンス越しなので、時計を
        // 進めて書き終えさせる。
        cx.executor().advance_clock(Duration::from_millis(200));
        cx.run_until_parked();
        // 再起動なら古いパネルは居ない。タブを手放させ、ワークスペースからも
        // 外してから読み直す (ADR-F36 Phase B)。
        panel.update_in(cx, |panel, window, cx| {
            panel.test_close_all_thread_tabs(window, cx);
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.remove_panel::<AgentPanel>(&panel, window, cx);
        });
        cx.run_until_parked();
        let async_cx = cx.update(|window, cx| window.to_async(cx));
        let loaded_panel = AgentPanel::load(workspace.downgrade(), async_cx)
            .await
            .expect("panel load should succeed");
        cx.run_until_parked();

        // 6. The real thread is the active view on reload. The draft
        //    was parked when the user navigated away, so the draft_thread
        //    slot is empty.
        loaded_panel.read_with(cx, |panel, cx| {
            assert_eq!(
                panel.active_thread_id(cx),
                Some(real_thread_id),
                "real thread should be the active view after reload"
            );
            assert!(
                !panel.active_thread_is_draft(cx),
                "real thread is not a draft"
            );
            // ADR-F36 Phase B: 打ちかけの下書きはタブに残るので、下書き枠も
            // そのまま次の起動へ持ち越す (以前は「離れたら保持枠へ」だった)。
            assert_eq!(
                panel
                    .draft_thread
                    .as_ref()
                    .map(|draft| draft.read(cx).thread_id),
                Some(draft_thread_id),
                "the new-draft slot keeps the draft that still has its own tab"
            );
        });

        // 7. All three threads' metadata rows survive the reload.
        cx.update(|_window, cx| {
            let store = ThreadMetadataStore::global(cx).read(cx);
            let ephemeral_row = store
                .entry(draft_thread_id)
                .expect("ephemeral draft metadata row should survive reload");
            assert!(
                ephemeral_row.is_draft(),
                "ephemeral draft row should still be a draft"
            );
            let retained_row = store
                .entry(retained_draft_id)
                .expect("retained draft metadata row should survive reload");
            assert!(
                retained_row.is_draft(),
                "retained draft row should still be a draft"
            );
            let real_row = store
                .entry(real_thread_id)
                .expect("real thread metadata row should survive reload");
            assert_eq!(real_row.session_id.as_ref(), Some(&real_session_id));
        });

        // 8. Opening the parked draft via load_agent_thread activates
        //    a fresh ConversationView and exposes its kvp-seeded prompt
        //    text in the editor.
        loaded_panel.update_in(cx, |panel, window, cx| {
            panel.load_agent_thread(
                Agent::Stub,
                draft_thread_id,
                None,
                None,
                false,
                AgentThreadSource::AgentPanel,
                window,
                cx,
            );
        });
        cx.run_until_parked();

        let restored_ephemeral_text =
            loaded_panel.read_with(cx, |panel, cx| panel.editor_text(draft_thread_id, cx));
        assert_eq!(
            restored_ephemeral_text.as_deref(),
            Some("in-flight draft text"),
            "ephemeral draft prompt text should be restored from the kvp store"
        );

        // 9. Opening the retained draft via load_agent_thread builds a
        //    fresh ConversationView (since retained_threads was not
        //    carried across the reload) and seeds its editor from the
        //    kvp store.
        loaded_panel.update_in(cx, |panel, window, cx| {
            panel.load_agent_thread(
                Agent::Stub,
                retained_draft_id,
                None,
                None,
                false,
                AgentThreadSource::AgentPanel,
                window,
                cx,
            );
        });
        cx.run_until_parked();

        let restored_retained_text =
            loaded_panel.read_with(cx, |panel, cx| panel.editor_text(retained_draft_id, cx));
        assert_eq!(
            restored_retained_text.as_deref(),
            Some("retained draft text"),
            "retained draft prompt text should be restored from the kvp store"
        );
    }

    #[gpui::test]
    async fn test_reloaded_ephemeral_draft_preserves_original_agent(cx: &mut TestAppContext) {
        init_test(cx);
        cx.update(|cx| {
            agent::ThreadStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/project", json!({ "file.txt": "" })).await;
        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;

        let multi_workspace =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace
            .read_with(cx, |mw, _cx| mw.workspace().clone())
            .unwrap();
        workspace.update(cx, |workspace, _cx| workspace.set_random_database_id());

        let cx = &mut VisualTestContext::from_window(multi_workspace.into(), cx);
        let panel = workspace.update_in(cx, |workspace, window, cx| {
            let panel = cx.new(|cx| AgentPanel::new(workspace, window, cx));
            workspace.add_panel(panel.clone(), window, cx);
            panel
        });

        let _stub_connection =
            crate::test_support::set_stub_agent_connection(StubAgentConnection::new());
        panel.update_in(cx, |panel, window, cx| {
            panel.selected_agent = Agent::Stub;
            panel.activate_draft(true, AgentThreadSource::AgentPanel, window, cx);
        });
        cx.run_until_parked();

        let draft_thread_id = crate::test_support::active_thread_id(&panel, cx);
        crate::test_support::type_draft_prompt(&panel, "pinned to stub", cx);

        // Diverge `selected_agent` from the draft's bound agent before
        // serialize.
        let other_agent = Agent::Custom {
            id: "other-agent".into(),
        };
        panel.update(cx, |panel, _cx| {
            panel.selected_agent = other_agent.clone();
        });
        panel.update(cx, |panel, cx| panel.serialize(cx));
        cx.run_until_parked();

        // Sanity-check: the draft's metadata row has agent_id="stub",
        // not "other-agent".
        cx.update(|_, cx| {
            let store = ThreadMetadataStore::global(cx).read(cx);
            let row = store
                .entry(draft_thread_id)
                .expect("draft metadata row should exist");
            assert_eq!(
                row.agent_id.as_ref(),
                "stub",
                "draft metadata should retain its original agent binding"
            );
        });

        let async_cx = cx.update(|window, cx| window.to_async(cx));
        let reloaded_panel = AgentPanel::load(workspace.downgrade(), async_cx)
            .await
            .expect("panel load should succeed");
        cx.run_until_parked();

        reloaded_panel.read_with(cx, |panel, cx| {
            let draft_view = panel
                .draft_thread
                .as_ref()
                .expect("draft slot should be repopulated");
            assert_eq!(
                draft_view.read(cx).thread_id,
                draft_thread_id,
                "restored draft should have the same ThreadId"
            );
            assert_eq!(
                draft_view.read(cx).agent_key(),
                &Agent::Stub,
                "restored draft should still be bound to its original Agent::Stub, \
                 not the panel's current `selected_agent`"
            );
        });
    }

    #[gpui::test]
    async fn test_empty_workspace_does_not_create_agent_entries(cx: &mut TestAppContext) {
        init_test(cx);
        cx.update(|cx| {
            agent::ThreadStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);
        });

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs.clone(), [], cx).await;
        let multi_workspace =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace
            .read_with(cx, |multi_workspace, _cx| {
                multi_workspace.workspace().clone()
            })
            .unwrap();
        let cx = &mut VisualTestContext::from_window(multi_workspace.into(), cx);

        let panel = workspace.update_in(cx, |workspace, window, cx| {
            let panel = cx.new(|cx| AgentPanel::new(workspace, window, cx));
            workspace.add_panel(panel.clone(), window, cx);
            panel
        });

        panel.read_with(cx, |panel, cx| {
            assert_eq!(
                panel
                    .connection_store()
                    .read(cx)
                    .connection_status(&Agent::NativeAgent, cx),
                crate::agent_connection_store::AgentConnectionStatus::Disconnected,
                "empty workspaces should not start the native agent connection"
            );
        });

        panel.update_in(cx, |panel, window, cx| {
            panel.new_thread(&NewThread, window, cx);
            panel.activate_draft(true, AgentThreadSource::AgentPanel, window, cx);
            panel.new_external_agent_thread(
                &NewExternalAgentThread {
                    agent: AgentId::new("external-agent"),
                },
                window,
                cx,
            );
        });
        cx.run_until_parked();

        panel.read_with(cx, |panel, cx| {
            assert!(
                panel.active_conversation_view().is_none(),
                "empty workspaces should not create agent threads"
            );
            assert!(
                panel.draft_thread.is_none(),
                "empty workspaces should not create draft threads"
            );
            assert!(
                panel.terminals(cx).is_empty(),
                "empty workspaces should not create agent panel terminals"
            );
        });

        cx.update(|_, cx| {
            cx.update_flags(true, vec!["agent-panel-terminal".to_string()]);
        });
        panel.update_in(cx, |panel, window, cx| {
            panel.new_terminal(None, AgentThreadSource::AgentPanel, window, cx);
        });
        cx.run_until_parked();

        panel.read_with(cx, |panel, cx| {
            assert!(
                panel.terminals(cx).is_empty(),
                "empty workspaces should not create terminals after the terminal feature is enabled"
            );
            assert_eq!(
                panel
                    .connection_store()
                    .read(cx)
                    .connection_status(&Agent::NativeAgent, cx),
                crate::agent_connection_store::AgentConnectionStatus::Disconnected,
                "empty workspace actions should not start the native agent connection"
            );
        });
    }

    #[gpui::test]
    async fn test_add_selection_to_terminal_thread_pastes_mention(cx: &mut TestAppContext) {
        init_test(cx);
        cx.update(|cx| {
            agent::ThreadStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/project",
            json!({ "file.rs": "line one\nline two\nline three\n" }),
        )
        .await;
        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;

        let multi_workspace =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace
            .read_with(cx, |mw, _cx| mw.workspace().clone())
            .unwrap();
        let mut cx = VisualTestContext::from_window(multi_workspace.into(), cx);

        let panel = workspace.update_in(&mut cx, |workspace, window, cx| {
            let panel = cx.new(|cx| AgentPanel::new(workspace, window, cx));
            workspace.add_panel(panel.clone(), window, cx);
            panel
        });

        // Make a terminal thread the active conversation. A display-only terminal
        // avoids spawning a real shell; its working directory is supplied directly
        // so the mention resolves relative to it. No agent is started inside it.
        let terminal_id = TerminalId::new();
        panel
            .update_in(&mut cx, |panel, window, cx| {
                panel.insert_display_only_terminal(
                    terminal_id,
                    Some(PathBuf::from("/project")),
                    Some("Terminal".into()),
                    None,
                    None,
                    true,
                    true,
                    false,
                    AgentThreadSource::AgentPanel,
                    window,
                    cx,
                )
            })
            .expect("display-only terminal should be inserted");
        cx.run_until_parked();

        panel.read_with(&cx, |panel, _cx| {
            assert_eq!(panel.active_terminal_id(), Some(terminal_id));
            assert!(panel.active_conversation_view().is_none());
        });

        // Open the file in the center pane so the selection comes from a
        // worktree-backed editor (with a project path).
        workspace
            .update_in(&mut cx, |workspace, window, cx| {
                workspace.open_paths(
                    vec![PathBuf::from("/project/file.rs")],
                    workspace::OpenOptions::default(),
                    None,
                    window,
                    cx,
                )
            })
            .await;
        cx.run_until_parked();

        let editor = workspace.update(&mut cx, |workspace, cx| {
            workspace
                .active_item(cx)
                .and_then(|item| item.act_as::<Editor>(cx))
                .expect("opened file should be an editor")
        });

        cx.focus(&editor);
        cx.run_until_parked();

        let terminal = panel.read_with(&cx, |panel, cx| {
            panel
                .terminals
                .get(&terminal_id)
                .expect("terminal should exist")
                .view
                .read(cx)
                .terminal()
                .clone()
        });
        // Drop any input the terminal may have received during setup.
        terminal.update(&mut cx, |terminal, _| {
            terminal.take_input_log();
        });

        // With only a cursor and nothing highlighted, the action is a no-op and
        // must not paste anything into the terminal.
        workspace.update_in(&mut cx, |_, window, cx| {
            window.dispatch_action(AddSelectionToThread.boxed_clone(), cx);
        });
        cx.run_until_parked();
        let pasted_without_selection =
            terminal.update(&mut cx, |terminal, _| terminal.take_input_log());
        assert!(
            pasted_without_selection.is_empty(),
            "no selection should paste nothing, got {pasted_without_selection:?}"
        );

        // Now highlight a portion of the file: from the start of line 2 into line 3.
        editor.update_in(&mut cx, |editor, window, cx| {
            editor.change_selections(Default::default(), window, cx, |selections| {
                selections.select_ranges([text::Point::new(1, 0)..text::Point::new(2, 4)]);
            });
        });
        cx.run_until_parked();

        workspace.update_in(&mut cx, |_, window, cx| {
            window.dispatch_action(AddSelectionToThread.boxed_clone(), cx);
        });
        cx.run_until_parked();

        let pasted: String = terminal
            .update(&mut cx, |terminal, _| terminal.take_input_log())
            .into_iter()
            .map(|bytes| String::from_utf8(bytes).expect("pasted bytes should be valid UTF-8"))
            .collect();

        // Lines are 1-based and inclusive; the path is presented as
        // `<rel-path>:<start>-<end>`, with a trailing space. ADR-F56: this tab
        // does not run Claude Code, so it keeps this form.
        assert_eq!(pasted, "file.rs:2-3 ");

        // ADR-F56: the Claude Code the launch action starts is recognised by its task.
        workspace.read_with(&cx, |workspace, cx| {
            assert_eq!(
                terminal_view::session_tools::claude_code_launch_task(None, workspace, cx, None)
                    .id
                    .0,
                HOST_CLAUDE_CODE_TASK_ID
            );
        });
    }

    #[gpui::test]
    async fn test_panel_thread_history_hides_when_panel_is_narrow(cx: &mut TestAppContext) {
        let (panel, mut cx) = setup_panel(cx).await;
        cx.run_until_parked();

        panel.read_with(&cx, |panel, cx| {
            assert!(
                panel.thread_history_visible(cx),
                "a project-backed panel shows the history list by default"
            );
        });

        panel.update(&mut cx, |panel, _cx| {
            panel.measured_panel_width.set(Some(
                SLIM_PANEL_THREAD_HISTORY_WIDTH + MIN_THREAD_WIDTH_BESIDE_HISTORY - px(1.),
            ));
        });
        panel.read_with(&cx, |panel, cx| {
            assert!(
                !panel.thread_history_visible(cx),
                "a panel too narrow for both halves drops the list"
            );
            assert!(
                panel.thread_history_enabled(cx),
                "hiding the list on width must not flip the toolbar toggle"
            );
        });

        panel.update(&mut cx, |panel, _cx| {
            panel.measured_panel_width.set(None);
        });
        cx.update(|_, cx| {
            let mut settings = AgentSettings::get_global(cx).clone();
            settings.panel_thread_history = false;
            AgentSettings::override_global(settings, cx);
        });
        panel.read_with(&cx, |panel, cx| {
            assert!(
                !panel.thread_history_visible(cx),
                "turning the setting off hides the list at any width"
            );
        });
    }

    /// ADR-F53: 少し狭くなっただけで一覧ごと消さない。240px の一覧と「一覧を
    /// 出さない」のあいだに、題名 1 行だけの 160px の一覧を挟む。
    #[gpui::test]
    async fn test_a_narrow_panel_gets_a_slim_history_before_it_loses_it(cx: &mut TestAppContext) {
        let (panel, mut cx) = setup_visible_panel(cx).await;
        cx.run_until_parked();

        let fit_at = |width: Pixels, cx: &mut VisualTestContext| {
            panel.update(cx, |panel, _cx| {
                panel.measured_panel_width.set(Some(width));
            });
            panel.read_with(cx, |panel, _cx| panel.thread_history_fit())
        };

        assert_eq!(fit_at(px(600.), &mut cx), ThreadHistoryFit::Full);
        assert_eq!(
            fit_at(
                PANEL_THREAD_HISTORY_WIDTH + MIN_THREAD_WIDTH_BESIDE_HISTORY,
                &mut cx
            ),
            ThreadHistoryFit::Full,
            "ちょうど 240 + 320 なら広いほう"
        );
        assert_eq!(fit_at(px(559.), &mut cx), ThreadHistoryFit::Slim);
        assert_eq!(fit_at(px(500.), &mut cx), ThreadHistoryFit::Slim);
        assert_eq!(
            fit_at(
                SLIM_PANEL_THREAD_HISTORY_WIDTH + MIN_THREAD_WIDTH_BESIDE_HISTORY,
                &mut cx
            ),
            ThreadHistoryFit::Slim,
            "ちょうど 160 + 320 ならまだ細い一覧が入る"
        );
        assert_eq!(fit_at(px(479.), &mut cx), ThreadHistoryFit::None);

        // 細い一覧も「一覧が出ている」: 開くボタンにもタブバーにも落ちない。
        assert_eq!(fit_at(px(500.), &mut cx), ThreadHistoryFit::Slim);
        panel.read_with(&cx, |panel, cx| {
            assert!(panel.thread_history_visible(cx), "細くても一覧は出ている");
            assert!(
                !panel.thread_history_open_button_visible(cx),
                "一覧が出ているあいだは開くボタンを出さない"
            );
            assert!(
                !panel.tab_bar_visible(cx),
                "auto: 一覧から開いているタブへ行けるのでタブバーは要らない"
            );
        });

        assert_eq!(fit_at(px(479.), &mut cx), ThreadHistoryFit::None);
        panel.read_with(&cx, |panel, cx| {
            assert!(!panel.thread_history_visible(cx));
            assert!(!panel.thread_history_open_button_visible(cx));
            assert!(
                panel.tab_bar_visible(cx),
                "一覧も開くボタンも出せない幅ではタブバーを戻す (行き止まりを作らない)"
            );
        });

        // 描く幅も切り替わる。幅は実測なので、ウィンドウごと狭めて確かめる。
        let mut seen = Vec::new();
        for window_width in [
            px(1400.),
            px(1200.),
            px(1100.),
            px(1000.),
            px(950.),
            px(900.),
            px(850.),
            px(800.),
            px(700.),
            px(600.),
        ] {
            cx.simulate_resize(size(window_width, px(700.)));
            cx.update(|window, _cx| window.refresh());
            cx.run_until_parked();
            let (fit, measured, list_is_slim) = panel.read_with(&cx, |panel, cx| {
                (
                    panel.thread_history_fit(),
                    panel.measured_panel_width.get(),
                    panel.thread_history.read(cx).is_slim(),
                )
            });
            assert_eq!(
                list_is_slim,
                fit == ThreadHistoryFit::Slim,
                "パネル幅 {measured:?} で一覧に伝えた細さ"
            );
            let drawn = cx
                .debug_bounds("PANEL_THREAD_HISTORY")
                .map(|bounds| bounds.size.width);
            let expected = match fit {
                ThreadHistoryFit::Full => Some(PANEL_THREAD_HISTORY_WIDTH),
                ThreadHistoryFit::Slim => Some(SLIM_PANEL_THREAD_HISTORY_WIDTH),
                ThreadHistoryFit::None => None,
            };
            assert_eq!(
                drawn, expected,
                "パネル幅 {measured:?} ({fit:?}) で描いた一覧の幅"
            );
            seen.push((window_width, measured, fit));
        }
        for fit in [
            ThreadHistoryFit::Full,
            ThreadHistoryFit::Slim,
            ThreadHistoryFit::None,
        ] {
            assert!(
                seen.iter().any(|(_, _, seen_fit)| *seen_fit == fit),
                "3 段すべてを通っていないと確かめたことにならない: {seen:?}"
            );
        }
    }

    #[gpui::test]
    async fn test_panel_thread_history_lists_threads_for_the_open_project(cx: &mut TestAppContext) {
        use crate::panel_thread_history::HistoryListItem;
        use crate::thread_metadata_store::ThreadMetadata;

        let (panel, mut cx) = setup_panel(cx).await;
        cx.run_until_parked();

        let in_project = ThreadId::new();
        let elsewhere = ThreadId::new();
        cx.update(|_, cx| {
            ThreadMetadataStore::global(cx).update(cx, |store, cx| {
                for (thread_id, root) in [(in_project, "/project"), (elsewhere, "/other")] {
                    store.save(
                        ThreadMetadata {
                            thread_id,
                            session_id: Some(acp::SessionId::new(root.to_string())),
                            agent_id: AgentId("zed".into()),
                            title: Some(root.into()),
                            title_override: None,
                            updated_at: Utc::now(),
                            created_at: Some(Utc::now()),
                            interacted_at: None,
                            worktree_paths: WorktreePaths::from_folder_paths(&PathList::new(&[
                                PathBuf::from(root),
                            ])),
                            remote_connection: None,
                            archived: false,
                        },
                        cx,
                    );
                }
            });
        });
        cx.run_until_parked();

        panel.read_with(&cx, |panel, cx| {
            let listed = panel
                .thread_history
                .read(cx)
                .items()
                .iter()
                .filter_map(|item| match item {
                    HistoryListItem::Entry { thread, .. } => Some(thread.thread_id),
                    HistoryListItem::BucketSeparator(_) => None,
                })
                .collect::<Vec<_>>();
            assert!(
                listed.contains(&in_project),
                "threads rooted in the panel's project are listed"
            );
            assert!(
                !listed.contains(&elsewhere),
                "threads from other projects stay out of the panel's list"
            );
        });
    }

    /// Whether the panel's store still holds a connection to the stub agent,
    /// which is what the idle disconnect drops.
    fn stub_is_connected(panel: &Entity<AgentPanel>, cx: &VisualTestContext) -> bool {
        panel.read_with(cx, |panel, cx| {
            panel
                .connection_store
                .read(cx)
                .entry(&Agent::Stub)
                .is_some()
        })
    }

    /// The `Rc` the store is keeping for [`Agent::Stub`] — what in production
    /// the agent's process hangs from.
    fn stub_agent_connection_rc(
        panel: &Entity<AgentPanel>,
        cx: &VisualTestContext,
    ) -> Rc<dyn AgentConnection> {
        panel.read_with(cx, |panel, cx| {
            match panel
                .connection_store
                .read(cx)
                .entry(&Agent::Stub)
                .expect("the stub agent should be connected")
                .read(cx)
            {
                crate::agent_connection_store::AgentConnectionEntry::Connected(state) => {
                    state.connection.clone()
                }
                _ => panic!("the stub agent should be connected"),
            }
        })
    }

    /// The panel with the stub agent selected and an empty draft in its
    /// new-thread slot, on screen, connected.
    async fn setup_panel_showing_an_empty_draft(
        cx: &mut TestAppContext,
    ) -> (Entity<AgentPanel>, ThreadId, VisualTestContext) {
        let (panel, mut cx) = setup_visible_panel(cx).await;
        cx.run_until_parked();
        crate::test_support::set_stub_agent_connection(StubAgentConnection::new());
        panel.update_in(&mut cx, |panel, window, cx| {
            // A panel shown for the first time makes a draft for its own
            // agent; this is about the external one, so that thread goes
            // first and the stub's draft is the only one the panel holds.
            for thread_id in panel.tabbed_threads.keys().copied().collect::<Vec<_>>() {
                panel.test_close_thread_tab(thread_id, window, cx);
                ThreadMetadataStore::global(cx).update(cx, |store, cx| {
                    store.delete(thread_id, cx);
                });
            }
            panel.selected_agent = Agent::Stub;
            panel.activate_draft(true, AgentThreadSource::AgentPanel, window, cx);
        });
        cx.run_until_parked();
        let thread_id = active_thread_id(&panel, &cx);
        assert!(
            stub_is_connected(&panel, &cx),
            "a draft for an external agent connects to it"
        );
        (panel, thread_id, cx)
    }

    fn draft_view(panel: &Entity<AgentPanel>, cx: &VisualTestContext) -> Entity<ConversationView> {
        panel.read_with(cx, |panel, _cx| {
            panel
                .draft_thread
                .as_ref()
                .expect("the panel holds a draft")
                .clone()
        })
    }

    /// Whether the parked draft's "reconnect now" button is in the frame the
    /// panel just drew: the row is rendered only by
    /// `ConversationView`'s parked state, and the button is the only thing
    /// that offers this action, so the focused element can reach it exactly
    /// when that row is on screen.
    fn reconnect_is_offered(cx: &mut VisualTestContext) -> bool {
        cx.update(|window, cx| {
            window
                .available_actions(cx)
                .iter()
                .any(|action| action.as_any().is::<crate::ReconnectParkedAgent>())
        })
    }

    /// The controls a draft's thread draws — model, attachments, tokens — need
    /// a connection, so a parked draft cannot draw them. A row of buttons that
    /// quietly disappears after five minutes reads as a bug, so the parked
    /// draft says what happened in their place and offers the way back.
    #[gpui::test]
    async fn test_a_parked_draft_says_its_agent_was_stopped(cx: &mut TestAppContext) {
        let (panel, _thread_id, mut cx) = setup_panel_showing_an_empty_draft(cx).await;
        let message_editor = crate::test_support::active_draft_editor(&panel, &cx);
        cx.focus(&message_editor);
        cx.run_until_parked();
        assert!(
            !reconnect_is_offered(&mut cx),
            "a connected draft has its own controls and nothing to explain"
        );

        cx.executor().advance_clock(Duration::from_secs(10 * 60));
        cx.focus(&message_editor);
        cx.run_until_parked();

        assert!(
            reconnect_is_offered(&mut cx),
            "a parked draft says so where those controls were, with the way back"
        );
    }

    /// The way back that does not send: the agent comes back, the draft keeps
    /// its text, and nothing is put to the agent until the user chooses to.
    #[gpui::test]
    async fn test_reconnecting_a_parked_draft_sends_nothing(cx: &mut TestAppContext) {
        let (panel, thread_id, mut cx) = setup_panel_showing_an_empty_draft(cx).await;
        cx.executor().advance_clock(Duration::from_secs(10 * 60));
        cx.run_until_parked();
        assert!(!stub_is_connected(&panel, &cx));

        let message_editor = crate::test_support::active_draft_editor(&panel, &cx);
        message_editor.update_in(&mut cx, |editor, window, cx| {
            editor.set_text("まだ送らない下書き", window, cx);
        });
        cx.focus(&message_editor);
        cx.run_until_parked();

        message_editor.update_in(&mut cx, |_editor, window, cx| {
            window.dispatch_action(Box::new(crate::ReconnectParkedAgent), cx);
        });
        cx.run_until_parked();

        assert!(
            stub_is_connected(&panel, &cx),
            "the row's button starts the agent again"
        );
        let view = draft_view(&panel, &cx);
        view.read_with(&cx, |view, _cx| {
            assert!(!view.is_parked());
            assert_eq!(
                view.thread_id, thread_id,
                "the same thread, under the same row"
            );
        });
        assert_eq!(
            crate::test_support::active_draft_editor(&panel, &cx)
                .read_with(&cx, |editor, cx| editor.text(cx)),
            "まだ送らない下書き",
            "written in the same box, which came back with its text"
        );
        panel.read_with(&cx, |panel, cx| {
            let thread = panel
                .active_agent_thread(cx)
                .expect("the draft is connected again");
            let thread = thread.read(cx);
            assert!(
                thread.entries().is_empty(),
                "and nothing was put to the agent: {}",
                thread.to_markdown(cx)
            );
            assert!(thread.is_draft_thread(), "it is still a draft");
        });
    }

    /// ADR-F49 (new decision): the empty draft the panel keeps for the agent it
    /// has selected held that agent's connection for as long as the window
    /// lived, so the one agent the user picked last was the one that never
    /// stopped. It now lets go of the connection where it stands: the tab, the
    /// row and the box the user writes in all stay, and only the process goes.
    #[gpui::test]
    async fn test_an_empty_draft_on_screen_lets_its_agent_go_and_keeps_its_tab(
        cx: &mut TestAppContext,
    ) {
        let (panel, thread_id, mut cx) = setup_panel_showing_an_empty_draft(cx).await;
        let view = draft_view(&panel, &cx);
        // Everything the connection reaches through: the thread, the view that
        // renders it and the conversation they share. If any of them outlived
        // the store's entry, the agent's process would outlive it too.
        let (released_thread, released_thread_view) = view.read_with(&cx, |view, cx| {
            let thread_view = view.root_thread_view().expect("the draft is connected");
            (
                thread_view.read(cx).thread.downgrade(),
                thread_view.downgrade(),
            )
        });
        // And the connection itself, which is what the agent's process hangs
        // from: the store letting go of its entry only reaches the process
        // while no other hand is on the `Rc`.
        let released_connection = Rc::downgrade(&stub_agent_connection_rc(&panel, &cx));

        cx.executor().advance_clock(Duration::from_secs(10 * 60));
        cx.run_until_parked();

        assert!(
            !stub_is_connected(&panel, &cx),
            "an empty draft is not a thread the user is in, on screen or not"
        );
        // The leak detector is what proves the process really ends: the store
        // dropping its entry is not enough while a view still holds an `Rc` on
        // the connection.
        released_thread.assert_released();
        released_thread_view.assert_released();
        assert_eq!(
            released_connection.strong_count(),
            0,
            "and nothing is left holding the connection the agent runs behind"
        );

        panel.read_with(&cx, |panel, cx| {
            assert_eq!(
                panel
                    .draft_thread
                    .as_ref()
                    .map(|draft| draft.read(cx).thread_id),
                Some(thread_id),
                "the draft stays in the new-thread slot"
            );
            assert_eq!(
                panel.active_thread_id(cx),
                Some(thread_id),
                "and the panel keeps drawing it rather than emptying itself"
            );
            assert!(
                panel.active_thread_is_draft(cx),
                "a parked draft is still a draft as far as the panel is concerned"
            );
        });
        assert!(
            cx.update(|_window, cx| ThreadItem::is_open_in_a_tab(thread_id, cx)),
            "its tab does not disappear from under the user"
        );
        assert!(view.read_with(&cx, |view, _cx| view.is_parked()));
        cx.update(|_window, cx| {
            let drafts = ThreadMetadataStore::global(cx)
                .read(cx)
                .entries()
                .filter(|thread| thread.is_draft())
                .map(|thread| thread.thread_id)
                .collect::<Vec<_>>();
            assert_eq!(drafts, vec![thread_id], "and its row stays exactly once");
        });
    }

    /// What is typed into a parked draft is the user's, and typing is not a
    /// reason to start the agent again. Sending is: the agent comes back, the
    /// prompt goes out, and it is the text that was typed while the agent was
    /// gone.
    #[gpui::test]
    async fn test_a_parked_draft_sends_what_was_typed_in_it_once_its_agent_is_back(
        cx: &mut TestAppContext,
    ) {
        let (panel, thread_id, mut cx) = setup_panel_showing_an_empty_draft(cx).await;
        let parked_session_id = crate::test_support::active_session_id(&panel, &cx);
        cx.executor().advance_clock(Duration::from_secs(10 * 60));
        cx.run_until_parked();
        assert!(!stub_is_connected(&panel, &cx));

        let message_editor = crate::test_support::active_draft_editor(&panel, &cx);
        message_editor.update_in(&mut cx, |editor, window, cx| {
            editor.set_text("パークしてから書いた下書き", window, cx);
        });
        // Also what says the parked view draws: a keystroke only reaches the
        // editor through the element tree the parked state renders.
        cx.focus(&message_editor);
        cx.simulate_input("!");
        cx.run_until_parked();
        cx.executor()
            .advance_clock(crate::conversation_view::DRAFT_PROMPT_PERSIST_DEBOUNCE * 2);
        cx.run_until_parked();

        assert!(
            !stub_is_connected(&panel, &cx),
            "typing is not a reason to start the agent again"
        );
        assert_eq!(
            message_editor.read_with(&cx, |editor, cx| editor.text(cx)),
            "パークしてから書いた下書き!",
            "and what was typed stays in the box it was typed in"
        );
        assert!(
            cx.update(|_window, cx| crate::draft_prompt_store::read(thread_id, cx).is_some()),
            "a parked draft persists what is written in it, so a window that              closes before the send does not take the text with it"
        );

        cx.focus(&message_editor);
        cx.run_until_parked();
        message_editor.update_in(&mut cx, |_editor, window, cx| {
            window.dispatch_action(Box::new(zed_actions::agent::Chat), cx);
        });
        cx.run_until_parked();

        assert!(
            stub_is_connected(&panel, &cx),
            "sending starts the agent again"
        );
        panel.read_with(&cx, |panel, cx| {
            let view = panel
                .active_conversation_view()
                .expect("the panel is still showing the thread that was parked");
            assert!(!view.read(cx).is_parked());
            assert_eq!(
                panel.active_thread_id(cx),
                Some(thread_id),
                "in the same thread, under the same row"
            );
            assert!(
                panel.draft_thread.is_none(),
                "and the new-thread slot let go of it, the way sending any draft does"
            );
        });
        let entries = panel.read_with(&cx, |panel, cx| {
            panel
                .active_agent_thread(cx)
                .expect("the draft is connected again")
                .read(cx)
                .to_markdown(cx)
        });
        assert!(
            entries.contains("パークしてから書いた下書き"),
            "the prompt carried the text typed while the agent was gone: {entries}"
        );
        // `Agent::Stub` answers every turn with this, so the reply is only
        // there if the reconnected thread really ran a turn against a live
        // connection rather than recording the message locally.
        assert!(
            entries.contains("Default response"),
            "and it reached the agent, which answered: {entries}"
        );
        assert_ne!(
            crate::test_support::active_session_id(&panel, &cx),
            parked_session_id,
            "on a session of its own: the one it had went with the agent"
        );
    }

    /// Asking for the agent back can fail — it is gone, the sign-in has to be
    /// repeated, its hosts have to be allowed — and the card that says so
    /// replaces the whole view, box and all. So what is in the box is written
    /// out as the agent is asked for, rather than left to the persist debounce
    /// that a send one keystroke after the last one outruns. Both ways back
    /// (this one and the send) go through the same place.
    #[gpui::test]
    async fn test_a_parked_draft_writes_its_text_out_before_asking_for_the_agent(
        cx: &mut TestAppContext,
    ) {
        let (panel, thread_id, mut cx) = setup_panel_showing_an_empty_draft(cx).await;
        cx.executor().advance_clock(Duration::from_secs(10 * 60));
        cx.run_until_parked();

        let message_editor = crate::test_support::active_draft_editor(&panel, &cx);
        message_editor.update_in(&mut cx, |editor, window, cx| {
            editor.set_text("消えては困る下書き", window, cx);
        });
        cx.focus(&message_editor);
        cx.run_until_parked();
        // The clock is deliberately not advanced: this is the keystroke that
        // is still inside the debounce when the user asks for the agent.
        assert!(
            cx.update(|_window, cx| crate::draft_prompt_store::read(thread_id, cx).is_none()),
            "the debounced write has not run yet, which is the case this is about"
        );

        message_editor.update_in(&mut cx, |_editor, window, cx| {
            window.dispatch_action(Box::new(crate::ReconnectParkedAgent), cx);
        });
        cx.run_until_parked();

        let persisted = cx
            .update(|_window, cx| crate::draft_prompt_store::read(thread_id, cx))
            .expect("asking for the agent wrote the draft out first");
        assert!(
            persisted.iter().any(|block| matches!(
                block,
                acp::ContentBlock::Text(text) if text.text.contains("消えては困る下書き")
            )),
            "with what was in the box: {persisted:?}"
        );
    }

    /// The other half of that: a draft built again under the same id — which
    /// is what the sign-in, the allowlist and the failure cards all end in —
    /// comes back with the text rather than an empty box.
    #[gpui::test]
    async fn test_a_draft_rebuilt_under_the_same_id_comes_back_with_its_text(
        cx: &mut TestAppContext,
    ) {
        let (panel, thread_id, mut cx) = setup_panel_showing_an_empty_draft(cx).await;
        cx.executor().advance_clock(Duration::from_secs(10 * 60));
        cx.run_until_parked();

        let view = draft_view(&panel, &cx);
        crate::test_support::active_draft_editor(&panel, &cx).update_in(
            &mut cx,
            |editor, window, cx| {
                editor.set_text("作り直しても残る下書き", window, cx);
            },
        );
        cx.executor()
            .advance_clock(crate::conversation_view::DRAFT_PROMPT_PERSIST_DEBOUNCE * 2);
        cx.run_until_parked();

        view.update_in(&mut cx, |view, window, cx| view.test_reset(window, cx));
        cx.run_until_parked();

        assert!(
            stub_is_connected(&panel, &cx),
            "the rebuilt draft asks for its agent"
        );
        assert_eq!(
            crate::test_support::active_draft_editor(&panel, &cx)
                .read_with(&cx, |editor, cx| editor.text(cx)),
            "作り直しても残る下書き",
            "and is filled from what was written out for this id"
        );
        assert_eq!(
            panel.read_with(&cx, |panel, cx| panel.active_thread_id(cx)),
            Some(thread_id),
            "under the same row"
        );
    }

    /// The dead end the idle disconnect would otherwise leave: the draft's
    /// agent was stopped, the send asks for it back, and it does not come.
    /// The card that says so is what the panel draws for that agent from then
    /// on, so it carries the way back — and the way back brings the text and
    /// the folders the session was on with it.
    #[gpui::test]
    async fn test_a_start_that_failed_can_be_tried_again_with_its_text_and_folders(
        cx: &mut TestAppContext,
    ) {
        let (panel, _thread_id, mut cx) = setup_panel_showing_an_empty_draft(cx).await;
        let started_on = panel
            .read_with(&cx, |panel, cx| {
                panel
                    .active_agent_thread(cx)
                    .expect("the draft is connected")
                    .read(cx)
                    .work_dirs()
                    .cloned()
            })
            .expect("the session was opened on the project's folder");

        cx.executor().advance_clock(Duration::from_secs(10 * 60));
        cx.run_until_parked();
        assert!(draft_view(&panel, &cx).read_with(&cx, |view, _cx| view.is_parked()));

        // The project grows a folder while the agent is stopped, so that a
        // start that fell back to the project's default would show here.
        let project = panel.read_with(&cx, |panel, _cx| panel.project.clone());
        let fs = cx.update(|_window, cx| <dyn fs::Fs>::global(cx));
        fs.as_fake()
            .insert_tree("/second", json!({ "other.txt": "" }))
            .await;
        project
            .update(&mut cx, |project, cx| {
                project.create_worktree(Path::new("/second"), true, cx)
            })
            .await
            .expect("the second folder joins the project");
        cx.run_until_parked();
        assert_ne!(
            cx.update(|_window, cx| project.read(cx).default_path_list(cx)),
            started_on,
            "the project's default is no longer what the session was opened on"
        );

        let message_editor = crate::test_support::active_draft_editor(&panel, &cx);
        message_editor.update_in(&mut cx, |editor, window, cx| {
            editor.set_text("失敗しても消えない下書き", window, cx);
        });
        cx.focus(&message_editor);
        cx.run_until_parked();

        crate::test_support::fail_next_stub_connect("エージェントが見つかりません");
        message_editor.update_in(&mut cx, |_editor, window, cx| {
            window.dispatch_action(Box::new(zed_actions::agent::Chat), cx);
        });
        cx.run_until_parked();
        assert!(
            !stub_is_connected(&panel, &cx),
            "the agent did not come back"
        );

        // ADR-F36 Phase B: the thread is drawn under a cached pane, so a frame
        // that reuses the cache records no `debug_selector`.
        cx.update(|window, _cx| window.refresh());
        cx.run_until_parked();
        let retry = cx
            .debug_bounds("RETRY-AGENT-START")
            .expect("the card that says the start failed carries the way back");
        cx.simulate_click(retry.center(), Modifiers::default());
        cx.run_until_parked();

        assert!(
            panel.read_with(&cx, |panel, cx| panel.active_agent_thread(cx).is_some()),
            "trying again starts the agent"
        );
        assert_eq!(
            crate::test_support::active_draft_editor(&panel, &cx)
                .read_with(&cx, |editor, cx| editor.text(cx)),
            "失敗しても消えない下書き",
            "with the text that was in the box when the start failed"
        );
        assert_eq!(
            panel.read_with(&cx, |panel, cx| panel
                .active_agent_thread(cx)
                .expect("the draft is connected again")
                .read(cx)
                .work_dirs()
                .cloned()),
            Some(started_on),
            "and on the folders the session was on, not the project's new default"
        );
    }

    /// A draft with something typed in it is never let go of: its unsent
    /// prompt is the user's, and parking it would be a connection dropped for
    /// a thread the user is in the middle of writing.
    #[gpui::test]
    async fn test_a_draft_with_an_unsent_prompt_keeps_its_agent(cx: &mut TestAppContext) {
        let (panel, thread_id, mut cx) = setup_panel_showing_an_empty_draft(cx).await;
        crate::test_support::type_draft_prompt(&panel, "まだ送っていない下書き", &mut cx);

        cx.executor().advance_clock(Duration::from_secs(10 * 60));
        cx.run_until_parked();

        assert!(
            stub_is_connected(&panel, &cx),
            "a draft the user has written in keeps its agent"
        );
        assert!(
            !draft_view(&panel, &cx).read_with(&cx, |view, _cx| view.is_parked()),
            "and is not parked"
        );
        panel.read_with(&cx, |panel, cx| {
            assert_eq!(
                panel
                    .draft_thread
                    .as_ref()
                    .map(|draft| draft.read(cx).thread_id),
                Some(thread_id),
                "and stays in the new-thread slot with its text"
            );
        });
    }

    /// The regression the tab-removing version of this left behind: with the
    /// panel showing something else, letting go of the draft's agent used to
    /// take the draft's tab out of the panel in front of the user. Parking
    /// touches no tab and no pane.
    #[gpui::test]
    async fn test_a_draft_behind_another_panel_view_keeps_its_tab_when_its_agent_goes(
        cx: &mut TestAppContext,
    ) {
        let (panel, thread_id, mut cx) = setup_panel_showing_an_empty_draft(cx).await;
        cx.update(|_, cx| {
            cx.update_flags(true, vec!["agent-panel-terminal".to_string()]);
        });
        panel
            .update_in(&mut cx, |panel, window, cx| {
                panel.insert_test_terminal("別の表示", true, window, cx)
            })
            .expect("the panel can show a terminal instead");
        cx.run_until_parked();
        assert!(
            panel.read_with(&cx, |panel, _cx| panel.active_terminal_id().is_some()),
            "the panel is showing something other than the draft"
        );

        cx.executor().advance_clock(Duration::from_secs(10 * 60));
        cx.run_until_parked();

        assert!(!stub_is_connected(&panel, &cx));
        assert!(
            draft_view(&panel, &cx).read_with(&cx, |view, _cx| view.is_parked()),
            "the draft behind the terminal let its agent go"
        );
        assert!(
            cx.update(|_window, cx| ThreadItem::is_open_in_a_tab(thread_id, cx)),
            "without its tab vanishing from behind what the panel is showing"
        );
        panel.read_with(&cx, |panel, _cx| {
            assert!(
                panel.active_terminal_id().is_some(),
                "and without pulling the panel off what the user was looking at"
            );
        });
    }

    /// パネルのアクティブなペインに載っているタブの id (並び順のまま)。
    fn panel_tab_ids(panel: &Entity<AgentPanel>, cx: &VisualTestContext) -> Vec<EntityId> {
        panel.read_with(cx, |panel, cx| {
            panel
                .active_pane
                .read(cx)
                .items()
                .map(|item| item.item_id())
                .collect()
        })
    }

    fn activate_panel_tab(panel: &Entity<AgentPanel>, index: usize, cx: &mut VisualTestContext) {
        panel.update_in(cx, |panel, window, cx| {
            let pane = panel.active_pane.clone();
            pane.update(cx, |pane, cx| {
                pane.activate_item(index, true, true, window, cx)
            });
        });
        cx.run_until_parked();
    }

    /// ADR-F53: 公式 CLI はエージェントパネルのペインのタブで動く。スレッド
    /// ではないタブが前に出てもパネルは下書きへ戻らず (戻すとそのタブが裏へ
    /// 落ちる)、そのタブを描いたままにし、スレッドのタブへ戻れば追従する。
    #[gpui::test]
    async fn test_a_non_thread_tab_stays_in_front_and_the_panel_follows_it_back(
        cx: &mut TestAppContext,
    ) {
        let (panel, mut cx) = setup_visible_panel(cx).await;
        cx.run_until_parked();

        let thread_tabs = panel_tab_ids(&panel, &cx);
        assert_eq!(
            thread_tabs.len(),
            1,
            "パネルは最初に下書きのタブを 1 枚持つ"
        );
        let draft_view_id = panel.read_with(&cx, |panel, _cx| {
            panel
                .active_conversation_view()
                .expect("下書きが表に出ている")
                .entity_id()
        });

        let terminal_view = panel.update_in(&mut cx, |panel, window, cx| {
            panel.add_test_terminal_tab(true, window, cx)
        });
        cx.run_until_parked();

        let terminal_tab = terminal_view.entity_id();
        assert_eq!(
            panel_tab_ids(&panel, &cx),
            vec![thread_tabs[0], terminal_tab],
            "端末はスレッドのタブの隣に並ぶ"
        );
        panel.read_with(&cx, |panel, cx| {
            assert_eq!(
                panel.test_visible_pane_item_id(),
                Some(terminal_tab),
                "パネルは端末のタブを描いている"
            );
            assert!(
                panel.active_conversation_view().is_none(),
                "下書きへ戻されていない"
            );
            assert_eq!(
                panel
                    .active_pane
                    .read(cx)
                    .active_item()
                    .map(|i| i.item_id()),
                Some(terminal_tab),
                "端末のタブが裏へ押しやられていない"
            );
        });
        assert!(
            cx.update(|window, cx| panel
                .read(cx)
                .activation_focus_handle(cx)
                .contains_focused(window, cx)),
            "パネルを触ったときのフォーカス先も端末のタブ"
        );

        activate_panel_tab(&panel, 0, &mut cx);
        panel.read_with(&cx, |panel, _cx| {
            assert!(
                panel.test_visible_pane_item_id().is_none(),
                "スレッドのタブへ戻れば端末の表示は畳む"
            );
            assert_eq!(
                panel
                    .active_conversation_view()
                    .map(|view| view.entity_id()),
                Some(draft_view_id),
                "戻り先は元の下書き"
            );
        });

        activate_panel_tab(&panel, 1, &mut cx);
        panel.read_with(&cx, |panel, _cx| {
            assert_eq!(
                panel.test_visible_pane_item_id(),
                Some(terminal_tab),
                "往復しても追従する"
            );
        });
    }

    /// タブを閉じればセッションは終わる: パネルは残ったタブへ戻り、
    /// `TerminalView` (と、その先の `Terminal` = CLI のプロセス) は解放される。
    #[gpui::test]
    async fn test_closing_the_cli_tab_returns_the_panel_and_releases_the_terminal(
        cx: &mut TestAppContext,
    ) {
        let (panel, mut cx) = setup_visible_panel(cx).await;
        cx.run_until_parked();

        let terminal_view = panel.update_in(&mut cx, |panel, window, cx| {
            panel.add_test_terminal_tab(true, window, cx)
        });
        cx.run_until_parked();
        let terminal_tab = terminal_view.entity_id();
        let released = terminal_view.downgrade();
        let terminal = terminal_view.read_with(&cx, |view, _cx| view.terminal().downgrade());
        drop(terminal_view);

        let close = panel.update_in(&mut cx, |panel, window, cx| {
            let pane = panel.active_pane.clone();
            pane.update(cx, |pane, cx| {
                pane.close_item_by_id(terminal_tab, pane::SaveIntent::Skip, window, cx)
            })
        });
        close.await.expect("端末のタブは確認なしで閉じられる");
        cx.run_until_parked();

        panel.read_with(&cx, |panel, _cx| {
            assert!(
                panel.test_visible_pane_item_id().is_none(),
                "閉じたタブを描き続けない"
            );
            assert!(
                panel.active_conversation_view().is_some(),
                "残っているスレッドのタブへ戻る"
            );
        });
        released.assert_released();
        terminal.assert_released();
    }

    /// ADR-F53: 二重起動の「すでに実行中です」から既存のタブを前に出す探索は、
    /// タブがエージェントパネルにあっても、そこから中央のペインへ動かされて
    /// いても見つける。
    #[gpui::test]
    async fn test_a_cli_tab_is_found_wherever_the_user_moved_it(cx: &mut TestAppContext) {
        let (panel, mut cx) = setup_visible_panel(cx).await;
        cx.run_until_parked();
        let workspace = panel.read_with(&cx, |panel, _cx| panel.workspace.clone());

        let terminal_view = panel.update_in(&mut cx, |panel, window, cx| {
            panel.add_test_terminal_tab(true, window, cx)
        });
        cx.run_until_parked();
        let terminal = terminal_view.read_with(&cx, |view, _cx| view.terminal().clone());

        let found = workspace
            .read_with(&cx, |workspace, cx| {
                crate::sandboxed_claude_code::terminal_tab_location(workspace, &terminal, cx)
            })
            .expect("the workspace can be read");
        let (host, pane, index) = found.expect("パネルのタブが見つかる");
        assert_eq!(
            host,
            crate::sandboxed_claude_code::TerminalTabHost::AgentPanel
        );
        assert_eq!(
            pane.entity_id(),
            panel.read_with(&cx, |panel, _cx| panel.active_pane.entity_id()),
            "パネルのアクティブなペインのタブ"
        );
        assert_eq!(
            pane.read_with(&cx, |pane, _cx| pane
                .item_for_index(index)
                .map(|item| item.item_id())),
            Some(terminal_view.entity_id()),
            "その並び順のタブがこの端末"
        );

        // タブを中央のペインへ動かしても見つかる。
        let item = panel
            .update_in(&mut cx, |panel, window, cx| {
                let pane = panel.active_pane.clone();
                pane.update(cx, |pane, cx| pane.take_active_item(window, cx))
            })
            .expect("端末のタブを取り出せる");
        workspace
            .update_in(&mut cx, |workspace, window, cx| {
                workspace.add_item_to_active_pane(item, None, true, window, cx);
            })
            .expect("the workspace can be updated");
        cx.run_until_parked();

        let found = workspace
            .read_with(&cx, |workspace, cx| {
                crate::sandboxed_claude_code::terminal_tab_location(workspace, &terminal, cx)
            })
            .expect("the workspace can be read");
        let (host, _pane, _index) = found.expect("中央へ動かしたタブも見つかる");
        assert_eq!(
            host,
            crate::sandboxed_claude_code::TerminalTabHost::Elsewhere,
            "前面化はパネルではなくそのペインに任せる"
        );

        // 全ウィンドウ版は、呼び手がもう調べた `Workspace` を必ず飛ばす
        // (貸し出し中で読むと落ちる)。ウィンドウが 1 つならここで打ち止め。
        let workspace_id = workspace.entity_id();
        let found_elsewhere = cx.update(|_window, cx| {
            crate::sandboxed_claude_code::terminal_tab_location_elsewhere(
                &terminal,
                workspace_id,
                cx,
            )
        });
        assert!(
            found_elsewhere.is_none(),
            "呼び手の `Workspace` は二度見ない"
        );

        // 同じ `Workspace` の中で見つかるなら、説明もそのまま呼び手に出す。
        let already_running = crate::sandboxed_claude_code::test_already_running_notification_id();
        workspace
            .update_in(&mut cx, |workspace, window, cx| {
                crate::sandboxed_claude_code::test_reveal_terminal_tab(
                    workspace, &terminal, window, cx,
                );
            })
            .expect("the workspace can be updated");
        cx.run_until_parked();
        assert!(
            workspace
                .read_with(&cx, |workspace, _cx| workspace
                    .notification_ids()
                    .contains(&already_running))
                .expect("the workspace can be read"),
            "切り替え先が無いので説明は呼び手に出る"
        );
    }

    /// ADR-F53: スレッド以外のタブ (公式 CLI の端末) でも `agent::ToggleSearch`
    /// (Ctrl+F) が効く。スレッドのタブでは今までどおり何も出ない。
    #[gpui::test]
    async fn test_a_panel_tab_gets_the_panes_search_bar(cx: &mut TestAppContext) {
        let (panel, mut cx) = setup_visible_panel(cx).await;
        cx.run_until_parked();

        let search_bar_shown = |cx: &mut VisualTestContext| {
            panel.read_with(cx, |panel, cx| {
                panel
                    .active_pane
                    .read(cx)
                    .toolbar()
                    .read(cx)
                    .item_of_type::<BufferSearchBar>()
                    .map(|search_bar| !search_bar.read(cx).is_dismissed())
            })
        };
        let toggle_search = |cx: &mut VisualTestContext| {
            cx.update(|window, cx| {
                window.dispatch_action(crate::ToggleSearch.boxed_clone(), cx);
            });
            cx.run_until_parked();
        };

        assert_eq!(
            search_bar_shown(&mut cx),
            Some(false),
            "ペインには検索バーが載っているが、まだ畳まれている"
        );

        toggle_search(&mut cx);
        assert_eq!(
            search_bar_shown(&mut cx),
            Some(false),
            "スレッドのタブは検索できないので何も出さない"
        );

        panel.update_in(&mut cx, |panel, window, cx| {
            panel.add_test_terminal_tab(true, window, cx);
        });
        cx.run_until_parked();

        toggle_search(&mut cx);
        assert_eq!(
            search_bar_shown(&mut cx),
            Some(true),
            "CLI のタブでは、そのペインのツールバーの検索バーが出る"
        );
    }

    /// ADR-F53: タブを別のウィンドウへ引き出したあとの二重起動は、その
    /// ウィンドウを前に出してからタブを選ぶ。
    #[gpui::test]
    async fn test_revealing_a_cli_tab_that_moved_to_another_window(cx: &mut TestAppContext) {
        let (panel, mut cx) = setup_visible_panel(cx).await;
        cx.run_until_parked();
        let workspace_a = panel
            .read_with(&cx, |panel, _cx| panel.workspace.clone())
            .upgrade()
            .expect("パネルのワークスペース");

        let cli_view = panel.update_in(&mut cx, |panel, window, cx| {
            panel.add_test_terminal_tab(true, window, cx)
        });
        let other_view = panel.update_in(&mut cx, |panel, window, cx| {
            panel.add_test_terminal_tab(true, window, cx)
        });
        cx.run_until_parked();
        let cli_terminal = cli_view.read_with(&cx, |view, _cx| view.terminal().clone());
        let cli_item_id = cli_view.entity_id();
        let other_item_id = other_view.entity_id();

        // 2 つめのウィンドウは別のプロジェクトで開く。`Workspace::test_new` は
        // プロジェクトのクライアントにメッセージハンドラーを登録するので、
        // 同じプロジェクトで 2 枚作ると二重登録で落ちる。
        let fs_b = FakeFs::new(cx.executor());
        let project_b = Project::test(fs_b, [], &mut cx).await;
        let window_b =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project_b.clone(), window, cx));
        let workspace_b = window_b
            .read_with(&cx, |multi_workspace, _cx| {
                multi_workspace.workspace().clone()
            })
            .expect("2 つめのウィンドウのワークスペース");
        let mut cx_b = VisualTestContext::from_window(window_b.into(), &cx);

        // 端末のタブを 2 枚とも 2 つめのウィンドウへ引き出す。
        for _ in 0..2 {
            let item = panel
                .update_in(&mut cx, |panel, window, cx| {
                    let pane = panel.active_pane.clone();
                    pane.update(cx, |pane, cx| pane.take_active_item(window, cx))
                })
                .expect("端末のタブを取り出せる");
            workspace_b.update_in(&mut cx_b, |workspace, window, cx| {
                workspace.add_item_to_active_pane(item, None, true, window, cx);
            });
        }
        cx.run_until_parked();
        cx_b.run_until_parked();

        // 探している CLI のタブが選ばれていない状態から始める。
        let pane_b = workspace_b.read_with(&cx_b, |workspace, _cx| workspace.active_pane().clone());
        let other_index = pane_b
            .read_with(&cx_b, |pane, _cx| {
                pane.items()
                    .position(|item| item.item_id() == other_item_id)
            })
            .expect("もう 1 枚のタブ");
        pane_b.update_in(&mut cx_b, |pane, window, cx| {
            pane.activate_item(other_index, true, true, window, cx);
        });
        cx_b.run_until_parked();
        let active_item_id = |cx: &mut VisualTestContext| {
            pane_b.read_with(cx, |pane, _cx| {
                pane.active_item().map(|item| item.item_id())
            })
        };
        assert_eq!(active_item_id(&mut cx_b), Some(other_item_id));

        cx.update(|window, _cx| window.activate_window());
        cx.run_until_parked();
        let window_a = cx
            .update(|_window, cx| cx.active_window())
            .expect("1 つめのウィンドウが前に出ている");
        assert_ne!(window_a, gpui::AnyWindowHandle::from(window_b));

        workspace_a.update_in(&mut cx, |workspace, window, cx| {
            crate::sandboxed_claude_code::test_reveal_terminal_tab(
                workspace,
                &cli_terminal,
                window,
                cx,
            );
        });
        cx.run_until_parked();
        cx_b.run_until_parked();

        assert_eq!(
            cx.update(|_window, cx| cx.active_window()),
            Some(gpui::AnyWindowHandle::from(window_b)),
            "タブを抱えているウィンドウごと前に出す"
        );
        assert_eq!(
            active_item_id(&mut cx_b),
            Some(cli_item_id),
            "そのウィンドウで探していたタブが選ばれる"
        );

        let already_running = crate::sandboxed_claude_code::test_already_running_notification_id();
        let says_already_running = |workspace: &Entity<Workspace>, cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, _cx| {
                workspace.notification_ids().contains(&already_running)
            })
        };
        assert!(
            says_already_running(&workspace_b, &mut cx_b),
            "説明はタブが出るほうのウィンドウに出す"
        );
        assert!(
            !says_already_running(&workspace_a, &mut cx),
            "呼び手側に出すと前面化した窓の裏に隠れる"
        );

        // 探索は `already_searched` の `Workspace` を必ず飛ばす。窓が 2 枚
        // ある状態で確かめないと「そもそも見つからない」と区別できない。
        // ウィンドウを開いたまま読むので `VisualTestContext::update`
        // (対象の窓を貸し出す) ではなく `App` そのものを回す。
        let workspace_a_id = workspace_a.entity_id();
        let workspace_b_id = workspace_b.entity_id();
        let (found_from_a, found_from_b) = cx.cx.update(|cx| {
            let place_of = |already_searched| {
                crate::sandboxed_claude_code::terminal_tab_location_elsewhere(
                    &cli_terminal,
                    already_searched,
                    cx,
                )
                .map(|place| (place.workspace.entity_id(), place.pane.entity_id()))
            };
            (place_of(workspace_a_id), place_of(workspace_b_id))
        });
        assert_eq!(
            found_from_a,
            Some((workspace_b_id, pane_b.entity_id())),
            "呼び手のぶんだけ飛ばして、タブを抱えている `Workspace` を返す"
        );
        assert_eq!(found_from_b, None, "渡された `Workspace` は二度見ない");
    }

    /// ADR-F53: 1 つのウィンドウは抱えている `Workspace` のうち 1 つしか
    /// 映さない。タブがいま映していないほうにあるときは、そちらへ切り替えて
    /// からタブを選ぶ (切り替えないとタブは選ばれても見えない)。
    #[gpui::test]
    async fn test_revealing_a_cli_tab_in_a_workspace_the_window_is_not_showing(
        cx: &mut TestAppContext,
    ) {
        let (panel, mut cx) = setup_visible_panel(cx).await;
        cx.run_until_parked();
        let workspace_a = panel
            .read_with(&cx, |panel, _cx| panel.workspace.clone())
            .upgrade()
            .expect("パネルのワークスペース");

        let cli_view = panel.update_in(&mut cx, |panel, window, cx| {
            panel.add_test_terminal_tab(true, window, cx)
        });
        cx.run_until_parked();
        let cli_terminal = cli_view.read_with(&cx, |view, _cx| view.terminal().clone());
        let cli_item_id = cli_view.entity_id();

        let window_handle = cx
            .update(|window, _cx| window.window_handle())
            .downcast::<MultiWorkspace>()
            .expect("ウィンドウの根は `MultiWorkspace`");

        // 同じウィンドウに 2 つめの `Workspace` を足して、そちらを映す。
        // 1 つめを先に pin しないと、切り替えでウィンドウから外れてしまう。
        let fs_c = FakeFs::new(cx.executor());
        let project_c = Project::test(fs_c, [], &mut cx).await;
        let workspace_c = window_handle
            .update(&mut cx, |multi_workspace, window, cx| {
                multi_workspace.retain_active_workspace(cx);
                multi_workspace.test_add_workspace(project_c.clone(), window, cx)
            })
            .expect("2 つめのワークスペースを足せる");
        cx.run_until_parked();

        let displayed = |cx: &mut VisualTestContext| {
            window_handle
                .read_with(cx, |multi_workspace, _cx| {
                    multi_workspace.workspace().entity_id()
                })
                .expect("映しているワークスペース")
        };
        assert_eq!(
            displayed(&mut cx),
            workspace_c.entity_id(),
            "2 つめのワークスペースを映している"
        );
        assert!(
            window_handle
                .read_with(&cx, |multi_workspace, _cx| multi_workspace
                    .workspaces()
                    .any(|workspace| workspace == &workspace_a))
                .unwrap_or(false),
            "1 つめのワークスペースはタブごと残っている"
        );

        // 二重起動は「いま映しているワークスペース」から起きる。
        workspace_c.update_in(&mut cx, |workspace, window, cx| {
            crate::sandboxed_claude_code::test_reveal_terminal_tab(
                workspace,
                &cli_terminal,
                window,
                cx,
            );
        });
        cx.run_until_parked();

        assert_eq!(
            displayed(&mut cx),
            workspace_a.entity_id(),
            "タブを抱えているワークスペースへ切り替える"
        );
        assert_eq!(
            panel.read_with(&cx, |panel, cx| panel
                .active_pane
                .read(cx)
                .active_item()
                .map(|item| item.item_id())),
            Some(cli_item_id),
            "そのワークスペースで探していたタブが選ばれる"
        );

        let already_running = crate::sandboxed_claude_code::test_already_running_notification_id();
        let says_already_running = |workspace: &Entity<Workspace>, cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, _cx| {
                workspace.notification_ids().contains(&already_running)
            })
        };
        assert!(
            says_already_running(&workspace_a, &mut cx),
            "説明は切り替えた先の `Workspace` に出す"
        );
        assert!(
            !says_already_running(&workspace_c, &mut cx),
            "呼び手側に出すと切り替えた裏に残って見えない"
        );
    }

    /// 最後のタブが端末だったときは、いつもどおり下書きに戻る。
    #[gpui::test]
    async fn test_closing_the_last_tab_when_it_is_a_terminal_returns_to_a_draft(
        cx: &mut TestAppContext,
    ) {
        let (panel, mut cx) = setup_visible_panel(cx).await;
        cx.run_until_parked();

        let terminal_view = panel.update_in(&mut cx, |panel, window, cx| {
            panel.add_test_terminal_tab(true, window, cx)
        });
        cx.run_until_parked();
        let terminal_tab = terminal_view.entity_id();
        let released_view = terminal_view.downgrade();
        let released_terminal =
            terminal_view.read_with(&cx, |view, _cx| view.terminal().downgrade());
        drop(terminal_view);

        // 下書きのタブを先に閉じて、端末だけを残す。
        let thread_tabs = panel_tab_ids(&panel, &cx)
            .into_iter()
            .filter(|id| *id != terminal_tab)
            .collect::<Vec<_>>();
        for tab in thread_tabs {
            let close = panel.update_in(&mut cx, |panel, window, cx| {
                let pane = panel.active_pane.clone();
                pane.update(cx, |pane, cx| {
                    pane.close_item_by_id(tab, pane::SaveIntent::Skip, window, cx)
                })
            });
            close.await.expect("スレッドのタブを閉じられる");
            cx.run_until_parked();
        }
        assert_eq!(panel_tab_ids(&panel, &cx), vec![terminal_tab]);

        let close = panel.update_in(&mut cx, |panel, window, cx| {
            let pane = panel.active_pane.clone();
            pane.update(cx, |pane, cx| {
                pane.close_item_by_id(terminal_tab, pane::SaveIntent::Skip, window, cx)
            })
        });
        close.await.expect("端末のタブを閉じられる");
        cx.run_until_parked();

        panel.read_with(&cx, |panel, _cx| {
            assert!(
                panel.test_visible_pane_item_id().is_none(),
                "端末の表示は畳む"
            );
            assert!(
                panel.active_conversation_view().is_some(),
                "パネルは畳まず、いつもの下書きに戻る"
            );
        });
        assert_eq!(
            panel_tab_ids(&panel, &cx).len(),
            1,
            "戻った下書きがタブ 1 枚として載っている"
        );
        // ペインが一度空になる経路。閉じたタブをツールバーや `base_view` が
        // 握ったままだと CLI のプロセスが残るので、解放まで見る。
        released_view.assert_released();
        released_terminal.assert_released();
    }

    /// ADR-F53 + ADR-F09: CLI のタブをエージェントパネルに置いてよいのは、
    /// 利用者が AI を有効にしているときだけ。既定 (`agent.enabled = false`) では
    /// パネルは登録されたままなので、登録の有無だけで決めると opt-in していない
    /// パネルが勝手に開く。
    #[gpui::test]
    async fn test_the_cli_tab_only_goes_to_the_panel_when_the_agent_is_enabled(
        cx: &mut TestAppContext,
    ) {
        let (panel, mut cx) = setup_visible_panel(cx).await;
        cx.run_until_parked();
        let workspace = panel.read_with(&cx, |panel, _cx| panel.workspace.clone());

        let set_agent_enabled = |enabled: bool, cx: &mut VisualTestContext| {
            cx.update(|_window, cx| {
                AgentSettings::override_global(
                    AgentSettings {
                        enabled,
                        ..AgentSettings::get_global(cx).clone()
                    },
                    cx,
                );
            });
        };

        set_agent_enabled(true, &mut cx);
        assert_eq!(
            workspace
                .read_with(&cx, |workspace, cx| {
                    crate::sandboxed_claude_code::agent_panel_for_cli_tab(workspace, cx)
                        .map(|panel| panel.entity_id())
                })
                .expect("the workspace can be read"),
            Some(panel.entity_id()),
            "AI を有効にしていればパネルのタブへ"
        );

        set_agent_enabled(false, &mut cx);
        assert!(
            workspace
                .read_with(&cx, |workspace, cx| {
                    crate::sandboxed_claude_code::agent_panel_for_cli_tab(workspace, cx).is_none()
                })
                .expect("the workspace can be read"),
            "AI を有効にしていなければ (パネルが登録されていても) ターミナルパネルへ"
        );
    }

    /// パネルのペインが「いまタブバーを描くか」を、ペイン自身に持たせた判定に
    /// 訊く。パネルをリースしたままだと判定がパネルを読めないので、ハンドルを
    /// 取り出してからリースの外で訊く (描画も同じ順序で起きる)。
    fn pane_shows_tab_bar(panel: &Entity<AgentPanel>, cx: &mut VisualTestContext) -> bool {
        let pane = panel.read_with(cx, |panel, _cx| panel.active_pane.clone());
        cx.update(|window, cx| pane.update(cx, |pane, cx| pane.should_display_tab_bar(window, cx)))
    }

    fn override_agent_settings(
        cx: &mut VisualTestContext,
        update: impl FnOnce(&mut AgentSettings),
    ) {
        cx.update(|_window, cx| {
            let mut settings = AgentSettings::get_global(cx).clone();
            update(&mut settings);
            AgentSettings::override_global(settings, cx);
        });
    }

    /// ADR-F53: 縦の一覧が開いていて分割もしていないときは、横のタブ列は同じ
    /// ことを二度言うだけなので `auto` は隠す。分割・一覧を閉じた・幅が足りない
    /// のいずれかで戻る。
    #[gpui::test]
    async fn test_the_panel_tab_bar_follows_the_setting_and_the_layout(cx: &mut TestAppContext) {
        let (panel, mut cx) = setup_visible_panel(cx).await;
        cx.run_until_parked();

        assert!(
            !pane_shows_tab_bar(&panel, &mut cx),
            "auto: 一覧が出ていてペインが 1 つならタブバーは出さない"
        );

        override_agent_settings(&mut cx, |settings| settings.panel_thread_history = false);
        assert!(
            pane_shows_tab_bar(&panel, &mut cx),
            "auto: 一覧を閉じたらタブの行き先がタブバーしか無い"
        );

        override_agent_settings(&mut cx, |settings| settings.panel_thread_history = true);
        panel.update(&mut cx, |panel, _cx| {
            panel.measured_panel_width.set(Some(
                SLIM_PANEL_THREAD_HISTORY_WIDTH + MIN_THREAD_WIDTH_BESIDE_HISTORY - px(1.),
            ));
        });
        assert!(
            pane_shows_tab_bar(&panel, &mut cx),
            "auto: 幅が足りず一覧が引っ込んだときも同じ"
        );
        panel.update(&mut cx, |panel, _cx| {
            panel.measured_panel_width.set(None);
        });
        assert!(!pane_shows_tab_bar(&panel, &mut cx));

        let first_pane = panel.read_with(&cx, |panel, _cx| panel.active_pane.clone());
        panel.update_in(&mut cx, |panel, window, cx| {
            panel.split_pane(&first_pane, SplitDirection::Right, window, cx);
        });
        cx.run_until_parked();
        assert!(
            pane_shows_tab_bar(&panel, &mut cx),
            "auto: 分割したらどのペインを見ているかはタブでしか分からない"
        );

        override_agent_settings(&mut cx, |settings| {
            settings.panel_tab_bar = PanelTabBar::Never
        });
        assert!(
            !pane_shows_tab_bar(&panel, &mut cx),
            "never: 分割していても出さない"
        );
        override_agent_settings(&mut cx, |settings| settings.panel_thread_history = false);
        assert!(
            !pane_shows_tab_bar(&panel, &mut cx),
            "never: 一覧を閉じていても、ツールバーのボタンで呼び戻せるうちは出さない"
        );
        override_agent_settings(&mut cx, |settings| settings.panel_thread_history = true);
        panel.update(&mut cx, |panel, _cx| {
            panel.measured_panel_width.set(Some(
                SLIM_PANEL_THREAD_HISTORY_WIDTH + MIN_THREAD_WIDTH_BESIDE_HISTORY - px(1.),
            ));
        });
        assert!(
            pane_shows_tab_bar(&panel, &mut cx),
            "never: 一覧も開くボタンも出せない幅では、タブへ行く手段が他に無いので戻す"
        );
        panel.update(&mut cx, |panel, _cx| {
            panel.measured_panel_width.set(None);
        });

        override_agent_settings(&mut cx, |settings| {
            settings.panel_tab_bar = PanelTabBar::Always
        });
        assert!(pane_shows_tab_bar(&panel, &mut cx));
        override_agent_settings(&mut cx, |settings| settings.panel_thread_history = false);
        assert!(
            pane_shows_tab_bar(&panel, &mut cx),
            "always: 一覧の開閉に関わらず出す"
        );
    }

    /// `auto` は「規則」なので、トグルは今見えているものを裏返す値を書く。
    #[gpui::test]
    async fn test_toggling_the_tab_bar_writes_the_value_that_flips_it(cx: &mut TestAppContext) {
        let (panel, mut cx) = setup_visible_panel(cx).await;
        cx.run_until_parked();

        let fs = panel.read_with(&cx, |panel, _cx| panel.fs.clone());
        fs.create_dir(::paths::settings_file().parent().expect("設定ファイルの親"))
            .await
            .expect("設定ファイルの置き場を作れる");

        assert!(!pane_shows_tab_bar(&panel, &mut cx));
        panel.update_in(&mut cx, |panel, window, cx| {
            panel.toggle_panel_tab_bar(&TogglePanelTabBar, window, cx);
        });
        cx.run_until_parked();
        cx.update(|_window, cx| {
            assert_eq!(
                AgentSettings::get_global(cx).panel_tab_bar,
                PanelTabBar::Always,
                "隠れていたので「常に表示」を書く"
            );
        });
        assert!(pane_shows_tab_bar(&panel, &mut cx));

        panel.update_in(&mut cx, |panel, window, cx| {
            panel.toggle_panel_tab_bar(&TogglePanelTabBar, window, cx);
        });
        cx.run_until_parked();
        cx.update(|_window, cx| {
            assert_eq!(
                AgentSettings::get_global(cx).panel_tab_bar,
                PanelTabBar::Never,
                "見えていたので「表示しない」を書く"
            );
        });
        assert!(!pane_shows_tab_bar(&panel, &mut cx));
    }

    /// ADR-F53: 縦の一覧の「開いているタブ」の節には、スレッドのタブも
    /// スレッド以外のタブ (公式 CLI) も、パネルの端末スレッドも並ぶ。
    #[gpui::test]
    async fn test_the_history_lists_every_open_tab_and_switches_to_the_one_clicked(
        cx: &mut TestAppContext,
    ) {
        let (panel, mut cx) = setup_visible_panel(cx).await;
        cx.update(|_window, cx| {
            cx.update_flags(true, vec!["agent-panel-terminal".to_string()]);
        });
        cx.run_until_parked();

        let draft_tab = panel_tab_ids(&panel, &cx)
            .first()
            .copied()
            .expect("パネルは下書きのタブを 1 枚持つ");
        let cli_tab = panel
            .update_in(&mut cx, |panel, window, cx| {
                panel.add_test_terminal_tab(true, window, cx)
            })
            .entity_id();
        let panel_terminal = panel
            .update_in(&mut cx, |panel, window, cx| {
                panel.insert_test_terminal("パネルの端末", true, window, cx)
            })
            .expect("端末スレッドを作れる");
        cx.run_until_parked();

        let rows = cx.update(|window, cx| panel.read(cx).open_tab_rows(window, cx));
        assert_eq!(
            rows.iter().map(|row| row.id).collect::<Vec<_>>(),
            vec![
                OpenTabId::PaneItem(draft_tab),
                OpenTabId::PaneItem(cli_tab),
                OpenTabId::PanelTerminal(panel_terminal),
            ],
            "ペインのタブが並び順どおりに来て、タブを持たない端末スレッドが続く"
        );
        assert_eq!(
            rows.iter()
                .filter(|row| row.is_active)
                .map(|row| row.id)
                .collect::<Vec<_>>(),
            vec![OpenTabId::PanelTerminal(panel_terminal)],
            "強調されるのは今描いているものだけ"
        );
        assert_eq!(
            rows.iter()
                .map(|row| row.icon == Icon::new(IconName::Terminal))
                .collect::<Vec<_>>(),
            vec![false, true, true],
            "スレッドのタブだけエージェントのアイコンで、端末はどちらも端末のアイコン"
        );

        cx.update(|window, _cx| window.refresh());
        cx.run_until_parked();
        panel.read_with(&cx, |panel, cx| {
            assert_eq!(
                panel.thread_history.read(cx).open_tabs(),
                rows.as_slice(),
                "描画のたびに一覧へ渡している"
            );
        });
        let cli_row = cx
            .debug_bounds("PANEL_OPEN_TAB-1")
            .expect("一覧に CLI のタブの行が描かれている");
        assert!(cli_row.size.width > px(0.), "行に幅がある");
        cx.simulate_click(cli_row.center(), Modifiers::default());
        cx.run_until_parked();

        panel.read_with(&cx, |panel, _cx| {
            assert_eq!(
                panel.test_visible_pane_item_id(),
                Some(cli_tab),
                "行をクリックしたらそのタブが前に出る"
            );
        });
        let rows = cx.update(|window, cx| panel.read(cx).open_tab_rows(window, cx));
        assert_eq!(
            rows.iter()
                .filter(|row| row.is_active)
                .map(|row| row.id)
                .collect::<Vec<_>>(),
            vec![OpenTabId::PaneItem(cli_tab)],
            "強調も移る"
        );
    }

    /// タブが自前のアイコンを持っていれば、一覧の行もそれを出す (ADR-F53)。
    struct CustomIconItem(FocusHandle);

    impl workspace::Item for CustomIconItem {
        type Event = ();

        fn include_in_nav_history() -> bool {
            false
        }

        fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
            "カスタムのタブ".into()
        }

        fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
            Some(Icon::new(IconName::GitBranch))
        }
    }

    impl EventEmitter<()> for CustomIconItem {}

    impl Focusable for CustomIconItem {
        fn focus_handle(&self, _cx: &App) -> FocusHandle {
            self.0.clone()
        }
    }

    impl Render for CustomIconItem {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    #[gpui::test]
    async fn test_an_open_tab_row_uses_the_items_own_icon(cx: &mut TestAppContext) {
        let (panel, mut cx) = setup_visible_panel(cx).await;
        cx.run_until_parked();

        let item = cx.new(|cx| CustomIconItem(cx.focus_handle()));
        let item_id = item.entity_id();
        panel.update_in(&mut cx, |panel, window, cx| {
            let pane = panel.active_pane.clone();
            pane.update(cx, |pane, cx| {
                pane.add_item(Box::new(item), true, true, None, window, cx);
            });
        });
        cx.run_until_parked();

        let rows = cx.update(|window, cx| panel.read(cx).open_tab_rows(window, cx));
        let row = rows
            .iter()
            .find(|row| row.id == OpenTabId::PaneItem(item_id))
            .expect("そのタブの行がある");
        assert_eq!(
            row.icon,
            Icon::new(IconName::GitBranch),
            "行のアイコンはタブが `Item::tab_icon` で出すもの"
        );
    }

    /// パネルは毎フレーム行を組み直して一覧へ渡す (`set_open_tabs`) ので、
    /// 中身が同じなら等しくなければならない。等しくないと短絡が効かず、
    /// `notify` → 再描画 → また `notify` が止まらなくなる (ADR-F53)。
    #[gpui::test]
    async fn test_rebuilding_the_open_tab_rows_does_not_keep_redrawing(cx: &mut TestAppContext) {
        let (panel, mut cx) = setup_visible_panel(cx).await;
        cx.run_until_parked();
        panel.update_in(&mut cx, |panel, window, cx| {
            panel.add_test_terminal_tab(true, window, cx);
        });
        cx.run_until_parked();

        let first = cx.update(|window, cx| panel.read(cx).open_tab_rows(window, cx));
        assert!(
            first
                .iter()
                .any(|row| row.icon != Icon::new(IconName::Terminal)),
            "端末以外のアイコンの行もある = アイコンごと比べている"
        );

        let history = panel.read_with(&cx, |panel, _cx| panel.thread_history.clone());
        let notifications = std::rc::Rc::new(std::cell::Cell::new(0usize));
        let subscription = cx.update(|_window, cx| {
            let notifications = notifications.clone();
            cx.observe(&history, move |_, _| {
                notifications.set(notifications.get() + 1);
            })
        });
        cx.run_until_parked();

        // パネルが描くたびに組み直した行。一覧はもう `first` を持っている。
        let rebuilt = cx.update(|window, cx| panel.read(cx).open_tab_rows(window, cx));
        assert_eq!(rebuilt, first, "同じ状態なら行はアイコンごと等しい");
        history.update(&mut cx, |history, cx| {
            history.set_open_tabs(rebuilt.clone(), cx);
        });
        cx.run_until_parked();
        assert_eq!(
            notifications.get(),
            0,
            "何も変わっていないフレームで一覧を作り直させない"
        );

        let mut changed = rebuilt;
        changed.first_mut().expect("行がある").is_active ^= true;
        history.update(&mut cx, |history, cx| {
            history.set_open_tabs(changed, cx);
        });
        cx.run_until_parked();
        assert_eq!(notifications.get(), 1, "本当に変わったときは作り直させる");
        drop(subscription);
    }

    /// 行の × はペインの通常の経路で閉じる: アイテムは解放される。
    #[gpui::test]
    async fn test_closing_an_open_tab_row_releases_the_item(cx: &mut TestAppContext) {
        let (panel, mut cx) = setup_visible_panel(cx).await;
        cx.run_until_parked();

        let terminal_view = panel.update_in(&mut cx, |panel, window, cx| {
            panel.add_test_terminal_tab(true, window, cx)
        });
        cx.run_until_parked();
        let cli_tab = terminal_view.entity_id();
        let released = terminal_view.downgrade();
        let terminal = terminal_view.read_with(&cx, |view, _cx| view.terminal().downgrade());
        drop(terminal_view);

        let history = panel.read_with(&cx, |panel, _cx| panel.thread_history.clone());
        history.update(&mut cx, |_history, cx| {
            cx.emit(PanelThreadHistoryEvent::CloseOpenTab(OpenTabId::PaneItem(
                cli_tab,
            )));
        });
        cx.run_until_parked();

        assert!(
            !panel_tab_ids(&panel, &cx).contains(&cli_tab),
            "閉じたタブは一覧からも消える"
        );
        released.assert_released();
        terminal.assert_released();
    }

    /// ADR-F53: スレッド以外を見ているときも縦の一覧は隣にいる。
    #[gpui::test]
    async fn test_the_history_is_drawn_beside_every_surface(cx: &mut TestAppContext) {
        let (panel, mut cx) = setup_visible_panel(cx).await;
        cx.update(|_window, cx| {
            cx.update_flags(true, vec!["agent-panel-terminal".to_string()]);
        });
        cx.run_until_parked();

        let history_width = |cx: &mut VisualTestContext| {
            cx.update(|window, _cx| window.refresh());
            cx.run_until_parked();
            cx.debug_bounds("PANEL_THREAD_HISTORY")
                .map(|bounds| bounds.size.width)
        };

        assert_eq!(
            history_width(&mut cx),
            Some(PANEL_THREAD_HISTORY_WIDTH),
            "スレッドの隣"
        );

        panel.update_in(&mut cx, |panel, window, cx| {
            panel.add_test_terminal_tab(true, window, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            history_width(&mut cx),
            Some(PANEL_THREAD_HISTORY_WIDTH),
            "スレッド以外のタブ (公式 CLI) の隣にも"
        );

        panel
            .update_in(&mut cx, |panel, window, cx| {
                panel.insert_test_terminal("パネルの端末", true, window, cx)
            })
            .expect("端末スレッドを作れる");
        cx.run_until_parked();
        assert_eq!(
            history_width(&mut cx),
            Some(PANEL_THREAD_HISTORY_WIDTH),
            "全面の端末スレッドの隣にも"
        );

        override_agent_settings(&mut cx, |settings| settings.panel_thread_history = false);
        assert_eq!(history_width(&mut cx), None, "閉じれば描かない");
        override_agent_settings(&mut cx, |settings| settings.panel_thread_history = true);
        assert_eq!(history_width(&mut cx), Some(PANEL_THREAD_HISTORY_WIDTH));

        // 幅は実測 (`render_width_probe`) なので、ウィンドウごと狭めて確かめる。
        cx.simulate_resize(size(
            SLIM_PANEL_THREAD_HISTORY_WIDTH + MIN_THREAD_WIDTH_BESIDE_HISTORY - px(1.),
            px(700.),
        ));
        assert_eq!(
            history_width(&mut cx),
            None,
            "両方を並べられない幅では引っ込む"
        );
    }

    fn refreshed_bounds(
        cx: &mut VisualTestContext,
        selector: &'static str,
    ) -> Option<Bounds<Pixels>> {
        cx.update(|window, _cx| window.refresh());
        cx.run_until_parked();
        cx.debug_bounds(selector)
    }

    /// 一覧を閉じているあいだは、ツールバーの左端のボタンが戻り道になる
    /// (ADR-F53)。右側の同じトグルはそのあいだ隠れて、導線は 1 つだけ。
    #[gpui::test]
    async fn test_the_collapsed_history_leaves_a_toolbar_button_that_brings_it_back(
        cx: &mut TestAppContext,
    ) {
        let (panel, mut cx) = setup_visible_panel(cx).await;
        cx.run_until_parked();

        assert_eq!(
            refreshed_bounds(&mut cx, "PANEL_THREAD_HISTORY_OPEN_BUTTON"),
            None,
            "一覧が出ているあいだは開くボタンを出さない"
        );
        assert!(
            cx.debug_bounds("PANEL_THREAD_HISTORY_TOGGLE").is_some(),
            "開いているあいだは右側のトグルがそのまま残る"
        );

        override_agent_settings(&mut cx, |settings| settings.panel_thread_history = false);
        let toolbar = refreshed_bounds(&mut cx, "AGENT_PANEL_TOOLBAR").expect("ツールバー");
        let open_button = cx
            .debug_bounds("PANEL_THREAD_HISTORY_OPEN_BUTTON")
            .expect("閉じたらツールバーに開くボタンが出る");
        assert!(
            open_button.left() - toolbar.left() < px(16.),
            "開くボタンはツールバーの左端 (« のあった側) に置く: {open_button:?} / {toolbar:?}"
        );
        assert!(
            open_button.top() >= toolbar.top() && open_button.bottom() <= toolbar.bottom(),
            "開くボタンはツールバーの行に収まる"
        );
        assert_eq!(
            cx.debug_bounds("PANEL_THREAD_HISTORY_TOGGLE"),
            None,
            "閉じているあいだは右側のトグルを出さない (導線は左端の 1 つ)"
        );

        cx.simulate_resize(size(
            SLIM_PANEL_THREAD_HISTORY_WIDTH + MIN_THREAD_WIDTH_BESIDE_HISTORY - px(1.),
            px(700.),
        ));
        assert_eq!(
            refreshed_bounds(&mut cx, "PANEL_THREAD_HISTORY_OPEN_BUTTON"),
            None,
            "押しても一覧が出ない幅では開くボタンも出さない"
        );
        panel.read_with(&cx, |panel, cx| {
            assert!(!panel.thread_history_open_button_visible(cx));
        });
        assert!(
            cx.debug_bounds("PANEL_THREAD_HISTORY_TOGGLE").is_some(),
            "そのときは右側のトグルが設定の切り替え口として戻る"
        );
    }

    /// 一覧のヘッダーの « と、閉じたあとにツールバーの左端に出るボタンは、
    /// 右側のトグルと同じ設定値を書いて往復する (ADR-F53)。
    #[gpui::test]
    async fn test_the_collapse_button_and_the_open_button_make_a_round_trip(
        cx: &mut TestAppContext,
    ) {
        let (panel, mut cx) = setup_visible_panel(cx).await;
        cx.run_until_parked();

        let fs = panel.read_with(&cx, |panel, _cx| panel.fs.clone());
        fs.create_dir(::paths::settings_file().parent().expect("設定ファイルの親"))
            .await
            .expect("設定ファイルの置き場を作れる");
        assert!(
            refreshed_bounds(&mut cx, "PANEL_THREAD_HISTORY").is_some(),
            "はじめは一覧が出ている"
        );

        let history = panel.read_with(&cx, |panel, _cx| panel.thread_history.clone());
        history.update(&mut cx, |_history, cx| {
            cx.emit(PanelThreadHistoryEvent::Collapse);
        });
        cx.run_until_parked();
        cx.update(|_window, cx| {
            assert!(
                !AgentSettings::get_global(cx).panel_thread_history,
                "« はツールバーのボタンと同じ設定値を閉じる側に書く"
            );
        });
        assert_eq!(
            refreshed_bounds(&mut cx, "PANEL_THREAD_HISTORY"),
            None,
            "一覧そのものは消える"
        );
        let open_button = cx
            .debug_bounds("PANEL_THREAD_HISTORY_OPEN_BUTTON")
            .expect("代わりにツールバーの左端に開くボタンが出る");

        cx.simulate_click(open_button.center(), Modifiers::default());
        cx.run_until_parked();
        cx.update(|_window, cx| {
            assert!(
                AgentSettings::get_global(cx).panel_thread_history,
                "開くボタンを押すと一覧が戻る"
            );
        });
        assert!(refreshed_bounds(&mut cx, "PANEL_THREAD_HISTORY").is_some());
        assert_eq!(
            cx.debug_bounds("PANEL_THREAD_HISTORY_OPEN_BUTTON"),
            None,
            "戻ったら開くボタンは引っ込む"
        );
    }

    /// 閉じた一覧の跡に帯を残さない: 本文の左端はドックの線 (= パネルの
    /// 左端) に揃い、戻り道はどのサーフェスでもツールバーの左端に残る。
    #[gpui::test]
    async fn test_the_collapsed_history_leaves_no_gap_beside_any_surface(cx: &mut TestAppContext) {
        let (panel, mut cx) = setup_visible_panel(cx).await;
        cx.update(|_window, cx| {
            cx.update_flags(true, vec!["agent-panel-terminal".to_string()]);
        });
        cx.run_until_parked();
        override_agent_settings(&mut cx, |settings| settings.panel_thread_history = false);

        assert!(
            refreshed_bounds(&mut cx, "PANEL_THREAD_HISTORY_OPEN_BUTTON").is_some(),
            "スレッド"
        );

        panel.update_in(&mut cx, |panel, window, cx| {
            panel.add_test_terminal_tab(true, window, cx);
        });
        cx.run_until_parked();
        assert!(
            refreshed_bounds(&mut cx, "PANEL_THREAD_HISTORY_OPEN_BUTTON").is_some(),
            "スレッド以外のタブ (公式 CLI)"
        );

        panel
            .update_in(&mut cx, |panel, window, cx| {
                panel.insert_test_terminal("パネルの端末", true, window, cx)
            })
            .expect("端末スレッドを作れる");
        cx.run_until_parked();
        assert!(
            refreshed_bounds(&mut cx, "PANEL_THREAD_HISTORY_OPEN_BUTTON").is_some(),
            "全面の端末スレッド"
        );

        let toolbar = cx.debug_bounds("AGENT_PANEL_TOOLBAR").expect("ツールバー");
        let surface = cx
            .debug_bounds("PANEL_TERMINAL_SURFACE")
            .expect("全面の端末");
        assert_eq!(
            surface.left(),
            toolbar.left(),
            "閉じているあいだは本文がパネルの左端から始まる"
        );
        assert_eq!(surface.size.width, toolbar.size.width);

        override_agent_settings(&mut cx, |settings| settings.panel_thread_history = true);
        let surface = refreshed_bounds(&mut cx, "PANEL_TERMINAL_SURFACE").expect("全面の端末");
        assert_eq!(
            surface.left(),
            toolbar.left() + PANEL_THREAD_HISTORY_WIDTH,
            "開いているときは一覧のぶんだけ右にずれる (測り方の確認)"
        );
    }

    /// 左端の開くボタンのぶん、残りの中身は縮んでツールバーからはみ出さない。
    /// `max_content_width` の中央寄せは、ボタンを除いた残りの幅の中で効く。
    #[gpui::test]
    async fn test_the_open_button_keeps_the_toolbar_content_inside_the_toolbar(
        cx: &mut TestAppContext,
    ) {
        let (_panel, mut cx) = setup_visible_panel(cx).await;
        cx.run_until_parked();
        override_agent_settings(&mut cx, |settings| {
            settings.panel_thread_history = false;
            settings.max_content_width = None;
        });

        let toolbar = refreshed_bounds(&mut cx, "AGENT_PANEL_TOOLBAR").expect("ツールバー");
        let open_button = cx
            .debug_bounds("PANEL_THREAD_HISTORY_OPEN_BUTTON")
            .expect("閉じたら開くボタンが出る");
        let content = cx
            .debug_bounds("AGENT_PANEL_TOOLBAR_CONTENT")
            .expect("ツールバーの中身");
        assert!(
            content.left() >= open_button.right(),
            "中身はボタンの右から始まる: {content:?} / {open_button:?}"
        );
        assert_eq!(
            content.right(),
            toolbar.right(),
            "中身はボタンのぶん縮み、右端はツールバーの右端に揃う: {content:?} / {toolbar:?}"
        );

        let max_width = px(260.);
        assert!(toolbar.size.width > max_width + open_button.size.width + px(40.));
        override_agent_settings(&mut cx, |settings| {
            settings.max_content_width = Some(max_width);
        });
        let open_button = refreshed_bounds(&mut cx, "PANEL_THREAD_HISTORY_OPEN_BUTTON")
            .expect("中央寄せのときも開くボタンが出る");
        let content = cx
            .debug_bounds("AGENT_PANEL_TOOLBAR_CONTENT")
            .expect("ツールバーの中身");
        assert!(
            open_button.left() - toolbar.left() < px(16.),
            "中央寄せでも開くボタンはパネルの左端に残る: {open_button:?} / {toolbar:?}"
        );
        assert_eq!(content.size.width, max_width);
        let left_margin = content.left() - open_button.right();
        let right_margin = toolbar.right() - content.right();
        assert!(
            (left_margin - right_margin).abs() < px(1.),
            "中身はボタンを除いた残りの幅の中央に寄る: {left_margin:?} / {right_margin:?}"
        );
    }

    async fn setup_panel(cx: &mut TestAppContext) -> (Entity<AgentPanel>, VisualTestContext) {
        init_test(cx);
        cx.update(|cx| {
            agent::ThreadStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);
        });

        let fs = FakeFs::new(cx.executor());
        cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
        fs.insert_tree("/project", json!({ "file.txt": "" })).await;
        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;

        let multi_workspace =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));

        let workspace = multi_workspace
            .read_with(cx, |mw, _cx| mw.workspace().clone())
            .unwrap();

        let mut cx = VisualTestContext::from_window(multi_workspace.into(), cx);

        let panel = workspace.update_in(&mut cx, |workspace, window, cx| {
            cx.new(|cx| AgentPanel::new(workspace, window, cx))
        });

        (panel, cx)
    }

    async fn setup_visible_panel(
        cx: &mut TestAppContext,
    ) -> (Entity<AgentPanel>, VisualTestContext) {
        setup_visible_panel_with_sidebar(cx, true).await
    }

    async fn setup_visible_panel_with_sidebar(
        cx: &mut TestAppContext,
        threads_list_active: bool,
    ) -> (Entity<AgentPanel>, VisualTestContext) {
        init_test(cx);
        cx.update(|cx| {
            agent::ThreadStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);
            AgentSettings::override_global(
                AgentSettings {
                    notify_when_agent_waiting: NotifyWhenAgentWaiting::PrimaryScreen,
                    ..AgentSettings::get_global(cx).clone()
                },
                cx,
            );
        });

        let fs = FakeFs::new(cx.executor());
        cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
        fs.insert_tree("/project", json!({ "file.txt": "" })).await;
        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;

        let multi_workspace =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));

        let workspace = multi_workspace
            .read_with(cx, |multi_workspace, _cx| {
                multi_workspace.workspace().clone()
            })
            .unwrap();

        let mut cx = VisualTestContext::from_window(multi_workspace.into(), cx);
        register_test_sidebar(threads_list_active, &mut cx);

        let panel = workspace.update_in(&mut cx, |workspace, window, cx| {
            let panel = cx.new(|cx| AgentPanel::new(workspace, window, cx));
            workspace.add_panel(panel.clone(), window, cx);
            workspace.focus_panel::<AgentPanel>(window, cx);
            panel
        });

        (panel, cx)
    }

    fn expected_terminal_drop_text(paths: &[PathBuf]) -> String {
        let mut text = String::new();
        for path in paths {
            text.push(' ');
            text.push_str(&shlex::try_quote(path.to_str().unwrap()).unwrap());
        }
        text.push(' ');
        text
    }

    #[gpui::test]
    async fn test_terminal_external_image_drop_writes_path(cx: &mut TestAppContext) {
        let (panel, mut cx) = setup_panel(cx).await;
        cx.update(|_, cx| {
            cx.update_flags(true, vec!["agent-panel-terminal".to_string()]);
        });

        let terminal_id = panel
            .update_in(&mut cx, |panel, window, cx| {
                panel.insert_test_terminal("Image Upload", true, window, cx)
            })
            .expect("test terminal should be inserted");
        cx.run_until_parked();

        let terminal = panel.read_with(&cx, |panel, cx| {
            panel
                .terminals
                .get(&terminal_id)
                .expect("terminal should remain in the panel")
                .view
                .read(cx)
                .terminal()
                .clone()
        });
        terminal.update(&mut cx, |terminal, _cx| terminal.take_input_log());

        let image_path = PathBuf::from("/tmp/dropped-image.png");
        panel.update_in(&mut cx, |panel, window, cx| {
            let external_paths = ExternalPaths(vec![image_path.clone()].into());
            panel.paste_external_paths_into_active_terminal(&external_paths, window, cx);
        });

        let mut input_log = terminal.update(&mut cx, |terminal, _cx| terminal.take_input_log());
        assert_eq!(input_log.len(), 1, "expected one write to the terminal");
        let written =
            String::from_utf8(input_log.remove(0)).expect("terminal write should be valid UTF-8");
        assert_eq!(
            written,
            expected_terminal_drop_text(std::slice::from_ref(&image_path))
        );
    }

    #[gpui::test]
    async fn test_terminal_external_paths_drop_handler_writes_image_path(cx: &mut TestAppContext) {
        let (panel, mut cx) = setup_panel(cx).await;
        cx.update(|_, cx| {
            cx.update_flags(true, vec!["agent-panel-terminal".to_string()]);
        });

        let terminal_id = panel
            .update_in(&mut cx, |panel, window, cx| {
                panel.insert_test_terminal("Image Upload", true, window, cx)
            })
            .expect("test terminal should be inserted");
        cx.run_until_parked();

        let terminal = panel.read_with(&cx, |panel, cx| {
            panel
                .terminals
                .get(&terminal_id)
                .expect("terminal should remain in the panel")
                .view
                .read(cx)
                .terminal()
                .clone()
        });
        terminal.update(&mut cx, |terminal, _cx| terminal.take_input_log());

        let image_path = PathBuf::from("/tmp/dropped-image.png");
        panel.update_in(&mut cx, |panel, window, cx| {
            let external_paths = ExternalPaths(vec![image_path.clone()].into());
            panel.handle_external_paths_drop(&external_paths, window, cx);
        });

        let mut input_log = terminal.update(&mut cx, |terminal, _cx| terminal.take_input_log());
        assert_eq!(input_log.len(), 1, "expected one write to the terminal");
        let written =
            String::from_utf8(input_log.remove(0)).expect("terminal write should be valid UTF-8");
        assert_eq!(
            written,
            expected_terminal_drop_text(std::slice::from_ref(&image_path))
        );
    }

    #[gpui::test]
    async fn test_external_file_drop_on_thread_does_not_paste_into_later_terminal(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        cx.update(|cx| {
            agent::ThreadStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);
            cx.update_flags(true, vec!["agent-panel-terminal".to_string()]);
        });

        let fs = FakeFs::new(cx.executor());
        cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
        fs.insert_tree("/project", json!({ "file.txt": "content" }))
            .await;
        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;

        let multi_workspace =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace
            .read_with(cx, |multi_workspace, _cx| {
                multi_workspace.workspace().clone()
            })
            .unwrap();
        let mut cx = VisualTestContext::from_window(multi_workspace.into(), cx);

        let panel = workspace.update_in(&mut cx, |workspace, window, cx| {
            let panel = cx.new(|cx| AgentPanel::new(workspace, window, cx));
            workspace.add_panel(panel.clone(), window, cx);
            panel
        });
        open_thread_with_connection(&panel, StubAgentConnection::new(), &mut cx);
        let thread_id = active_thread_id(&panel, &cx);

        let file_path = PathBuf::from("/project/file.txt");
        panel.update_in(&mut cx, |panel, window, cx| {
            let external_paths = ExternalPaths(vec![file_path.clone()].into());
            panel.handle_external_paths_drop(&external_paths, window, cx);
        });

        let terminal_id = panel
            .update_in(&mut cx, |panel, window, cx| {
                panel.insert_test_terminal("Drop Target", true, window, cx)
            })
            .expect("test terminal should be inserted");
        let terminal = panel.read_with(&cx, |panel, cx| {
            panel
                .terminals
                .get(&terminal_id)
                .expect("terminal should remain in the panel")
                .view
                .read(cx)
                .terminal()
                .clone()
        });
        terminal.update(&mut cx, |terminal, _cx| terminal.take_input_log());

        cx.run_until_parked();

        let input_log = terminal.update(&mut cx, |terminal, _cx| terminal.take_input_log());
        assert!(
            input_log.is_empty(),
            "thread drop completion should not write to the active terminal"
        );

        let expected_uri = MentionUri::File {
            abs_path: file_path,
        }
        .to_uri()
        .to_string();
        let expected_text = format!("[@file.txt]({expected_uri}) ");
        let actual_text = panel.read_with(&cx, |panel, cx| panel.editor_text(thread_id, cx));
        assert_eq!(actual_text.as_deref(), Some(expected_text.as_str()));
    }

    #[gpui::test]
    async fn test_terminal_entry_kind_controls_new_entry(cx: &mut TestAppContext) {
        let (panel, mut cx) = setup_panel(cx).await;
        panel.read_with(&cx, |panel, cx| {
            assert!(panel.project.read(cx).supports_terminal(cx));
            assert!(!panel.should_create_terminal_for_new_entry(cx));
        });

        let terminal_id = panel
            .update_in(&mut cx, |panel, window, cx| {
                panel.insert_test_terminal("Dev Server", true, window, cx)
            })
            .expect("test terminal should be inserted");
        cx.run_until_parked();

        panel.read_with(&cx, |panel, cx| {
            assert_eq!(panel.active_terminal_id(), Some(terminal_id));
            assert!(panel.has_terminal(terminal_id));
            assert!(panel.should_create_terminal_for_new_entry(cx));
            let terminals = panel.terminals(cx);
            assert_eq!(terminals.len(), 1);
            assert_eq!(terminals[0].title.as_ref(), "Dev Server");
        });

        panel.update_in(&mut cx, |panel, window, cx| {
            panel.activate_new_thread(false, AgentThreadSource::AgentPanel, window, cx);
        });
        cx.run_until_parked();

        panel.read_with(&cx, |panel, cx| {
            assert_eq!(panel.active_terminal_id(), None);
            assert!(panel.has_terminal(terminal_id));
            assert!(!panel.should_create_terminal_for_new_entry(cx));
        });
    }

    #[gpui::test]
    async fn test_skills_menu_entry_shows_manage_skills_shortcut(cx: &mut TestAppContext) {
        init_test(cx);
        cx.update(|cx| {
            let default_key_bindings = settings::KeymapFile::load_asset_allow_partial_failure(
                "keymaps/default-macos.json",
                cx,
            )
            .unwrap();
            cx.bind_keys(default_key_bindings);
            agent::ThreadStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);
        });

        let fs = FakeFs::new(cx.executor());
        cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
        fs.insert_tree("/project", json!({ "file.txt": "" })).await;
        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;

        let multi_workspace =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace
            .read_with(cx, |multi_workspace, _cx| {
                multi_workspace.workspace().clone()
            })
            .unwrap();
        let mut cx = VisualTestContext::from_window(multi_workspace.into(), cx);

        let panel = workspace.update_in(&mut cx, |workspace, window, cx| {
            let panel = cx.new(|cx| AgentPanel::new(workspace, window, cx));
            workspace.add_panel(panel.clone(), window, cx);
            panel
        });
        open_thread_with_connection(&panel, StubAgentConnection::new(), &mut cx);
        workspace.update_in(&mut cx, |workspace, window, cx| {
            workspace.focus_panel::<AgentPanel>(window, cx);
        });
        cx.run_until_parked();

        panel.update_in(&mut cx, |panel, window, cx| {
            panel.toggle_options_menu(&ToggleOptionsMenu, window, cx);
        });
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("MENU_ITEM-スキル").is_some(),
            "Skills menu item should be visible"
        );
        assert!(
            cx.debug_bounds("KEY_BINDING-l").is_some(),
            "Skills menu item should show the ManageSkills shortcut"
        );
    }

    #[gpui::test]
    async fn test_terminal_title_omits_placeholder_title(cx: &mut TestAppContext) {
        let (panel, mut cx) = setup_panel(cx).await;
        let terminal_id = panel
            .update_in(&mut cx, |panel, window, cx| {
                panel.insert_test_terminal("", true, window, cx)
            })
            .expect("test terminal should be inserted");
        cx.run_until_parked();

        panel.read_with(&cx, |panel, cx| {
            let terminals = panel.terminals(cx);
            assert_eq!(terminals.len(), 1);
            assert_eq!(terminals[0].title.as_ref(), "");
            let terminal = panel
                .terminals
                .get(&terminal_id)
                .expect("terminal should remain in the panel");
            assert_eq!(terminal.title(cx).as_ref(), "");
        });

        let terminal_view = panel.read_with(&cx, |panel, _cx| {
            panel
                .terminals
                .get(&terminal_id)
                .expect("terminal should remain in the panel")
                .view
                .clone()
        });
        let terminal_entity =
            terminal_view.read_with(&cx, |terminal_view, _cx| terminal_view.terminal().clone());
        terminal_entity.update(&mut cx, |_terminal, cx| {
            cx.emit(TerminalEvent::TitleChanged);
        });
        cx.run_until_parked();

        panel.read_with(&cx, |panel, cx| {
            let terminals = panel.terminals(cx);
            assert_eq!(terminals.len(), 1);
            assert_eq!(terminals[0].title.as_ref(), "");
            let terminal = panel
                .terminals
                .get(&terminal_id)
                .expect("terminal should remain in the panel");
            assert_eq!(terminal.title(cx).as_ref(), "");
        });

        terminal_entity.update(&mut cx, |terminal, cx| {
            terminal.breadcrumb_text = "Shell Breadcrumb".to_string();
            cx.emit(TerminalEvent::BreadcrumbsChanged);
        });
        cx.run_until_parked();

        panel.read_with(&cx, |panel, cx| {
            let terminals = panel.terminals(cx);
            assert_eq!(terminals.len(), 1);
            assert_eq!(terminals[0].title.as_ref(), "Shell Breadcrumb");
            let terminal = panel
                .terminals
                .get(&terminal_id)
                .expect("terminal should remain in the panel");
            assert_eq!(terminal.title(cx).as_ref(), "Shell Breadcrumb");
        });
    }

    #[gpui::test]
    async fn test_title_edit_affordance_matches_threads_and_terminals(cx: &mut TestAppContext) {
        let (panel, mut cx) = setup_panel(cx).await;

        panel.update_in(&mut cx, |panel, window, cx| {
            panel.activate_draft(false, AgentThreadSource::AgentPanel, window, cx);
        });
        cx.run_until_parked();

        panel.update_in(&mut cx, |panel, window, cx| {
            assert!(matches!(
                panel.visible_surface(),
                VisibleSurface::AgentThread(_)
            ));
            assert!(panel.should_show_title_edit(window, cx));
        });

        let terminal_id = panel
            .update_in(&mut cx, |panel, window, cx| {
                panel.insert_test_terminal("Dev Server", true, window, cx)
            })
            .expect("test terminal should be inserted");
        cx.run_until_parked();

        panel.update_in(&mut cx, |panel, window, cx| {
            assert!(matches!(
                panel.visible_surface(),
                VisibleSurface::Terminal(_)
            ));
            assert!(panel.should_show_title_edit(window, cx));

            panel.edit_terminal_title(terminal_id, window, cx);
            assert!(!panel.should_show_title_edit(window, cx));
        });
    }

    #[gpui::test]
    async fn test_restored_terminal_uses_metadata_title_until_shell_title_arrives(
        cx: &mut TestAppContext,
    ) {
        let (panel, mut cx) = setup_panel(cx).await;
        let terminal_id = TerminalId::new();
        let now = Utc::now();
        let metadata = TerminalThreadMetadata {
            terminal_id,
            title: "Persisted Shell Title".into(),
            custom_title: None,
            created_at: now,
            worktree_paths: WorktreePaths::from_folder_paths(&PathList::new(&[PathBuf::from(
                "/project",
            )])),
            remote_connection: None,
            working_directory: None,
        };

        panel.update_in(&mut cx, |panel, window, cx| {
            panel
                .restore_test_terminal(metadata, true, AgentThreadSource::Sidebar, None, window, cx)
                .expect("test terminal should be restored");
        });
        cx.run_until_parked();

        let terminal_view = panel.read_with(&cx, |panel, cx| {
            let terminals = panel.terminals(cx);
            assert_eq!(terminals.len(), 1);
            assert_eq!(terminals[0].title.as_ref(), "Persisted Shell Title");
            panel
                .terminals
                .get(&terminal_id)
                .expect("terminal should be restored")
                .view
                .clone()
        });

        let terminal_entity =
            terminal_view.read_with(&cx, |terminal_view, _cx| terminal_view.terminal().clone());
        terminal_entity.update(&mut cx, |terminal, cx| {
            terminal.breadcrumb_text = "Fresh Shell Title".to_string();
            cx.emit(TerminalEvent::BreadcrumbsChanged);
        });
        cx.run_until_parked();

        panel.read_with(&cx, |panel, cx| {
            let terminals = panel.terminals(cx);
            assert_eq!(terminals.len(), 1);
            assert_eq!(terminals[0].title.as_ref(), "Fresh Shell Title");
        });
    }

    #[gpui::test]
    async fn test_restored_terminal_selects_without_focusing(cx: &mut TestAppContext) {
        let (panel, mut cx) = setup_panel(cx).await;
        let terminal_id = TerminalId::new();
        let now = Utc::now();
        let metadata = TerminalThreadMetadata {
            terminal_id,
            title: "Persisted Shell Title".into(),
            custom_title: None,
            created_at: now,
            worktree_paths: WorktreePaths::from_folder_paths(&PathList::new(&[PathBuf::from(
                "/project",
            )])),
            remote_connection: None,
            working_directory: None,
        };

        panel.update_in(&mut cx, |panel, window, cx| {
            panel
                .restore_test_terminal(
                    metadata,
                    false,
                    AgentThreadSource::Sidebar,
                    None,
                    window,
                    cx,
                )
                .expect("test terminal should be restored");
        });
        cx.run_until_parked();

        panel.read_with(&cx, |panel, _cx| {
            assert_eq!(panel.active_terminal_id(), Some(terminal_id));
        });
    }

    #[gpui::test]
    async fn test_terminal_working_directory_uses_active_workspace_while_workspace_is_updating(
        cx: &mut TestAppContext,
    ) {
        let (workspace, panel, mut cx) = setup_workspace_panel(cx).await;
        panel
            .update_in(&mut cx, |panel, window, cx| {
                panel.insert_test_terminal("Dev Server", false, window, cx)
            })
            .expect("test terminal should be inserted");
        cx.run_until_parked();

        panel.read_with(&cx, |panel, cx| {
            assert_eq!(panel.last_created_entry_kind, AgentPanelEntryKind::Terminal);
            assert!(panel.should_create_terminal_for_new_entry(cx));
        });

        workspace.update_in(&mut cx, |workspace, window, cx| {
            let panel = workspace
                .panel::<AgentPanel>(cx)
                .expect("agent panel should be registered in workspace");
            panel.read_with(cx, |panel, cx| {
                panel.terminal_working_directory(Some(workspace), cx);
            });
            workspace.focus_panel::<AgentPanel>(window, cx);
        });

        panel.read_with(&cx, |panel, cx| {
            assert_eq!(panel.last_created_entry_kind, AgentPanelEntryKind::Terminal);
            assert!(panel.should_create_terminal_for_new_entry(cx));
        });
    }

    #[gpui::test]
    async fn test_terminal_title_editor_is_created_only_while_editing(cx: &mut TestAppContext) {
        let (panel, mut cx) = setup_panel(cx).await;
        let terminal_id = panel
            .update_in(&mut cx, |panel, window, cx| {
                panel.insert_test_terminal("Dev Server", true, window, cx)
            })
            .expect("test terminal should be inserted");
        cx.run_until_parked();

        panel.read_with(&cx, |panel, _cx| {
            let terminal = panel
                .terminals
                .get(&terminal_id)
                .expect("terminal should remain in the panel");
            assert!(terminal.title_editor.is_none());
        });

        panel.update(&mut cx, |panel, cx| {
            panel.refresh_terminal_metadata(terminal_id, cx);
        });
        cx.run_until_parked();

        panel.read_with(&cx, |panel, _cx| {
            let terminal = panel
                .terminals
                .get(&terminal_id)
                .expect("terminal should remain in the panel");
            assert!(terminal.title_editor.is_none());
        });

        panel.update_in(&mut cx, |panel, window, cx| {
            panel.edit_terminal_title(terminal_id, window, cx);
        });
        cx.run_until_parked();

        panel.read_with(&cx, |panel, cx| {
            let terminal = panel
                .terminals
                .get(&terminal_id)
                .expect("terminal should remain in the panel");
            let title_editor = terminal
                .title_editor
                .as_ref()
                .expect("terminal title editor should be active while editing");
            assert_eq!(title_editor.read(cx).text(cx), "Dev Server");
        });

        panel.update_in(&mut cx, |panel, window, cx| {
            panel.stop_editing_terminal_title(terminal_id, false, window, cx);
        });
        cx.run_until_parked();

        panel.read_with(&cx, |panel, _cx| {
            let terminal = panel
                .terminals
                .get(&terminal_id)
                .expect("terminal should remain in the panel");
            assert!(terminal.title_editor.is_none());
        });
    }

    #[gpui::test]
    async fn test_terminal_title_editor_does_not_set_custom_title_when_unchanged(
        cx: &mut TestAppContext,
    ) {
        let (panel, mut cx) = setup_panel(cx).await;
        let terminal_id = panel
            .update_in(&mut cx, |panel, window, cx| {
                panel.insert_test_terminal("Initial Custom Title", true, window, cx)
            })
            .expect("test terminal should be inserted");
        cx.run_until_parked();

        let terminal_view = panel.read_with(&cx, |panel, _cx| {
            panel
                .terminals
                .get(&terminal_id)
                .expect("terminal should remain in the panel")
                .view
                .clone()
        });
        terminal_view.update(&mut cx, |terminal_view, cx| {
            terminal_view.set_custom_title(None, cx);
        });
        let terminal_entity =
            terminal_view.read_with(&cx, |terminal_view, _cx| terminal_view.terminal().clone());
        terminal_entity.update(&mut cx, |terminal, cx| {
            terminal.breadcrumb_text = "Shell Breadcrumb".to_string();
            cx.emit(TerminalEvent::BreadcrumbsChanged);
        });
        cx.run_until_parked();

        panel.read_with(&cx, |panel, cx| {
            let terminals = panel.terminals(cx);
            assert_eq!(terminals.len(), 1);
            assert_eq!(terminals[0].title.as_ref(), "Shell Breadcrumb");
        });

        panel.update_in(&mut cx, |panel, window, cx| {
            panel.edit_terminal_title(terminal_id, window, cx);
        });
        cx.run_until_parked();

        let title_editor = panel.read_with(&cx, |panel, cx| {
            let terminal = panel
                .terminals
                .get(&terminal_id)
                .expect("terminal should remain in the panel");
            let title_editor = terminal
                .title_editor
                .as_ref()
                .expect("terminal title editor should be active while editing")
                .clone();
            assert_eq!(title_editor.read(cx).text(cx), "Shell Breadcrumb");
            title_editor
        });

        panel.update_in(&mut cx, |panel, window, cx| {
            panel.handle_terminal_title_editor_event(
                terminal_id,
                &title_editor,
                &editor::EditorEvent::BufferEdited,
                window,
                cx,
            );
        });
        cx.run_until_parked();

        terminal_view.read_with(&cx, |terminal_view, _cx| {
            assert!(terminal_view.custom_title().is_none());
        });

        panel.update_in(&mut cx, |panel, window, cx| {
            panel.stop_editing_terminal_title(terminal_id, false, window, cx);
        });
        terminal_entity.update(&mut cx, |terminal, cx| {
            terminal.breadcrumb_text = "Updated Shell Breadcrumb".to_string();
            cx.emit(TerminalEvent::BreadcrumbsChanged);
        });
        cx.run_until_parked();

        panel.read_with(&cx, |panel, cx| {
            let terminals = panel.terminals(cx);
            assert_eq!(terminals.len(), 1);
            assert_eq!(terminals[0].title.as_ref(), "Updated Shell Breadcrumb");
        });
    }

    #[gpui::test]
    async fn test_terminal_custom_title_recomposes_with_live_spinner(cx: &mut TestAppContext) {
        let (panel, mut cx) = setup_panel(cx).await;
        let terminal_id = panel
            .update_in(&mut cx, |panel, window, cx| {
                panel.insert_test_terminal("Fix bug", true, window, cx)
            })
            .expect("test terminal should be inserted");
        cx.run_until_parked();

        let terminal_entity = panel.read_with(&cx, |panel, _cx| {
            panel
                .terminals
                .get(&terminal_id)
                .expect("terminal should remain in the panel")
                .view
                .clone()
        });
        let terminal_entity =
            terminal_entity.read_with(&cx, |terminal_view, _cx| terminal_view.terminal().clone());

        terminal_entity.update(&mut cx, |terminal, cx| {
            terminal.breadcrumb_text = "⠋ Thinking".to_string();
            cx.emit(TerminalEvent::BreadcrumbsChanged);
        });
        cx.run_until_parked();

        panel.read_with(&cx, |panel, cx| {
            let terminals = panel.terminals(cx);
            assert_eq!(terminals.len(), 1);
            assert_eq!(terminals[0].title.as_ref(), "⠋ Fix bug");
            let metadata = panel
                .terminal_metadata(terminal_id, cx)
                .expect("terminal metadata should be available");
            assert_eq!(metadata.title.as_ref(), "⠋ Thinking");
            assert_eq!(
                metadata.custom_title.as_ref().map(|title| title.as_ref()),
                Some("Fix bug")
            );
            assert_eq!(metadata.display_title().as_ref(), "⠋ Fix bug");
        });

        terminal_entity.update(&mut cx, |terminal, cx| {
            terminal.breadcrumb_text = "⠙ Thinking".to_string();
            cx.emit(TerminalEvent::BreadcrumbsChanged);
        });
        cx.run_until_parked();

        panel.read_with(&cx, |panel, cx| {
            let terminals = panel.terminals(cx);
            assert_eq!(terminals.len(), 1);
            assert_eq!(terminals[0].title.as_ref(), "⠙ Fix bug");
            let metadata = panel
                .terminal_metadata(terminal_id, cx)
                .expect("terminal metadata should be available");
            assert_eq!(metadata.title.as_ref(), "⠙ Thinking");
            assert_eq!(metadata.display_title().as_ref(), "⠙ Fix bug");
        });

        terminal_entity.update(&mut cx, |terminal, cx| {
            terminal.breadcrumb_text = "Thinking".to_string();
            cx.emit(TerminalEvent::BreadcrumbsChanged);
        });
        cx.run_until_parked();

        panel.read_with(&cx, |panel, cx| {
            let terminals = panel.terminals(cx);
            assert_eq!(terminals.len(), 1);
            assert_eq!(terminals[0].title.as_ref(), "Fix bug");
            let metadata = panel
                .terminal_metadata(terminal_id, cx)
                .expect("terminal metadata should be available");
            assert_eq!(metadata.title.as_ref(), "Thinking");
            assert_eq!(metadata.display_title().as_ref(), "Fix bug");
        });
    }

    #[gpui::test]
    async fn test_terminal_title_editor_excludes_spinner_prefix(cx: &mut TestAppContext) {
        let (panel, mut cx) = setup_panel(cx).await;
        let terminal_id = panel
            .update_in(&mut cx, |panel, window, cx| {
                panel.insert_test_terminal("Initial Custom Title", true, window, cx)
            })
            .expect("test terminal should be inserted");
        cx.run_until_parked();

        let terminal_view = panel.read_with(&cx, |panel, _cx| {
            panel
                .terminals
                .get(&terminal_id)
                .expect("terminal should remain in the panel")
                .view
                .clone()
        });
        terminal_view.update(&mut cx, |terminal_view, cx| {
            terminal_view.set_custom_title(None, cx);
        });
        let terminal_entity =
            terminal_view.read_with(&cx, |terminal_view, _cx| terminal_view.terminal().clone());
        terminal_entity.update(&mut cx, |terminal, cx| {
            terminal.breadcrumb_text = "⠋ Thinking".to_string();
            cx.emit(TerminalEvent::BreadcrumbsChanged);
        });
        cx.run_until_parked();

        panel.update_in(&mut cx, |panel, window, cx| {
            panel.edit_terminal_title(terminal_id, window, cx);
        });
        cx.run_until_parked();

        let title_editor = panel.read_with(&cx, |panel, cx| {
            let terminal = panel
                .terminals
                .get(&terminal_id)
                .expect("terminal should remain in the panel");
            let title_editor = terminal
                .title_editor
                .as_ref()
                .expect("terminal title editor should be active while editing")
                .clone();
            assert_eq!(title_editor.read(cx).text(cx), "Thinking");
            title_editor
        });

        title_editor.update_in(&mut cx, |editor, window, cx| {
            editor.set_text("Fix bug", window, cx);
            editor.focus_handle(cx).focus(window, cx);
        });
        panel.update_in(&mut cx, |panel, window, cx| {
            panel.handle_terminal_title_editor_event(
                terminal_id,
                &title_editor,
                &editor::EditorEvent::BufferEdited,
                window,
                cx,
            );
        });
        cx.run_until_parked();

        terminal_view.read_with(&cx, |terminal_view, _cx| {
            assert_eq!(terminal_view.custom_title(), Some("Fix bug"));
        });
        panel.read_with(&cx, |panel, cx| {
            let terminals = panel.terminals(cx);
            assert_eq!(terminals.len(), 1);
            assert_eq!(terminals[0].title.as_ref(), "⠋ Fix bug");
            let metadata = panel
                .terminal_metadata(terminal_id, cx)
                .expect("terminal metadata should be available");
            assert_eq!(metadata.title.as_ref(), "⠋ Thinking");
            assert_eq!(
                metadata.custom_title.as_ref().map(|title| title.as_ref()),
                Some("Fix bug")
            );
        });

        panel.update_in(&mut cx, |panel, window, cx| {
            panel.stop_editing_terminal_title(terminal_id, false, window, cx);
            panel.edit_terminal_title(terminal_id, window, cx);
        });
        cx.run_until_parked();

        panel.read_with(&cx, |panel, cx| {
            let terminal = panel
                .terminals
                .get(&terminal_id)
                .expect("terminal should remain in the panel");
            let title_editor = terminal
                .title_editor
                .as_ref()
                .expect("terminal title editor should be active while editing");
            assert_eq!(title_editor.read(cx).text(cx), "Fix bug");
        });
    }

    #[gpui::test]
    async fn test_terminal_bell_marks_and_activation_clears_notification(cx: &mut TestAppContext) {
        let (panel, mut cx) = setup_panel(cx).await;
        let first_terminal_id = panel
            .update_in(&mut cx, |panel, window, cx| {
                panel.insert_test_terminal("Build", true, window, cx)
            })
            .expect("first test terminal should be inserted");
        let second_terminal_id = panel
            .update_in(&mut cx, |panel, window, cx| {
                panel.insert_test_terminal("Server", true, window, cx)
            })
            .expect("second test terminal should be inserted");
        cx.run_until_parked();

        panel.read_with(&cx, |panel, _cx| {
            assert_eq!(panel.active_terminal_id(), Some(second_terminal_id));
        });

        panel.update(&mut cx, |panel, cx| {
            panel.emit_test_terminal_bell(first_terminal_id, cx);
        });
        cx.run_until_parked();

        panel.read_with(&cx, |panel, cx| {
            let first_terminal = panel
                .terminals(cx)
                .into_iter()
                .find(|terminal| terminal.id == first_terminal_id)
                .expect("first terminal should remain in the panel");
            assert!(first_terminal.has_notification);
        });

        panel.update_in(&mut cx, |panel, window, cx| {
            panel.activate_terminal(first_terminal_id, true, window, cx);
        });
        cx.run_until_parked();

        panel.read_with(&cx, |panel, cx| {
            let first_terminal = panel
                .terminals(cx)
                .into_iter()
                .find(|terminal| terminal.id == first_terminal_id)
                .expect("first terminal should remain in the panel");
            assert!(!first_terminal.has_notification);
        });
    }

    #[gpui::test]
    async fn test_visible_terminal_bell_is_suppressed(cx: &mut TestAppContext) {
        let (panel, mut cx) = setup_visible_panel(cx).await;
        let terminal_id = panel
            .update_in(&mut cx, |panel, window, cx| {
                panel.insert_test_terminal("Claude", true, window, cx)
            })
            .expect("test terminal should be inserted");
        cx.run_until_parked();

        cx.update(|window, cx| {
            assert!(window.is_window_active());
            assert!(panel.read(cx).focus_handle(cx).contains_focused(window, cx));
        });

        panel.update(&mut cx, |panel, cx| {
            panel.emit_test_terminal_bell(terminal_id, cx);
        });
        cx.run_until_parked();

        panel.read_with(&cx, |panel, cx| {
            let terminal = panel
                .terminals(cx)
                .into_iter()
                .find(|terminal| terminal.id == terminal_id)
                .expect("terminal should remain in the panel");
            assert!(!terminal.has_notification);
        });
        assert!(
            cx.windows()
                .iter()
                .all(|window| window.downcast::<AgentNotification>().is_none())
        );
    }

    #[gpui::test]
    async fn test_visible_terminal_bell_is_suppressed_without_focus(cx: &mut TestAppContext) {
        let (panel, mut cx) = setup_visible_panel(cx).await;
        let terminal_id = panel
            .update_in(&mut cx, |panel, window, cx| {
                panel.insert_test_terminal("Claude", true, window, cx)
            })
            .expect("test terminal should be inserted");
        cx.run_until_parked();

        let workspace = cx.update(|window, cx| {
            window
                .root::<MultiWorkspace>()
                .flatten()
                .expect("test window should have a MultiWorkspace root")
                .read(cx)
                .workspace()
                .clone()
        });
        workspace.update_in(&mut cx, |workspace, window, cx| {
            workspace.focus_handle(cx).focus(window, cx);
        });
        cx.update(|window, cx| {
            assert!(window.is_window_active());
            assert!(workspace.read(cx).focus_handle(cx).is_focused(window));
            assert!(!panel.read(cx).focus_handle(cx).contains_focused(window, cx));
        });

        panel.update(&mut cx, |panel, cx| {
            panel.emit_test_terminal_bell(terminal_id, cx);
        });
        cx.run_until_parked();

        panel.read_with(&cx, |panel, cx| {
            let terminal = panel
                .terminals(cx)
                .into_iter()
                .find(|terminal| terminal.id == terminal_id)
                .expect("terminal should remain in the panel");
            assert!(!terminal.has_notification);
        });
        assert!(
            cx.windows()
                .iter()
                .all(|window| window.downcast::<AgentNotification>().is_none())
        );
    }

    #[gpui::test]
    async fn test_terminal_bell_marks_without_popup_when_sidebar_open(cx: &mut TestAppContext) {
        let (panel, mut cx) = setup_visible_panel(cx).await;
        let first_terminal_id = panel
            .update_in(&mut cx, |panel, window, cx| {
                panel.insert_test_terminal("Build", true, window, cx)
            })
            .expect("first test terminal should be inserted");
        let second_terminal_id = panel
            .update_in(&mut cx, |panel, window, cx| {
                panel.insert_test_terminal("Server", true, window, cx)
            })
            .expect("second test terminal should be inserted");
        cx.run_until_parked();

        panel.read_with(&cx, |panel, _cx| {
            assert_eq!(panel.active_terminal_id(), Some(second_terminal_id));
        });
        cx.update(|window, cx| {
            let multi_workspace = window
                .root::<MultiWorkspace>()
                .flatten()
                .expect("test window should have a MultiWorkspace root");
            multi_workspace.update(cx, |multi_workspace, cx| {
                multi_workspace.open_sidebar(cx);
            });
        });
        cx.run_until_parked();

        panel.update(&mut cx, |panel, cx| {
            panel.emit_test_terminal_bell(first_terminal_id, cx);
        });
        cx.run_until_parked();

        panel.read_with(&cx, |panel, cx| {
            let first_terminal = panel
                .terminals(cx)
                .into_iter()
                .find(|terminal| terminal.id == first_terminal_id)
                .expect("first terminal should remain in the panel");
            assert!(first_terminal.has_notification);
        });
        assert!(
            cx.windows()
                .iter()
                .all(|window| window.downcast::<AgentNotification>().is_none())
        );
    }

    #[gpui::test]
    async fn test_terminal_bell_notifies_when_sidebar_history_open(cx: &mut TestAppContext) {
        let (panel, mut cx) = setup_visible_panel_with_sidebar(cx, false).await;
        let first_terminal_id = panel
            .update_in(&mut cx, |panel, window, cx| {
                panel.insert_test_terminal("Build", true, window, cx)
            })
            .expect("first test terminal should be inserted");
        let second_terminal_id = panel
            .update_in(&mut cx, |panel, window, cx| {
                panel.insert_test_terminal("Server", true, window, cx)
            })
            .expect("second test terminal should be inserted");
        cx.run_until_parked();

        panel.read_with(&cx, |panel, _cx| {
            assert_eq!(panel.active_terminal_id(), Some(second_terminal_id));
        });
        cx.update(|window, cx| {
            let multi_workspace = window
                .root::<MultiWorkspace>()
                .flatten()
                .expect("test window should have a MultiWorkspace root");
            multi_workspace.update(cx, |multi_workspace, cx| {
                multi_workspace.open_sidebar(cx);
            });
        });
        cx.run_until_parked();

        panel.update(&mut cx, |panel, cx| {
            panel.emit_test_terminal_bell(first_terminal_id, cx);
        });
        cx.run_until_parked();

        panel.read_with(&cx, |panel, cx| {
            let first_terminal = panel
                .terminals(cx)
                .into_iter()
                .find(|terminal| terminal.id == first_terminal_id)
                .expect("first terminal should remain in the panel");
            assert!(first_terminal.has_notification);
        });
        cx.windows()
            .iter()
            .find_map(|window| window.downcast::<AgentNotification>())
            .expect("terminal bell should notify when the sidebar thread list is hidden");
    }

    #[gpui::test]
    async fn test_terminal_notification_dismissed_when_sidebar_opens(cx: &mut TestAppContext) {
        let (panel, mut cx) = setup_visible_panel(cx).await;
        let first_terminal_id = panel
            .update_in(&mut cx, |panel, window, cx| {
                panel.insert_test_terminal("Build", true, window, cx)
            })
            .expect("first test terminal should be inserted");
        let second_terminal_id = panel
            .update_in(&mut cx, |panel, window, cx| {
                panel.insert_test_terminal("Server", true, window, cx)
            })
            .expect("second test terminal should be inserted");
        cx.run_until_parked();

        panel.read_with(&cx, |panel, _cx| {
            assert_eq!(panel.active_terminal_id(), Some(second_terminal_id));
        });
        panel.update(&mut cx, |panel, cx| {
            panel.emit_test_terminal_bell(first_terminal_id, cx);
        });
        cx.run_until_parked();

        cx.windows()
            .iter()
            .find_map(|window| window.downcast::<AgentNotification>())
            .expect("inactive terminal bell should show a notification");

        cx.update(|window, cx| {
            let multi_workspace = window
                .root::<MultiWorkspace>()
                .flatten()
                .expect("test window should have a MultiWorkspace root");
            multi_workspace.update(cx, |multi_workspace, cx| {
                multi_workspace.open_sidebar(cx);
            });
        });
        cx.run_until_parked();

        panel.read_with(&cx, |panel, cx| {
            let first_terminal = panel
                .terminals(cx)
                .into_iter()
                .find(|terminal| terminal.id == first_terminal_id)
                .expect("first terminal should remain in the panel");
            assert!(first_terminal.has_notification);
        });
        assert!(
            cx.windows()
                .iter()
                .all(|window| window.downcast::<AgentNotification>().is_none())
        );
    }

    #[gpui::test]
    async fn test_focused_terminal_bell_notifies_when_window_inactive(cx: &mut TestAppContext) {
        let (panel, mut cx) = setup_visible_panel(cx).await;
        let terminal_id = panel
            .update_in(&mut cx, |panel, window, cx| {
                panel.insert_test_terminal("Claude", true, window, cx)
            })
            .expect("test terminal should be inserted");
        cx.run_until_parked();

        cx.update(|window, cx| {
            assert!(window.is_window_active());
            assert!(panel.read(cx).focus_handle(cx).contains_focused(window, cx));
        });
        cx.deactivate_window();
        cx.update(|window, _cx| {
            assert!(!window.is_window_active());
        });

        panel.update(&mut cx, |panel, cx| {
            panel.emit_test_terminal_bell(terminal_id, cx);
        });
        cx.run_until_parked();

        panel.read_with(&cx, |panel, cx| {
            let terminal = panel
                .terminals(cx)
                .into_iter()
                .find(|terminal| terminal.id == terminal_id)
                .expect("terminal should remain in the panel");
            assert!(terminal.has_notification);
        });
        cx.windows()
            .iter()
            .find_map(|window| window.downcast::<AgentNotification>())
            .expect("background terminal bell should show a notification");
    }

    #[gpui::test]
    async fn test_active_terminal_notification_clears_when_window_reactivates(
        cx: &mut TestAppContext,
    ) {
        let (panel, mut cx) = setup_visible_panel(cx).await;
        let terminal_id = panel
            .update_in(&mut cx, |panel, window, cx| {
                panel.insert_test_terminal("Claude", true, window, cx)
            })
            .expect("test terminal should be inserted");
        cx.run_until_parked();

        cx.deactivate_window();
        panel.update(&mut cx, |panel, cx| {
            panel.emit_test_terminal_bell(terminal_id, cx);
        });
        cx.run_until_parked();

        panel.read_with(&cx, |panel, cx| {
            let terminal = panel
                .terminals(cx)
                .into_iter()
                .find(|terminal| terminal.id == terminal_id)
                .expect("terminal should remain in the panel");
            assert!(terminal.has_notification);
        });
        cx.windows()
            .iter()
            .find_map(|window| window.downcast::<AgentNotification>())
            .expect("background terminal bell should show a notification");

        cx.update(|window, _cx| {
            window.activate_window();
        });
        cx.run_until_parked();

        panel.read_with(&cx, |panel, cx| {
            let terminal = panel
                .terminals(cx)
                .into_iter()
                .find(|terminal| terminal.id == terminal_id)
                .expect("terminal should remain in the panel");
            assert!(!terminal.has_notification);
        });
        assert!(
            cx.windows()
                .iter()
                .all(|window| window.downcast::<AgentNotification>().is_none())
        );
    }

    #[gpui::test]
    async fn test_terminal_notification_dismissed_when_active_terminal_becomes_visible(
        cx: &mut TestAppContext,
    ) {
        let (panel, mut cx) = setup_panel(cx).await;
        cx.update(|_window, cx| {
            AgentSettings::override_global(
                AgentSettings {
                    notify_when_agent_waiting: NotifyWhenAgentWaiting::PrimaryScreen,
                    ..AgentSettings::get_global(cx).clone()
                },
                cx,
            );
        });
        let terminal_id = panel
            .update_in(&mut cx, |panel, window, cx| {
                panel.insert_test_terminal("Claude", true, window, cx)
            })
            .expect("test terminal should be inserted");
        cx.run_until_parked();

        panel.update(&mut cx, |panel, cx| {
            panel.emit_test_terminal_bell(terminal_id, cx);
        });
        cx.run_until_parked();

        panel.read_with(&cx, |panel, cx| {
            let terminal = panel
                .terminals(cx)
                .into_iter()
                .find(|terminal| terminal.id == terminal_id)
                .expect("terminal should remain in the panel");
            assert!(terminal.has_notification);
        });
        cx.windows()
            .iter()
            .find_map(|window| window.downcast::<AgentNotification>())
            .expect("hidden terminal bell should show a notification");

        let workspace = cx.update(|window, cx| {
            window
                .root::<MultiWorkspace>()
                .flatten()
                .expect("test window should have a MultiWorkspace root")
                .read(cx)
                .workspace()
                .clone()
        });
        workspace.update_in(&mut cx, |workspace, window, cx| {
            workspace.add_panel(panel.clone(), window, cx);
            workspace.focus_panel::<AgentPanel>(window, cx);
        });
        cx.run_until_parked();

        panel.read_with(&cx, |panel, cx| {
            let terminal = panel
                .terminals(cx)
                .into_iter()
                .find(|terminal| terminal.id == terminal_id)
                .expect("terminal should remain in the panel");
            assert!(!terminal.has_notification);
        });
        assert!(
            cx.windows()
                .iter()
                .all(|window| window.downcast::<AgentNotification>().is_none())
        );
    }

    #[gpui::test]
    async fn test_terminal_notification_closed_when_panel_dropped(cx: &mut TestAppContext) {
        let (panel, mut cx) = setup_panel(cx).await;
        cx.update(|_window, cx| {
            AgentSettings::override_global(
                AgentSettings {
                    notify_when_agent_waiting: NotifyWhenAgentWaiting::PrimaryScreen,
                    ..AgentSettings::get_global(cx).clone()
                },
                cx,
            );
        });
        let terminal_id = panel
            .update_in(&mut cx, |panel, window, cx| {
                panel.insert_test_terminal("Claude", true, window, cx)
            })
            .expect("test terminal should be inserted");
        let weak_panel = panel.downgrade();
        cx.run_until_parked();

        panel.update(&mut cx, |panel, cx| {
            panel.emit_test_terminal_bell(terminal_id, cx);
        });
        cx.run_until_parked();

        cx.windows()
            .iter()
            .find_map(|window| window.downcast::<AgentNotification>())
            .expect("hidden terminal bell should show a notification");

        drop(panel);
        cx.update(|_window, _cx| {});
        cx.run_until_parked();

        assert!(
            !weak_panel.is_upgradable(),
            "agent panel should be released after dropping the last handle"
        );
        assert!(
            cx.windows()
                .iter()
                .all(|window| window.downcast::<AgentNotification>().is_none())
        );
    }

    #[gpui::test]
    async fn test_terminal_notification_view_activates_terminal_workspace(cx: &mut TestAppContext) {
        init_test(cx);
        cx.update(|cx| {
            agent::ThreadStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);
            cx.update_flags(true, vec!["agent-panel-terminal".to_string()]);
            AgentSettings::override_global(
                AgentSettings {
                    notify_when_agent_waiting: NotifyWhenAgentWaiting::PrimaryScreen,
                    ..AgentSettings::get_global(cx).clone()
                },
                cx,
            );
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/project_a", json!({ "file.txt": "" }))
            .await;
        fs.insert_tree("/project_b", json!({ "file.txt": "" }))
            .await;
        let project_a = Project::test(fs.clone(), [Path::new("/project_a")], cx).await;
        let project_b = Project::test(fs, [Path::new("/project_b")], cx).await;

        let multi_workspace =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));
        let workspace_a = multi_workspace
            .read_with(cx, |multi_workspace, _cx| {
                multi_workspace.workspace().clone()
            })
            .unwrap();
        let workspace_b = multi_workspace
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.test_add_workspace(project_b.clone(), window, cx)
            })
            .unwrap();

        let cx = &mut VisualTestContext::from_window(multi_workspace.into(), cx);
        let panel_a = workspace_a.update_in(cx, |workspace, window, cx| {
            let panel = cx.new(|cx| AgentPanel::new(workspace, window, cx));
            workspace.add_panel(panel.clone(), window, cx);
            panel
        });

        let first_terminal_id = panel_a
            .update_in(cx, |panel, window, cx| {
                panel.insert_test_terminal("Build", true, window, cx)
            })
            .expect("first test terminal should be inserted");
        let second_terminal_id = panel_a
            .update_in(cx, |panel, window, cx| {
                panel.insert_test_terminal("Server", true, window, cx)
            })
            .expect("second test terminal should be inserted");
        cx.run_until_parked();

        multi_workspace
            .read_with(cx, |multi_workspace, _cx| {
                assert_eq!(multi_workspace.workspace(), &workspace_b);
            })
            .unwrap();
        panel_a.read_with(cx, |panel, _cx| {
            assert_eq!(panel.active_terminal_id(), Some(second_terminal_id));
        });

        panel_a.update(cx, |panel, cx| {
            panel.emit_test_terminal_bell(first_terminal_id, cx);
        });
        cx.run_until_parked();

        let notification = cx
            .windows()
            .iter()
            .find_map(|window| window.downcast::<AgentNotification>())
            .expect("terminal bell should show a notification");
        notification
            .update(cx, |notification, _window, cx| notification.accept(cx))
            .unwrap();
        assert!(
            cx.windows()
                .iter()
                .all(|window| window.downcast::<AgentNotification>().is_none())
        );
        cx.run_until_parked();

        multi_workspace
            .read_with(cx, |multi_workspace, _cx| {
                assert_eq!(multi_workspace.workspace(), &workspace_a);
            })
            .unwrap();
        panel_a.read_with(cx, |panel, cx| {
            assert_eq!(panel.active_terminal_id(), Some(first_terminal_id));
            let first_terminal = panel
                .terminals(cx)
                .into_iter()
                .find(|terminal| terminal.id == first_terminal_id)
                .expect("first terminal should remain in the panel");
            assert!(!first_terminal.has_notification);
        });
    }

    #[gpui::test]
    async fn test_running_thread_retained_when_navigating_away(cx: &mut TestAppContext) {
        let (panel, mut cx) = setup_panel(cx).await;

        let connection_a = StubAgentConnection::new();
        open_thread_with_connection(&panel, connection_a.clone(), &mut cx);
        send_message(&panel, &mut cx);

        let session_id_a = active_session_id(&panel, &cx);
        let thread_id_a = active_thread_id(&panel, &cx);

        // Send a chunk to keep thread A generating (don't end the turn).
        cx.update(|_, cx| {
            connection_a.send_update(
                session_id_a.clone(),
                acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new("chunk".into())),
                cx,
            );
        });
        cx.run_until_parked();

        // Verify thread A is generating.
        panel.read_with(&cx, |panel, cx| {
            let thread = panel.active_agent_thread(cx).unwrap();
            assert_eq!(thread.read(cx).status(), ThreadStatus::Generating);
            assert!((panel.test_background_thread_count() == 0));
        });

        // Open a new thread B — thread A should be retained in background.
        let connection_b = StubAgentConnection::new();
        open_thread_with_connection(&panel, connection_b, &mut cx);

        panel.read_with(&cx, |panel, _cx| {
            assert_eq!(
                panel.test_background_thread_count(),
                1,
                "Running thread A should be retained in retained_threads"
            );
            assert!(
                panel.test_thread_is_in_background(thread_id_a),
                "Retained thread should be keyed by thread A's thread ID"
            );
        });
    }

    #[gpui::test]
    async fn test_idle_non_loadable_thread_retained_when_navigating_away(cx: &mut TestAppContext) {
        let (panel, mut cx) = setup_panel(cx).await;

        let connection_a = StubAgentConnection::new();
        connection_a.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
            acp::ContentChunk::new("Response".into()),
        )]);
        open_thread_with_connection(&panel, connection_a, &mut cx);
        send_message(&panel, &mut cx);

        let weak_view_a = panel.read_with(&cx, |panel, _cx| {
            panel.active_conversation_view().unwrap().downgrade()
        });
        let thread_id_a = active_thread_id(&panel, &cx);

        // Thread A should be idle (auto-completed via set_next_prompt_updates).
        panel.read_with(&cx, |panel, cx| {
            let thread = panel.active_agent_thread(cx).unwrap();
            assert_eq!(thread.read(cx).status(), ThreadStatus::Idle);
        });

        // Open a new thread B — thread A should be retained because it is not loadable.
        let connection_b = StubAgentConnection::new();
        open_thread_with_connection(&panel, connection_b, &mut cx);

        panel.read_with(&cx, |panel, _cx| {
            assert_eq!(
                panel.test_background_thread_count(),
                1,
                "Idle non-loadable thread A should be retained in retained_threads"
            );
            assert!(
                panel.test_thread_is_in_background(thread_id_a),
                "Retained thread should be keyed by thread A's thread ID"
            );
        });

        assert!(
            weak_view_a.upgrade().is_some(),
            "Idle non-loadable ConnectionView should still be retained"
        );
    }

    #[gpui::test]
    async fn test_background_thread_promoted_via_load(cx: &mut TestAppContext) {
        let (panel, mut cx) = setup_panel(cx).await;

        let connection_a = StubAgentConnection::new();
        open_thread_with_connection(&panel, connection_a.clone(), &mut cx);
        send_message(&panel, &mut cx);

        let session_id_a = active_session_id(&panel, &cx);
        let thread_id_a = active_thread_id(&panel, &cx);

        // Keep thread A generating.
        cx.update(|_, cx| {
            connection_a.send_update(
                session_id_a.clone(),
                acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new("chunk".into())),
                cx,
            );
        });
        cx.run_until_parked();

        // Open thread B — thread A goes to background.
        let connection_b = StubAgentConnection::new();
        open_thread_with_connection(&panel, connection_b, &mut cx);
        send_message(&panel, &mut cx);

        let thread_id_b = active_thread_id(&panel, &cx);

        panel.read_with(&cx, |panel, _cx| {
            assert_eq!(panel.test_background_thread_count(), 1);
            assert!(panel.test_thread_is_in_background(thread_id_a));
        });

        // Load thread A back via load_agent_thread — should promote from background.
        panel.update_in(&mut cx, |panel, window, cx| {
            panel.load_agent_thread(
                panel.selected_agent(cx),
                thread_id_a,
                None,
                None,
                true,
                AgentThreadSource::AgentPanel,
                window,
                cx,
            );
        });

        // Thread A should now be the active view, promoted from background.
        let active_session = active_session_id(&panel, &cx);
        assert_eq!(
            active_session, session_id_a,
            "Thread A should be the active thread after promotion"
        );

        panel.read_with(&cx, |panel, _cx| {
            assert!(
                !panel.test_thread_is_in_background(thread_id_a),
                "Promoted thread A should no longer be in retained_threads"
            );
            assert!(
                panel.test_thread_is_in_background(thread_id_b),
                "Thread B (idle, non-loadable) should remain retained in retained_threads"
            );
        });
    }

    #[gpui::test]
    async fn test_reopening_visible_thread_keeps_thread_usable(cx: &mut TestAppContext) {
        let (panel, mut cx) = setup_panel(cx).await;
        cx.run_until_parked();

        panel.update(&mut cx, |panel, cx| {
            panel.connection_store.update(cx, |store, cx| {
                store.restart_connection(
                    Agent::NativeAgent,
                    Rc::new(StubAgentServer::new(SessionTrackingConnection::new())),
                    cx,
                );
            });
        });
        cx.run_until_parked();

        panel.update_in(&mut cx, |panel, window, cx| {
            panel.external_thread(
                Some(Agent::NativeAgent),
                None,
                None,
                None,
                None,
                true,
                AgentThreadSource::AgentPanel,
                window,
                cx,
            );
        });
        cx.run_until_parked();
        send_message(&panel, &mut cx);

        let session_id = active_session_id(&panel, &cx);

        panel.update_in(&mut cx, |panel, window, cx| {
            panel.open_thread(session_id.clone(), None, None, window, cx);
        });
        cx.run_until_parked();

        send_message(&panel, &mut cx);

        panel.read_with(&cx, |panel, cx| {
            let active_view = panel
                .active_conversation_view()
                .expect("visible conversation should remain open after reopening");
            let connected = active_view
                .read(cx)
                .as_connected()
                .expect("visible conversation should still be connected in the UI");
            assert!(
                !connected.has_thread_error(cx),
                "reopening an already-visible session should keep the thread usable"
            );
        });
    }

    #[gpui::test]
    async fn test_initial_content_for_thread_summary_uses_own_session_id(cx: &mut TestAppContext) {
        init_test(cx);
        cx.update(|cx| {
            agent::ThreadStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);
        });

        let source_session_id = acp::SessionId::new("source-thread-session");
        let source_title: SharedString = "Source Thread Title".into();
        let db_thread = agent::DbThread {
            title: source_title.clone(),
            messages: Vec::new(),
            updated_at: Utc::now(),
            detailed_summary: None,
            initial_project_snapshot: None,
            cumulative_token_usage: Default::default(),
            request_token_usage: HashMap::default(),
            model: None,
            profile: None,
            subagent_context: None,
            speed: None,
            thinking_enabled: false,
            thinking_effort: None,
            draft_prompt: None,
            ui_scroll_position: None,
            sandboxed_terminal_temp_dir: None,
            sandbox_grants: Default::default(),
            permission_judgements: Default::default(),
        };

        let thread_store = cx.update(|cx| ThreadStore::global(cx));
        thread_store
            .update(cx, |store, cx| {
                store.save_thread(
                    source_session_id.clone(),
                    db_thread,
                    PathList::default(),
                    cx,
                )
            })
            .await
            .expect("saving source thread should succeed");
        cx.run_until_parked();

        thread_store.read_with(cx, |store, _cx| {
            let entry = store
                .thread_from_session_id(&source_session_id)
                .expect("saved thread should be listed in the store");
            assert!(
                entry.parent_session_id.is_none(),
                "saved thread is a root thread with no parent session"
            );
        });

        let content = cx
            .update(|cx| {
                AgentPanel::initial_content_for_thread_summary(source_session_id.clone(), cx)
            })
            .expect("initial content should be produced for a root thread");

        match content {
            AgentInitialContent::ThreadSummary { session_id, title } => {
                assert_eq!(
                    session_id, source_session_id,
                    "thread-summary mention should use the source thread's own session id"
                );
                assert_eq!(title, Some(source_title.clone()));
            }
            _ => panic!("expected AgentInitialContent::ThreadSummary"),
        }

        // Unknown session ids should still produce no content.
        let missing = cx.update(|cx| {
            AgentPanel::initial_content_for_thread_summary(
                acp::SessionId::new("does-not-exist"),
                cx,
            )
        });
        assert!(
            missing.is_none(),
            "unknown session ids should not produce initial content"
        );
    }

    #[gpui::test]
    async fn test_cleanup_retained_threads_keeps_five_most_recent_idle_loadable_threads(
        cx: &mut TestAppContext,
    ) {
        let (panel, mut cx) = setup_panel(cx).await;
        let connection = StubAgentConnection::new()
            .with_supports_load_session(true)
            .with_agent_id("loadable-stub".into())
            .with_telemetry_id("loadable-stub".into());
        let mut session_ids = Vec::new();
        let mut thread_ids = Vec::new();

        for _ in 0..7 {
            let (session_id, thread_id) =
                open_generating_thread_with_loadable_connection(&panel, &connection, &mut cx);
            session_ids.push(session_id);
            thread_ids.push(thread_id);
        }

        let base_time = Instant::now();

        for session_id in session_ids.iter().take(6) {
            connection.end_turn(session_id.clone(), acp::StopReason::EndTurn);
        }
        cx.run_until_parked();

        panel.update(&mut cx, |panel, cx| {
            for (index, thread_id) in thread_ids.iter().take(6).enumerate() {
                let conversation_view = panel
                    .retained_threads
                    .get(thread_id)
                    .expect("retained thread should exist")
                    .clone();
                conversation_view.update(cx, |view, cx| {
                    view.set_updated_at(base_time + Duration::from_secs(index as u64), cx);
                });
            }
            panel.cleanup_retained_threads(cx);
        });

        panel.read_with(&cx, |panel, _cx| {
            assert_eq!(
                panel.test_background_thread_count(),
                5,
                "cleanup should keep at most five idle loadable retained threads"
            );
            assert!(
                !panel.retained_threads.contains_key(&thread_ids[0]),
                "oldest idle loadable retained thread should be removed"
            );
            for thread_id in &thread_ids[1..6] {
                assert!(
                    panel.retained_threads.contains_key(thread_id),
                    "more recent idle loadable retained threads should be retained"
                );
            }
            assert!(
                !panel.retained_threads.contains_key(&thread_ids[6]),
                "the active thread should not also be stored as a retained thread"
            );
        });
    }

    #[gpui::test]
    async fn test_cleanup_retained_threads_preserves_idle_non_loadable_threads(
        cx: &mut TestAppContext,
    ) {
        let (panel, mut cx) = setup_panel(cx).await;

        let non_loadable_connection = StubAgentConnection::new();
        let (_non_loadable_session_id, non_loadable_thread_id) =
            open_idle_thread_with_non_loadable_connection(
                &panel,
                &non_loadable_connection,
                &mut cx,
            );

        let loadable_connection = StubAgentConnection::new()
            .with_supports_load_session(true)
            .with_agent_id("loadable-stub".into())
            .with_telemetry_id("loadable-stub".into());
        let mut loadable_session_ids = Vec::new();
        let mut loadable_thread_ids = Vec::new();

        for _ in 0..7 {
            let (session_id, thread_id) = open_generating_thread_with_loadable_connection(
                &panel,
                &loadable_connection,
                &mut cx,
            );
            loadable_session_ids.push(session_id);
            loadable_thread_ids.push(thread_id);
        }

        let base_time = Instant::now();

        for session_id in loadable_session_ids.iter().take(6) {
            loadable_connection.end_turn(session_id.clone(), acp::StopReason::EndTurn);
        }
        cx.run_until_parked();

        panel.update(&mut cx, |panel, cx| {
            for (index, thread_id) in loadable_thread_ids.iter().take(6).enumerate() {
                let conversation_view = panel
                    .retained_threads
                    .get(thread_id)
                    .expect("retained thread should exist")
                    .clone();
                conversation_view.update(cx, |view, cx| {
                    view.set_updated_at(base_time + Duration::from_secs(index as u64), cx);
                });
            }
            panel.cleanup_retained_threads(cx);
        });

        panel.read_with(&cx, |panel, _cx| {
            assert_eq!(
                panel.test_background_thread_count(),
                6,
                "cleanup should keep the non-loadable idle thread in addition to five loadable ones"
            );
            assert!(
                panel.test_thread_is_in_background(non_loadable_thread_id),
                "idle non-loadable retained threads should not be cleanup candidates"
            );
            assert!(
                !panel.retained_threads.contains_key(&loadable_thread_ids[0]),
                "oldest idle loadable retained thread should still be removed"
            );
            for thread_id in &loadable_thread_ids[1..6] {
                assert!(
                    panel.retained_threads.contains_key(thread_id),
                    "more recent idle loadable retained threads should be retained"
                );
            }
            assert!(
                !panel.retained_threads.contains_key(&loadable_thread_ids[6]),
                "the active loadable thread should not also be stored as a retained thread"
            );
        });
    }

    #[test]
    fn test_deserialize_agent_variants() {
        // PascalCase (legacy AgentType format, persisted in panel state)
        assert_eq!(
            serde_json::from_str::<Agent>(r#""NativeAgent""#).unwrap(),
            Agent::NativeAgent,
        );
        assert_eq!(
            serde_json::from_str::<Agent>(r#"{"Custom":{"name":"my-agent"}}"#).unwrap(),
            Agent::Custom {
                id: "my-agent".into(),
            },
        );

        // Legacy TextThread variant deserializes to NativeAgent
        assert_eq!(
            serde_json::from_str::<Agent>(r#""TextThread""#).unwrap(),
            Agent::NativeAgent,
        );

        // snake_case (canonical format)
        assert_eq!(
            serde_json::from_str::<Agent>(r#""native_agent""#).unwrap(),
            Agent::NativeAgent,
        );
        assert_eq!(
            serde_json::from_str::<Agent>(r#"{"custom":{"name":"my-agent"}}"#).unwrap(),
            Agent::Custom {
                id: "my-agent".into(),
            },
        );

        // Serialization uses snake_case
        assert_eq!(
            serde_json::to_string(&Agent::NativeAgent).unwrap(),
            r#""native_agent""#,
        );
        assert_eq!(
            serde_json::to_string(&Agent::Custom {
                id: "my-agent".into()
            })
            .unwrap(),
            r#"{"custom":{"name":"my-agent"}}"#,
        );
    }

    #[gpui::test]
    fn test_resolve_worktree_branch_target() {
        let resolved = git_ui_core::worktree_service::resolve_worktree_branch_target(
            &NewWorktreeBranchTarget::ExistingBranch {
                name: "feature".to_string(),
            },
        );
        assert_eq!(resolved, Some("feature".to_string()));

        let resolved = git_ui_core::worktree_service::resolve_worktree_branch_target(
            &NewWorktreeBranchTarget::CurrentBranch,
        );
        assert_eq!(resolved, None);

        let resolved = git_ui_core::worktree_service::resolve_worktree_branch_target(
            &NewWorktreeBranchTarget::RemoteBranch {
                remote_name: "origin".to_string(),
                branch_name: "main".to_string(),
            },
        );
        assert_eq!(resolved, Some("refs/remotes/origin/main".to_string()));
    }

    #[gpui::test]
    async fn test_work_dirs_update_when_worktrees_change(cx: &mut TestAppContext) {
        use crate::thread_metadata_store::ThreadMetadataStore;

        init_test(cx);
        cx.update(|cx| {
            agent::ThreadStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);
        });

        // Set up a project with one worktree.
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/project_a", json!({ "file.txt": "" }))
            .await;
        let project = Project::test(fs.clone(), [Path::new("/project_a")], cx).await;

        let multi_workspace =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace
            .read_with(cx, |mw, _cx| mw.workspace().clone())
            .unwrap();
        let mut cx = VisualTestContext::from_window(multi_workspace.into(), cx);

        let panel = workspace.update_in(&mut cx, |workspace, window, cx| {
            cx.new(|cx| AgentPanel::new(workspace, window, cx))
        });

        // Open thread A and send a message. With empty next_prompt_updates it
        // stays generating, so opening B will move A to retained_threads.
        let connection_a = StubAgentConnection::new().with_agent_id("agent-a".into());
        open_thread_with_custom_connection(&panel, connection_a.clone(), &mut cx);
        send_message(&panel, &mut cx);
        let session_id_a = active_session_id(&panel, &cx);
        let thread_id_a = active_thread_id(&panel, &cx);

        // Open thread C — thread A (generating) moves to background.
        // Thread C completes immediately (idle), then opening B moves C to background too.
        let connection_c = StubAgentConnection::new().with_agent_id("agent-c".into());
        connection_c.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
            acp::ContentChunk::new("done".into()),
        )]);
        open_thread_with_custom_connection(&panel, connection_c.clone(), &mut cx);
        send_message(&panel, &mut cx);
        let thread_id_c = active_thread_id(&panel, &cx);

        // Open thread B — thread C (idle, non-loadable) is retained in background.
        let connection_b = StubAgentConnection::new().with_agent_id("agent-b".into());
        open_thread_with_custom_connection(&panel, connection_b.clone(), &mut cx);
        send_message(&panel, &mut cx);
        let session_id_b = active_session_id(&panel, &cx);
        let _thread_id_b = active_thread_id(&panel, &cx);

        let metadata_store = cx.update(|_, cx| ThreadMetadataStore::global(cx));

        panel.read_with(&cx, |panel, _cx| {
            assert!(
                panel.test_thread_is_in_background(thread_id_a),
                "Thread A should be in retained_threads"
            );
            assert!(
                panel.test_thread_is_in_background(thread_id_c),
                "Thread C should be in retained_threads"
            );
        });

        // Verify initial work_dirs for thread B contain only /project_a.
        let initial_b_paths = panel.read_with(&cx, |panel, cx| {
            let thread = panel.active_agent_thread(cx).unwrap();
            thread.read(cx).work_dirs().cloned().unwrap()
        });
        assert_eq!(
            initial_b_paths.ordered_paths().collect::<Vec<_>>(),
            vec![&PathBuf::from("/project_a")],
            "Thread B should initially have only /project_a"
        );

        // Now add a second worktree to the project.
        fs.insert_tree("/project_b", json!({ "other.txt": "" }))
            .await;
        let (new_tree, _) = project
            .update(&mut cx, |project, cx| {
                project.find_or_create_worktree("/project_b", true, cx)
            })
            .await
            .unwrap();
        cx.read(|cx| new_tree.read(cx).as_local().unwrap().scan_complete())
            .await;
        cx.run_until_parked();

        // Verify thread B's (active) work_dirs now include both worktrees.
        let updated_b_paths = panel.read_with(&cx, |panel, cx| {
            let thread = panel.active_agent_thread(cx).unwrap();
            thread.read(cx).work_dirs().cloned().unwrap()
        });
        let mut b_paths_sorted = updated_b_paths.ordered_paths().cloned().collect::<Vec<_>>();
        b_paths_sorted.sort();
        assert_eq!(
            b_paths_sorted,
            vec![PathBuf::from("/project_a"), PathBuf::from("/project_b")],
            "Thread B work_dirs should include both worktrees after adding /project_b"
        );

        // Verify thread A's (background) work_dirs are also updated.
        let updated_a_paths = panel.read_with(&cx, |panel, cx| {
            let bg_view = panel.test_background_view(thread_id_a).unwrap();
            let root_thread = bg_view.read(cx).root_thread_view().unwrap();
            root_thread
                .read(cx)
                .thread
                .read(cx)
                .work_dirs()
                .cloned()
                .unwrap()
        });
        let mut a_paths_sorted = updated_a_paths.ordered_paths().cloned().collect::<Vec<_>>();
        a_paths_sorted.sort();
        assert_eq!(
            a_paths_sorted,
            vec![PathBuf::from("/project_a"), PathBuf::from("/project_b")],
            "Thread A work_dirs should include both worktrees after adding /project_b"
        );

        // Verify thread idle C was also updated.
        let updated_c_paths = panel.read_with(&cx, |panel, cx| {
            let bg_view = panel.test_background_view(thread_id_c).unwrap();
            let root_thread = bg_view.read(cx).root_thread_view().unwrap();
            root_thread
                .read(cx)
                .thread
                .read(cx)
                .work_dirs()
                .cloned()
                .unwrap()
        });
        let mut c_paths_sorted = updated_c_paths.ordered_paths().cloned().collect::<Vec<_>>();
        c_paths_sorted.sort();
        assert_eq!(
            c_paths_sorted,
            vec![PathBuf::from("/project_a"), PathBuf::from("/project_b")],
            "Thread C (idle background) work_dirs should include both worktrees after adding /project_b"
        );

        // Verify the metadata store reflects the new paths for running threads only.
        cx.run_until_parked();
        for (label, session_id) in [("thread B", &session_id_b), ("thread A", &session_id_a)] {
            let metadata_paths = metadata_store.read_with(&cx, |store, _cx| {
                let metadata = store
                    .entry_by_session(session_id)
                    .unwrap_or_else(|| panic!("{label} thread metadata should exist"));
                metadata.folder_paths().clone()
            });
            let mut sorted = metadata_paths.ordered_paths().cloned().collect::<Vec<_>>();
            sorted.sort();
            assert_eq!(
                sorted,
                vec![PathBuf::from("/project_a"), PathBuf::from("/project_b")],
                "{label} thread metadata folder_paths should include both worktrees"
            );
        }

        // Now remove a worktree and verify work_dirs shrink.
        let worktree_b_id = new_tree.read_with(&cx, |tree, _| tree.id());
        project.update(&mut cx, |project, cx| {
            project.remove_worktree(worktree_b_id, cx);
        });
        cx.run_until_parked();

        let after_remove_b = panel.read_with(&cx, |panel, cx| {
            let thread = panel.active_agent_thread(cx).unwrap();
            thread.read(cx).work_dirs().cloned().unwrap()
        });
        assert_eq!(
            after_remove_b.ordered_paths().collect::<Vec<_>>(),
            vec![&PathBuf::from("/project_a")],
            "Thread B work_dirs should revert to only /project_a after removing /project_b"
        );

        let after_remove_a = panel.read_with(&cx, |panel, cx| {
            let bg_view = panel.test_background_view(thread_id_a).unwrap();
            let root_thread = bg_view.read(cx).root_thread_view().unwrap();
            root_thread
                .read(cx)
                .thread
                .read(cx)
                .work_dirs()
                .cloned()
                .unwrap()
        });
        assert_eq!(
            after_remove_a.ordered_paths().collect::<Vec<_>>(),
            vec![&PathBuf::from("/project_a")],
            "Thread A work_dirs should revert to only /project_a after removing /project_b"
        );
    }

    #[gpui::test]
    async fn test_new_workspace_inherits_global_last_used_agent(cx: &mut TestAppContext) {
        init_test(cx);
        cx.update(|cx| {
            agent::ThreadStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);
            // Use an isolated DB so parallel tests can't overwrite our global key.
            cx.set_global(db::AppDatabase::test_new());
        });

        let custom_agent = Agent::Custom {
            id: "my-preferred-agent".into(),
        };

        // Write a known agent to the global KVP to simulate a user who has
        // previously used this agent in another workspace.
        let kvp = cx.update(|cx| KeyValueStore::global(cx));
        write_global_last_used_agent(kvp, custom_agent.clone()).await;

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs.clone(), [], cx).await;

        let multi_workspace =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));

        let workspace = multi_workspace
            .read_with(cx, |multi_workspace, _cx| {
                multi_workspace.workspace().clone()
            })
            .unwrap();

        workspace.update(cx, |workspace, _cx| {
            workspace.set_random_database_id();
        });

        let cx = &mut VisualTestContext::from_window(multi_workspace.into(), cx);

        // Load the panel via `load()`, which reads the global fallback
        // asynchronously when no per-workspace state exists.
        let async_cx = cx.update(|window, cx| window.to_async(cx));
        let panel = AgentPanel::load(workspace.downgrade(), async_cx)
            .await
            .expect("panel load should succeed");
        cx.run_until_parked();

        panel.read_with(cx, |panel, _cx| {
            assert_eq!(
                panel.selected_agent, custom_agent,
                "new workspace should inherit the global last-used agent"
            );
        });
    }

    #[gpui::test]
    async fn test_select_agent_action_updates_visible_draft(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        cx.update(|cx| {
            agent::ThreadStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);
            <dyn fs::Fs>::set_global(fs.clone(), cx);
        });

        fs.insert_tree("/project", json!({ "file.txt": "" })).await;
        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        let multi_workspace =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace
            .read_with(cx, |multi_workspace, _cx| {
                multi_workspace.workspace().clone()
            })
            .unwrap();
        let cx = &mut VisualTestContext::from_window(multi_workspace.into(), cx);

        let panel = workspace.update_in(cx, |workspace, window, cx| {
            let panel = cx.new(|cx| AgentPanel::new(workspace, window, cx));
            workspace.add_panel(panel.clone(), window, cx);
            panel
        });

        panel.update_in(cx, |panel, window, cx| {
            panel.activate_draft(false, AgentThreadSource::AgentPanel, window, cx);
        });

        cx.dispatch_action(SelectAgent {
            agent: "my-configured-agent".to_string(),
        });
        cx.run_until_parked();

        let expected_agent = Agent::Custom {
            id: "my-configured-agent".into(),
        };

        panel.read_with(cx, |panel, cx| {
            let draft = panel.draft_thread.as_ref().expect("draft should exist");
            assert_eq!(panel.selected_agent, expected_agent);
            assert_eq!(*draft.read(cx).agent_key(), expected_agent);
        });

        let kvp = cx.update(|_, cx| KeyValueStore::global(cx));
        assert_eq!(
            read_global_last_used_agent(&kvp),
            Some(expected_agent),
            "the selection should be persisted as the global last-used agent"
        );
    }

    #[gpui::test]
    async fn test_workspaces_maintain_independent_agent_selection(cx: &mut TestAppContext) {
        init_test(cx);
        cx.update(|cx| {
            agent::ThreadStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);
        });

        let fs = FakeFs::new(cx.executor());
        let project_a = Project::test(fs.clone(), [], cx).await;
        let project_b = Project::test(fs, [], cx).await;

        let multi_workspace =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));

        let workspace_a = multi_workspace
            .read_with(cx, |multi_workspace, _cx| {
                multi_workspace.workspace().clone()
            })
            .unwrap();

        let workspace_b = multi_workspace
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.test_add_workspace(project_b.clone(), window, cx)
            })
            .unwrap();

        workspace_a.update(cx, |workspace, _cx| {
            workspace.set_random_database_id();
        });
        workspace_b.update(cx, |workspace, _cx| {
            workspace.set_random_database_id();
        });

        let cx = &mut VisualTestContext::from_window(multi_workspace.into(), cx);

        let agent_a = Agent::Custom {
            id: "agent-alpha".into(),
        };
        let agent_b = Agent::Custom {
            id: "agent-beta".into(),
        };

        // Set up workspace A with agent_a
        let panel_a = workspace_a.update_in(cx, |workspace, window, cx| {
            cx.new(|cx| AgentPanel::new(workspace, window, cx))
        });
        panel_a.update(cx, |panel, _cx| {
            panel.selected_agent = agent_a.clone();
        });

        // Set up workspace B with agent_b
        let panel_b = workspace_b.update_in(cx, |workspace, window, cx| {
            cx.new(|cx| AgentPanel::new(workspace, window, cx))
        });
        panel_b.update(cx, |panel, _cx| {
            panel.selected_agent = agent_b.clone();
        });

        // Serialize both panels
        panel_a.update(cx, |panel, cx| panel.serialize(cx));
        panel_b.update(cx, |panel, cx| panel.serialize(cx));
        cx.run_until_parked();

        // Load fresh panels from serialized state and verify independence
        let async_cx = cx.update(|window, cx| window.to_async(cx));
        let loaded_a = AgentPanel::load(workspace_a.downgrade(), async_cx)
            .await
            .expect("panel A load should succeed");
        cx.run_until_parked();

        let async_cx = cx.update(|window, cx| window.to_async(cx));
        let loaded_b = AgentPanel::load(workspace_b.downgrade(), async_cx)
            .await
            .expect("panel B load should succeed");
        cx.run_until_parked();

        loaded_a.read_with(cx, |panel, _cx| {
            assert_eq!(
                panel.selected_agent, agent_a,
                "workspace A should restore agent-alpha, not agent-beta"
            );
        });

        loaded_b.read_with(cx, |panel, _cx| {
            assert_eq!(
                panel.selected_agent, agent_b,
                "workspace B should restore agent-beta, not agent-alpha"
            );
        });
    }

    #[gpui::test]
    async fn test_new_thread_uses_workspace_selected_agent(cx: &mut TestAppContext) {
        init_test(cx);
        cx.update(|cx| {
            agent::ThreadStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/project", json!({ "file.txt": "" })).await;
        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;

        let multi_workspace =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));

        let workspace = multi_workspace
            .read_with(cx, |multi_workspace, _cx| {
                multi_workspace.workspace().clone()
            })
            .unwrap();

        workspace.update(cx, |workspace, _cx| {
            workspace.set_random_database_id();
        });

        let cx = &mut VisualTestContext::from_window(multi_workspace.into(), cx);

        let custom_agent = Agent::Custom {
            id: "my-custom-agent".into(),
        };

        let panel = workspace.update_in(cx, |workspace, window, cx| {
            let panel = cx.new(|cx| AgentPanel::new(workspace, window, cx));
            workspace.add_panel(panel.clone(), window, cx);
            panel
        });

        // Set selected_agent to a custom agent
        panel.update(cx, |panel, _cx| {
            panel.selected_agent = custom_agent.clone();
        });

        // Call new_thread, which internally calls external_thread(None, ...)
        // This resolves the agent from self.selected_agent
        panel.update_in(cx, |panel, window, cx| {
            panel.new_thread(&NewThread, window, cx);
        });

        panel.read_with(cx, |panel, _cx| {
            assert_eq!(
                panel.selected_agent, custom_agent,
                "selected_agent should remain the custom agent after new_thread"
            );
            assert!(
                panel.active_conversation_view().is_some(),
                "a thread should have been created"
            );
        });
    }

    #[gpui::test]
    async fn test_draft_replaced_when_selected_agent_changes(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        cx.update(|cx| {
            agent::ThreadStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);
            <dyn fs::Fs>::set_global(fs.clone(), cx);
        });

        fs.insert_tree("/project", json!({ "file.txt": "" })).await;
        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;

        let multi_workspace =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));

        let workspace = multi_workspace
            .read_with(cx, |multi_workspace, _cx| {
                multi_workspace.workspace().clone()
            })
            .unwrap();

        workspace.update(cx, |workspace, _cx| {
            workspace.set_random_database_id();
        });

        let cx = &mut VisualTestContext::from_window(multi_workspace.into(), cx);

        let panel = workspace.update_in(cx, |workspace, window, cx| {
            let panel = cx.new(|cx| AgentPanel::new(workspace, window, cx));
            workspace.add_panel(panel.clone(), window, cx);
            panel
        });

        // Create a draft with the default NativeAgent.
        panel.update_in(cx, |panel, window, cx| {
            panel.activate_draft(true, AgentThreadSource::AgentPanel, window, cx);
        });

        let first_draft_id = panel.read_with(cx, |panel, cx| {
            assert!(panel.draft_thread.is_some());
            assert_eq!(panel.selected_agent, Agent::NativeAgent);
            let draft = panel.draft_thread.as_ref().unwrap();
            assert_eq!(*draft.read(cx).agent_key(), Agent::NativeAgent);
            draft.entity_id()
        });

        // Switch selected_agent to a custom agent, then activate_draft again.
        // The stale NativeAgent draft should be replaced.
        let custom_agent = Agent::Custom {
            id: "my-custom-agent".into(),
        };
        panel.update_in(cx, |panel, window, cx| {
            panel.selected_agent = custom_agent.clone();
            panel.activate_draft(true, AgentThreadSource::AgentPanel, window, cx);
        });

        panel.read_with(cx, |panel, cx| {
            let draft = panel.draft_thread.as_ref().expect("draft should exist");
            assert_ne!(
                draft.entity_id(),
                first_draft_id,
                "a new draft should have been created"
            );
            assert_eq!(
                *draft.read(cx).agent_key(),
                custom_agent,
                "the new draft should use the custom agent"
            );
        });

        // Calling activate_draft again with the same agent should return the
        // cached draft (no replacement).
        let second_draft_id = panel.read_with(cx, |panel, _cx| {
            panel.draft_thread.as_ref().unwrap().entity_id()
        });

        panel.update_in(cx, |panel, window, cx| {
            panel.activate_draft(true, AgentThreadSource::AgentPanel, window, cx);
        });

        panel.read_with(cx, |panel, _cx| {
            assert_eq!(
                panel.draft_thread.as_ref().unwrap().entity_id(),
                second_draft_id,
                "draft should be reused when the agent has not changed"
            );
        });
    }

    #[gpui::test]
    async fn test_activate_draft_preserves_typed_content(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        cx.update(|cx| {
            agent::ThreadStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);
            <dyn fs::Fs>::set_global(fs.clone(), cx);
        });

        fs.insert_tree("/project", json!({ "file.txt": "" })).await;
        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;

        let multi_workspace =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));

        let workspace = multi_workspace
            .read_with(cx, |multi_workspace, _cx| {
                multi_workspace.workspace().clone()
            })
            .unwrap();

        workspace.update(cx, |workspace, _cx| {
            workspace.set_random_database_id();
        });

        let cx = &mut VisualTestContext::from_window(multi_workspace.into(), cx);

        let panel = workspace.update_in(cx, |workspace, window, cx| {
            let panel = cx.new(|cx| AgentPanel::new(workspace, window, cx));
            workspace.add_panel(panel.clone(), window, cx);
            panel
        });

        // Create a draft using the Stub agent, which connects synchronously.
        panel.update_in(cx, |panel, window, cx| {
            panel.selected_agent = Agent::Stub;
            panel.activate_draft(true, AgentThreadSource::AgentPanel, window, cx);
        });
        cx.run_until_parked();

        let initial_draft_id = panel.read_with(cx, |panel, _cx| {
            panel.draft_thread.as_ref().unwrap().entity_id()
        });
        let initial_thread_id =
            panel.read_with(cx, |panel, cx| panel.active_thread_id(cx).unwrap());

        // Type some text into the draft editor.
        let thread_view = panel.read_with(cx, |panel, cx| panel.active_thread_view(cx).unwrap());
        let message_editor = thread_view.read_with(cx, |view, _cx| view.message_editor.clone());
        message_editor.update_in(cx, |editor, window, cx| {
            editor.set_text("Don't lose me!", window, cx);
        });

        // Press cmd-n on a typed draft — the draft is parked into
        // `retained_threads` so the user can return to it from the
        // sidebar, and a fresh, *empty* ephemeral draft becomes active.
        // The parked draft retains the prompt; the new one is a blank
        // slate.
        cx.dispatch_action(NewThread);
        cx.run_until_parked();

        panel.read_with(cx, |panel, _cx| {
            assert!(
                panel.test_thread_is_in_background(initial_thread_id),
                "typed draft should have been parked into retained_threads"
            );
            let active_draft_id = panel.draft_thread.as_ref().unwrap().entity_id();
            assert_ne!(
                active_draft_id, initial_draft_id,
                "cmd-n should produce a fresh ephemeral draft"
            );
        });

        // The parked draft still holds the typed prompt.
        let parked_text = panel.read_with(cx, |panel, cx| panel.editor_text(initial_thread_id, cx));
        assert_eq!(
            parked_text.as_deref(),
            Some("Don't lose me!"),
            "parked draft should retain the typed prompt"
        );

        // The new active draft starts empty — no carry-over.
        let active_thread_id = panel.read_with(cx, |panel, cx| panel.active_thread_id(cx).unwrap());
        let active_text = panel.read_with(cx, |panel, cx| panel.editor_text(active_thread_id, cx));
        assert_eq!(
            active_text, None,
            "fresh ephemeral draft should start empty, not carry the parked draft's prompt"
        );
    }

    /// When the user is viewing a *parked* draft (selected from the
    /// sidebar) and presses `+`, the panel should just focus the
    /// ephemeral new-draft slot — not park it and create yet another
    /// empty draft. `+` is "go to my new-thread slot", not "reset state".
    #[gpui::test]
    async fn test_plus_with_parked_draft_active_focuses_ephemeral(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        cx.update(|cx| {
            agent::ThreadStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);
            <dyn fs::Fs>::set_global(fs.clone(), cx);
        });

        fs.insert_tree("/project", json!({ "file.txt": "" })).await;
        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        let multi_workspace =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace
            .read_with(cx, |multi_workspace, _cx| {
                multi_workspace.workspace().clone()
            })
            .unwrap();
        workspace.update(cx, |workspace, _cx| workspace.set_random_database_id());
        let cx = &mut VisualTestContext::from_window(multi_workspace.into(), cx);
        let panel = workspace.update_in(cx, |workspace, window, cx| {
            let panel = cx.new(|cx| AgentPanel::new(workspace, window, cx));
            workspace.add_panel(panel.clone(), window, cx);
            panel
        });

        // Open an initial draft, type into it, then press `+` to park it
        // and create a fresh ephemeral. The fresh ephemeral is what we'll
        // expect to refocus later.
        panel.update_in(cx, |panel, window, cx| {
            panel.selected_agent = Agent::Stub;
            panel.activate_draft(true, AgentThreadSource::AgentPanel, window, cx);
        });
        cx.run_until_parked();
        let parked_thread_id = crate::test_support::active_thread_id(&panel, cx);
        crate::test_support::type_draft_prompt(&panel, "parked draft prompt", cx);
        panel.update_in(cx, |panel, window, cx| {
            panel.new_thread(&NewThread, window, cx);
        });
        cx.run_until_parked();

        let ephemeral_thread_id = crate::test_support::active_thread_id(&panel, cx);
        let ephemeral_entity_id = panel.read_with(cx, |panel, _cx| {
            panel.draft_thread.as_ref().unwrap().entity_id()
        });
        assert_ne!(
            ephemeral_thread_id, parked_thread_id,
            "sanity: parking should have produced a fresh ephemeral draft"
        );

        // Activate the parked draft (simulates clicking it in the sidebar).
        panel.update_in(cx, |panel, window, cx| {
            panel.load_agent_thread(
                Agent::Stub,
                parked_thread_id,
                None,
                None,
                true,
                AgentThreadSource::Sidebar,
                window,
                cx,
            );
        });
        cx.run_until_parked();
        assert_eq!(
            crate::test_support::active_thread_id(&panel, cx),
            parked_thread_id,
            "sanity: parked draft should be the active view after load_agent_thread"
        );
        // The parked draft has content, so it was NOT reclaimed as
        // ephemeral. The previous ephemeral draft should still be in
        // the draft_thread slot.
        panel.read_with(cx, |panel, _cx| {
            assert_eq!(
                panel.draft_thread.as_ref().unwrap().entity_id(),
                ephemeral_entity_id,
                "ephemeral draft slot should still hold the fresh draft"
            );
        });

        // Now press `+`. The ephemeral draft should become the active
        // view since it matches the selected agent.
        panel.update_in(cx, |panel, window, cx| {
            panel.new_thread(&NewThread, window, cx);
        });
        cx.run_until_parked();

        panel.read_with(cx, |panel, cx| {
            assert_eq!(
                panel.active_thread_id(cx),
                Some(ephemeral_thread_id),
                "`+` should have switched back to the existing ephemeral draft"
            );
            assert_eq!(
                panel.draft_thread.as_ref().unwrap().entity_id(),
                ephemeral_entity_id,
                "`+` should not have replaced the ephemeral draft"
            );
            assert!(
                panel.test_thread_is_in_background(parked_thread_id),
                "parked draft should remain in `retained_threads`"
            );
        });
    }

    /// When viewing a parked draft (agent A) and selecting a different
    /// agent (B) from the dropdown menu, the panel should create a fresh
    /// draft for agent B — not reuse the existing ephemeral draft that
    /// was bound to agent A.
    #[gpui::test]
    async fn test_new_external_agent_replaces_mismatched_ephemeral_draft(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        cx.update(|cx| {
            agent::ThreadStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);
            <dyn fs::Fs>::set_global(fs.clone(), cx);
        });

        fs.insert_tree("/project", json!({ "file.txt": "" })).await;
        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        let multi_workspace =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace
            .read_with(cx, |multi_workspace, _cx| {
                multi_workspace.workspace().clone()
            })
            .unwrap();
        workspace.update(cx, |workspace, _cx| workspace.set_random_database_id());
        let cx = &mut VisualTestContext::from_window(multi_workspace.into(), cx);
        let panel = workspace.update_in(cx, |workspace, window, cx| {
            let panel = cx.new(|cx| AgentPanel::new(workspace, window, cx));
            workspace.add_panel(panel.clone(), window, cx);
            panel
        });

        // Create a draft with Stub agent, type into it, then press `+`
        // to park it — this also creates a fresh ephemeral draft (Stub).
        panel.update_in(cx, |panel, window, cx| {
            panel.selected_agent = Agent::Stub;
            panel.activate_draft(true, AgentThreadSource::AgentPanel, window, cx);
        });
        cx.run_until_parked();
        let parked_thread_id = crate::test_support::active_thread_id(&panel, cx);
        crate::test_support::type_draft_prompt(&panel, "parked prompt", cx);
        panel.update_in(cx, |panel, window, cx| {
            panel.new_thread(&NewThread, window, cx);
        });
        cx.run_until_parked();

        let ephemeral_thread_id = crate::test_support::active_thread_id(&panel, cx);
        assert_ne!(ephemeral_thread_id, parked_thread_id);
        panel.read_with(cx, |panel, cx| {
            assert_eq!(
                panel.draft_thread.as_ref().unwrap().read(cx).agent_key(),
                &Agent::Stub,
                "ephemeral draft should be Stub agent"
            );
        });

        // Navigate back to the parked draft (simulates sidebar click).
        panel.update_in(cx, |panel, window, cx| {
            panel.load_agent_thread(
                Agent::Stub,
                parked_thread_id,
                None,
                None,
                true,
                AgentThreadSource::Sidebar,
                window,
                cx,
            );
        });
        cx.run_until_parked();
        assert_eq!(
            crate::test_support::active_thread_id(&panel, cx),
            parked_thread_id,
        );

        // Now switch to NativeAgent (simulates selecting a different
        // agent from the toolbar dropdown). This should NOT reuse the
        // Stub ephemeral draft — it should replace it with one bound to
        // NativeAgent.
        panel.update_in(cx, |panel, window, cx| {
            panel.selected_agent = Agent::NativeAgent;
            panel.activate_new_thread(true, AgentThreadSource::AgentPanel, window, cx);
        });
        cx.run_until_parked();

        panel.read_with(cx, |panel, cx| {
            let draft = panel.draft_thread.as_ref().expect("draft should exist");
            assert_eq!(
                draft.read(cx).agent_key(),
                &Agent::NativeAgent,
                "ephemeral draft should be bound to NativeAgent, not Stub"
            );
            let active_id = panel.active_thread_id(cx).unwrap();
            assert_ne!(
                active_id, ephemeral_thread_id,
                "old Stub ephemeral draft should have been replaced"
            );
            assert!(
                panel.test_thread_is_in_background(parked_thread_id),
                "parked draft should still be in retained_threads"
            );
        });
    }

    #[gpui::test]
    async fn test_typed_draft_is_parked_when_switching_agents(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        cx.update(|cx| {
            agent::ThreadStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);
            <dyn fs::Fs>::set_global(fs.clone(), cx);
        });

        fs.insert_tree("/project", json!({ "file.txt": "" })).await;
        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;

        let multi_workspace =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));

        let workspace = multi_workspace
            .read_with(cx, |multi_workspace, _cx| {
                multi_workspace.workspace().clone()
            })
            .unwrap();

        workspace.update(cx, |workspace, _cx| {
            workspace.set_random_database_id();
        });

        let cx = &mut VisualTestContext::from_window(multi_workspace.into(), cx);

        let panel = workspace.update_in(cx, |workspace, window, cx| {
            let panel = cx.new(|cx| AgentPanel::new(workspace, window, cx));
            workspace.add_panel(panel.clone(), window, cx);
            panel
        });

        // Create a draft with a custom stub server that connects synchronously.
        panel.update_in(cx, |panel, window, cx| {
            panel.open_draft_with_server(
                Rc::new(StubAgentServer::new(StubAgentConnection::new())),
                window,
                cx,
            );
        });
        cx.run_until_parked();

        let initial_draft_id = panel.read_with(cx, |panel, _cx| {
            panel.draft_thread.as_ref().unwrap().entity_id()
        });
        let initial_thread_id =
            panel.read_with(cx, |panel, cx| panel.active_thread_id(cx).unwrap());

        // Type text into the first draft's editor.
        let thread_view = panel.read_with(cx, |panel, cx| panel.active_thread_view(cx).unwrap());
        let message_editor = thread_view.read_with(cx, |view, _cx| view.message_editor.clone());
        message_editor.update_in(cx, |editor, window, cx| {
            editor.set_text("saved prompt", window, cx);
        });

        // Switch to a different agent. The typed draft should be parked
        // into `retained_threads` (keeping the user's prompt accessible
        // from the sidebar) and a fresh empty draft on the new agent
        // should become active.
        cx.dispatch_action(NewExternalAgentThread {
            agent: Agent::Stub.id(),
        });
        cx.run_until_parked();

        // A new draft should have been created for the Stub agent.
        panel.read_with(cx, |panel, cx| {
            let draft = panel.draft_thread.as_ref().expect("draft should exist");
            assert_ne!(
                draft.entity_id(),
                initial_draft_id,
                "a new draft should have been created for the new agent"
            );
            assert_eq!(
                *draft.read(cx).agent_key(),
                Agent::Stub,
                "new draft should use the new agent"
            );
            assert!(
                panel.test_thread_is_in_background(initial_thread_id),
                "typed draft should have been parked into retained_threads"
            );
        });

        // The parked draft retains the prompt.
        let parked_text = panel.read_with(cx, |panel, cx| panel.editor_text(initial_thread_id, cx));
        assert_eq!(
            parked_text.as_deref(),
            Some("saved prompt"),
            "parked draft should retain the user's prompt"
        );

        // The new draft on the new agent starts empty.
        let active_thread_id = panel.read_with(cx, |panel, cx| panel.active_thread_id(cx).unwrap());
        let active_text = panel.read_with(cx, |panel, cx| panel.editor_text(active_thread_id, cx));
        assert_eq!(
            active_text, None,
            "new draft on the new agent should start empty, not carry the parked draft's prompt"
        );
    }

    #[gpui::test]
    async fn test_rollback_all_succeed_returns_ok(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        cx.update(|cx| {
            cx.update_flags(true, vec!["agent-v2".to_string()]);
            agent::ThreadStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);
            <dyn fs::Fs>::set_global(fs.clone(), cx);
        });

        fs.insert_tree(
            "/project",
            json!({
                ".git": {},
                "src": { "main.rs": "fn main() {}" }
            }),
        )
        .await;

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.executor().run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project.repositories(cx).values().next().unwrap().clone()
        });

        let multi_workspace =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));

        let path_a = PathBuf::from("/worktrees/branch/project_a");
        let path_b = PathBuf::from("/worktrees/branch/project_b");

        let (sender_a, receiver_a) = futures::channel::oneshot::channel::<Result<()>>();
        let (sender_b, receiver_b) = futures::channel::oneshot::channel::<Result<()>>();
        sender_a.send(Ok(())).unwrap();
        sender_b.send(Ok(())).unwrap();

        let creation_infos = vec![
            (repository.clone(), path_a.clone(), receiver_a),
            (repository.clone(), path_b.clone(), receiver_b),
        ];

        let fs_clone = fs.clone();
        let result = multi_workspace
            .update(cx, |_, window, cx| {
                window.spawn(cx, async move |cx| {
                    git_ui_core::worktree_service::await_and_rollback_on_failure(
                        creation_infos,
                        fs_clone,
                        cx,
                    )
                    .await
                })
            })
            .unwrap()
            .await;

        let paths = result.expect("all succeed should return Ok");
        assert_eq!(paths, vec![path_a, path_b]);
    }

    #[gpui::test]
    async fn test_rollback_on_failure_attempts_all_worktrees(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        cx.update(|cx| {
            cx.update_flags(true, vec!["agent-v2".to_string()]);
            agent::ThreadStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);
            <dyn fs::Fs>::set_global(fs.clone(), cx);
        });

        fs.insert_tree(
            "/project",
            json!({
                ".git": {},
                "src": { "main.rs": "fn main() {}" }
            }),
        )
        .await;

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.executor().run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project.repositories(cx).values().next().unwrap().clone()
        });

        // Actually create a worktree so it exists in FakeFs for rollback to find.
        let success_path = PathBuf::from("/worktrees/branch/project");
        cx.update(|cx| {
            repository.update(cx, |repo, _| {
                repo.create_worktree(
                    git::repository::CreateWorktreeTarget::NewBranch {
                        branch_name: "branch".to_string(),
                        base_sha: None,
                    },
                    success_path.clone(),
                )
            })
        })
        .await
        .unwrap()
        .unwrap();
        cx.executor().run_until_parked();

        // Verify the worktree directory exists before rollback.
        assert!(
            fs.is_dir(&success_path).await,
            "worktree directory should exist before rollback"
        );

        let multi_workspace =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));

        // Build creation_infos: one success, one failure.
        let failed_path = PathBuf::from("/worktrees/branch/failed_project");

        let (sender_ok, receiver_ok) = futures::channel::oneshot::channel::<Result<()>>();
        let (sender_err, receiver_err) = futures::channel::oneshot::channel::<Result<()>>();
        sender_ok.send(Ok(())).unwrap();
        sender_err
            .send(Err(anyhow!("branch already exists")))
            .unwrap();

        let creation_infos = vec![
            (repository.clone(), success_path.clone(), receiver_ok),
            (repository.clone(), failed_path.clone(), receiver_err),
        ];

        let fs_clone = fs.clone();
        let result = multi_workspace
            .update(cx, |_, window, cx| {
                window.spawn(cx, async move |cx| {
                    git_ui_core::worktree_service::await_and_rollback_on_failure(
                        creation_infos,
                        fs_clone,
                        cx,
                    )
                    .await
                })
            })
            .unwrap()
            .await;

        assert!(
            result.is_err(),
            "should return error when any creation fails"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("branch already exists"),
            "error should mention the original failure: {err_msg}"
        );

        // The successful worktree should have been rolled back by git.
        cx.executor().run_until_parked();
        assert!(
            !fs.is_dir(&success_path).await,
            "successful worktree directory should be removed by rollback"
        );
    }

    #[gpui::test]
    async fn test_rollback_on_canceled_receiver(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        cx.update(|cx| {
            cx.update_flags(true, vec!["agent-v2".to_string()]);
            agent::ThreadStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);
            <dyn fs::Fs>::set_global(fs.clone(), cx);
        });

        fs.insert_tree(
            "/project",
            json!({
                ".git": {},
                "src": { "main.rs": "fn main() {}" }
            }),
        )
        .await;

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.executor().run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project.repositories(cx).values().next().unwrap().clone()
        });

        let multi_workspace =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));

        let path = PathBuf::from("/worktrees/branch/project");

        // Drop the sender to simulate a canceled receiver.
        let (_sender, receiver) = futures::channel::oneshot::channel::<Result<()>>();
        drop(_sender);

        let creation_infos = vec![(repository.clone(), path.clone(), receiver)];

        let fs_clone = fs.clone();
        let result = multi_workspace
            .update(cx, |_, window, cx| {
                window.spawn(cx, async move |cx| {
                    git_ui_core::worktree_service::await_and_rollback_on_failure(
                        creation_infos,
                        fs_clone,
                        cx,
                    )
                    .await
                })
            })
            .unwrap()
            .await;

        assert!(
            result.is_err(),
            "should return error when receiver is canceled"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("canceled"),
            "error should mention cancellation: {err_msg}"
        );
    }

    #[gpui::test]
    async fn test_rollback_cleans_up_orphan_directories(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        cx.update(|cx| {
            cx.update_flags(true, vec!["agent-v2".to_string()]);
            agent::ThreadStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);
            <dyn fs::Fs>::set_global(fs.clone(), cx);
        });

        fs.insert_tree(
            "/project",
            json!({
                ".git": {},
                "src": { "main.rs": "fn main() {}" }
            }),
        )
        .await;

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.executor().run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project.repositories(cx).values().next().unwrap().clone()
        });

        let multi_workspace =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));

        // Simulate the orphan state: create_dir_all was called but git
        // worktree add failed, leaving a directory with leftover files.
        let orphan_path = PathBuf::from("/worktrees/branch/orphan_project");
        fs.insert_tree(
            "/worktrees/branch/orphan_project",
            json!({ "leftover.txt": "junk" }),
        )
        .await;

        assert!(
            fs.is_dir(&orphan_path).await,
            "orphan dir should exist before rollback"
        );

        let (sender, receiver) = futures::channel::oneshot::channel::<Result<()>>();
        sender.send(Err(anyhow!("hook failed"))).unwrap();

        let creation_infos = vec![(repository.clone(), orphan_path.clone(), receiver)];

        let fs_clone = fs.clone();
        let result = multi_workspace
            .update(cx, |_, window, cx| {
                window.spawn(cx, async move |cx| {
                    git_ui_core::worktree_service::await_and_rollback_on_failure(
                        creation_infos,
                        fs_clone,
                        cx,
                    )
                    .await
                })
            })
            .unwrap()
            .await;

        cx.executor().run_until_parked();

        assert!(result.is_err());
        assert!(
            !fs.is_dir(&orphan_path).await,
            "orphan worktree directory should be removed by filesystem cleanup"
        );
    }

    #[gpui::test]
    async fn test_selected_agent_syncs_when_navigating_between_threads(cx: &mut TestAppContext) {
        let (panel, mut cx) = setup_panel(cx).await;

        let stub_agent = Agent::Custom { id: "Test".into() };

        // Open thread A and send a message so it is retained.
        let connection_a = StubAgentConnection::new();
        connection_a.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
            acp::ContentChunk::new("response a".into()),
        )]);
        open_thread_with_connection(&panel, connection_a, &mut cx);
        let _session_id_a = active_session_id(&panel, &cx);
        let thread_id_a = active_thread_id(&panel, &cx);
        send_message(&panel, &mut cx);
        cx.run_until_parked();

        panel.read_with(&cx, |panel, _cx| {
            assert_eq!(panel.selected_agent, stub_agent);
        });

        // Open thread B with a different agent — thread A goes to retained.
        let custom_agent = Agent::Custom {
            id: "my-custom-agent".into(),
        };
        let connection_b = StubAgentConnection::new()
            .with_agent_id("my-custom-agent".into())
            .with_telemetry_id("my-custom-agent".into());
        connection_b.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
            acp::ContentChunk::new("response b".into()),
        )]);
        open_thread_with_custom_connection(&panel, connection_b, &mut cx);
        send_message(&panel, &mut cx);
        cx.run_until_parked();

        panel.read_with(&cx, |panel, _cx| {
            assert_eq!(
                panel.selected_agent, custom_agent,
                "selected_agent should have changed to the custom agent"
            );
        });

        // Navigate back to thread A via load_agent_thread.
        panel.update_in(&mut cx, |panel, window, cx| {
            panel.load_agent_thread(
                stub_agent.clone(),
                thread_id_a,
                None,
                None,
                true,
                AgentThreadSource::AgentPanel,
                window,
                cx,
            );
        });

        panel.read_with(&cx, |panel, _cx| {
            assert_eq!(
                panel.selected_agent, stub_agent,
                "selected_agent should sync back to thread A's agent"
            );
        });
    }

    #[gpui::test]
    async fn test_classify_worktrees_skips_non_git_root_with_nested_repo(cx: &mut TestAppContext) {
        init_test(cx);
        cx.update(|cx| {
            agent::ThreadStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/repo_a",
            json!({
                ".git": {},
                "src": { "main.rs": "" }
            }),
        )
        .await;
        fs.insert_tree(
            "/repo_b",
            json!({
                ".git": {},
                "src": { "lib.rs": "" }
            }),
        )
        .await;
        // `plain_dir` is NOT a git repo, but contains a nested git repo.
        fs.insert_tree(
            "/plain_dir",
            json!({
                "nested_repo": {
                    ".git": {},
                    "src": { "lib.rs": "" }
                }
            }),
        )
        .await;

        let project = Project::test(
            fs.clone(),
            [
                Path::new("/repo_a"),
                Path::new("/repo_b"),
                Path::new("/plain_dir"),
            ],
            cx,
        )
        .await;

        // Let the worktree scanner discover all `.git` directories.
        cx.executor().run_until_parked();

        let multi_workspace =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));

        let workspace = multi_workspace
            .read_with(cx, |mw, _cx| mw.workspace().clone())
            .unwrap();

        let cx = &mut VisualTestContext::from_window(multi_workspace.into(), cx);

        let panel = workspace.update_in(cx, |workspace, window, cx| {
            cx.new(|cx| AgentPanel::new(workspace, window, cx))
        });

        cx.run_until_parked();

        panel.read_with(cx, |panel, cx| {
            let (git_repos, non_git_paths) =
                git_ui_core::worktree_service::classify_worktrees(panel.project.read(cx), cx);

            let git_work_dirs: Vec<PathBuf> = git_repos
                .iter()
                .map(|repo| repo.read(cx).work_directory_abs_path.to_path_buf())
                .collect();

            assert_eq!(
                git_repos.len(),
                2,
                "only repo_a and repo_b should be classified as git repos, \
                 but got: {git_work_dirs:?}"
            );
            assert!(
                git_work_dirs.contains(&PathBuf::from("/repo_a")),
                "repo_a should be in git_repos: {git_work_dirs:?}"
            );
            assert!(
                git_work_dirs.contains(&PathBuf::from("/repo_b")),
                "repo_b should be in git_repos: {git_work_dirs:?}"
            );

            assert_eq!(
                non_git_paths,
                vec![PathBuf::from("/plain_dir")],
                "plain_dir should be classified as a non-git path \
                 (not matched to nested_repo inside it)"
            );
        });
    }
    #[gpui::test]
    async fn test_vim_search_does_not_steal_focus_from_agent_panel(cx: &mut TestAppContext) {
        init_test(cx);
        cx.update(|cx| {
            agent::ThreadStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);
            vim::init(cx);
            search::init(cx);

            // Enable vim mode
            settings::SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |s| s.vim_mode = Some(true));
            });

            // Load vim keybindings
            let mut vim_key_bindings =
                settings::KeymapFile::load_asset_allow_partial_failure("keymaps/vim.json", cx)
                    .unwrap();
            for key_binding in &mut vim_key_bindings {
                key_binding.set_meta(settings::KeybindSource::Vim.meta());
            }
            cx.bind_keys(vim_key_bindings);
        });

        // Create a project with a file so we have a buffer in the center pane.
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/project", json!({ "file.txt": "hello world" }))
            .await;
        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;

        let multi_workspace =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace
            .read_with(cx, |mw, _cx| mw.workspace().clone())
            .unwrap();
        let mut cx = VisualTestContext::from_window(multi_workspace.into(), cx);

        // Open a file in the center pane.
        workspace
            .update_in(&mut cx, |workspace, window, cx| {
                workspace.open_paths(
                    vec![PathBuf::from("/project/file.txt")],
                    workspace::OpenOptions::default(),
                    None,
                    window,
                    cx,
                )
            })
            .await;
        cx.run_until_parked();

        // Add a BufferSearchBar to the center pane's toolbar, as a real
        // workspace would have.
        workspace.update_in(&mut cx, |workspace, window, cx| {
            workspace.active_pane().update(cx, |pane, cx| {
                pane.toolbar().update(cx, |toolbar, cx| {
                    let search_bar = cx.new(|cx| search::BufferSearchBar::new(None, window, cx));
                    toolbar.add_item(search_bar, window, cx);
                });
            });
        });

        // Create the agent panel and add it to the workspace.
        let panel = workspace.update_in(&mut cx, |workspace, window, cx| {
            let panel = cx.new(|cx| AgentPanel::new(workspace, window, cx));
            workspace.add_panel(panel.clone(), window, cx);
            panel
        });

        // Open a thread so the panel has an active editor.
        open_thread_with_connection(&panel, StubAgentConnection::new(), &mut cx);

        // Focus the agent panel.
        workspace.update_in(&mut cx, |workspace, window, cx| {
            workspace.focus_panel::<AgentPanel>(window, cx);
        });
        cx.run_until_parked();

        // Verify the agent panel has focus.
        workspace.update_in(&mut cx, |_, window, cx| {
            assert!(
                panel.read(cx).focus_handle(cx).contains_focused(window, cx),
                "Agent panel should be focused before pressing '/'"
            );
        });

        // Press '/' — the vim search keybinding.
        cx.simulate_keystrokes("/");

        // Focus should remain on the agent panel.
        workspace.update_in(&mut cx, |_, window, cx| {
            assert!(
                panel.read(cx).focus_handle(cx).contains_focused(window, cx),
                "Focus should remain on the agent panel after pressing '/'"
            );
        });
    }

    /// Connection that tracks closed sessions and detects prompts against
    /// sessions that no longer exist, used to reproduce session disassociation.
    #[derive(Clone, Default)]
    struct DisassociationTrackingConnection {
        next_session_number: Arc<Mutex<usize>>,
        sessions: Arc<Mutex<HashSet<acp::SessionId>>>,
        closed_sessions: Arc<Mutex<Vec<acp::SessionId>>>,
        missing_prompt_sessions: Arc<Mutex<Vec<acp::SessionId>>>,
    }

    impl DisassociationTrackingConnection {
        fn new() -> Self {
            Self::default()
        }

        fn create_session(
            self: Rc<Self>,
            session_id: acp::SessionId,
            project: Entity<Project>,
            work_dirs: PathList,
            title: Option<SharedString>,
            cx: &mut App,
        ) -> Entity<AcpThread> {
            self.sessions.lock().insert(session_id.clone());

            let action_log = cx.new(|_| ActionLog::new(project.clone()));
            cx.new(|cx| {
                AcpThread::new(
                    None,
                    title,
                    Some(work_dirs),
                    self,
                    project,
                    action_log,
                    session_id,
                    watch::Receiver::constant(
                        acp::PromptCapabilities::new()
                            .image(true)
                            .audio(true)
                            .embedded_context(true),
                    ),
                    cx,
                )
            })
        }
    }

    impl AgentConnection for DisassociationTrackingConnection {
        fn agent_id(&self) -> AgentId {
            agent::ZED_AGENT_ID.clone()
        }

        fn telemetry_id(&self) -> SharedString {
            "disassociation-tracking-test".into()
        }

        fn new_session(
            self: Rc<Self>,
            project: Entity<Project>,
            work_dirs: PathList,
            cx: &mut App,
        ) -> Task<Result<Entity<AcpThread>>> {
            let session_id = {
                let mut next_session_number = self.next_session_number.lock();
                let session_id = acp::SessionId::new(format!(
                    "disassociation-tracking-session-{}",
                    *next_session_number
                ));
                *next_session_number += 1;
                session_id
            };
            let thread = self.create_session(session_id, project, work_dirs, None, cx);
            Task::ready(Ok(thread))
        }

        fn supports_load_session(&self) -> bool {
            true
        }

        fn load_session(
            self: Rc<Self>,
            session_id: acp::SessionId,
            project: Entity<Project>,
            work_dirs: PathList,
            title: Option<SharedString>,
            cx: &mut App,
        ) -> Task<Result<Entity<AcpThread>>> {
            let thread = self.create_session(session_id, project, work_dirs, title, cx);
            thread.update(cx, |thread, cx| {
                thread
                    .handle_session_update(
                        acp::SessionUpdate::UserMessageChunk(acp::ContentChunk::new(
                            "Restored user message".into(),
                        )),
                        cx,
                    )
                    .expect("restored user message should be applied");
                thread
                    .handle_session_update(
                        acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                            "Restored assistant message".into(),
                        )),
                        cx,
                    )
                    .expect("restored assistant message should be applied");
            });
            Task::ready(Ok(thread))
        }

        fn supports_close_session(&self) -> bool {
            true
        }

        fn close_session(
            self: Rc<Self>,
            session_id: &acp::SessionId,
            _cx: &mut App,
        ) -> Task<Result<()>> {
            self.sessions.lock().remove(session_id);
            self.closed_sessions.lock().push(session_id.clone());
            Task::ready(Ok(()))
        }

        fn auth_methods(&self) -> &[acp::AuthMethod] {
            &[]
        }

        fn authenticate(&self, _method_id: acp::AuthMethodId, _cx: &mut App) -> Task<Result<()>> {
            Task::ready(Ok(()))
        }

        fn prompt(
            &self,
            params: acp::PromptRequest,
            _cx: &mut App,
        ) -> Task<Result<acp::PromptResponse>> {
            if !self.sessions.lock().contains(&params.session_id) {
                self.missing_prompt_sessions.lock().push(params.session_id);
                return Task::ready(Err(anyhow!("Session not found")));
            }

            Task::ready(Ok(acp::PromptResponse::new(acp::StopReason::EndTurn)))
        }

        fn cancel(&self, _session_id: &acp::SessionId, _cx: &mut App) {}

        fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
            self
        }
    }

    async fn setup_workspace_panel(
        cx: &mut TestAppContext,
    ) -> (Entity<Workspace>, Entity<AgentPanel>, VisualTestContext) {
        init_test(cx);
        cx.update(|cx| {
            agent::ThreadStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/project", json!({ "file.txt": "" })).await;
        cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;

        let multi_workspace =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));

        let workspace = multi_workspace
            .read_with(cx, |mw, _cx| mw.workspace().clone())
            .unwrap();

        let mut cx = VisualTestContext::from_window(multi_workspace.into(), cx);

        let panel = workspace.update_in(&mut cx, |workspace, window, cx| {
            let panel = cx.new(|cx| AgentPanel::new(workspace, window, cx));
            workspace.add_panel(panel.clone(), window, cx);
            panel
        });

        (workspace, panel, cx)
    }

    /// Reproduces the retained-thread reset race:
    ///
    /// 1. Thread A is active and Connected.
    /// 2. User switches to thread B → A goes to retained_threads.
    /// 3. A thread_error is set on retained A's thread view.
    /// 4. AgentServersUpdated fires → retained A's handle_agent_servers_updated
    ///    sees has_thread_error=true → calls reset() → close_all_sessions →
    ///    session X removed, state = Loading.
    /// 5. User reopens thread X via open_thread → load_agent_thread checks
    ///    retained A's has_session → returns false (state is Loading) →
    ///    creates new ConversationView C.
    /// 6. Both A's reload task and C's load task complete → both call
    ///    load_session(X) → both get Connected with session X.
    /// 7. A is eventually cleaned up → on_release → close_all_sessions →
    ///    removes session X.
    /// 8. C sends → "Session not found".
    #[gpui::test]
    async fn test_retained_thread_reset_race_disassociates_session(cx: &mut TestAppContext) {
        let (_workspace, panel, mut cx) = setup_workspace_panel(cx).await;
        cx.run_until_parked();

        let connection = DisassociationTrackingConnection::new();
        panel.update(&mut cx, |panel, cx| {
            panel.connection_store.update(cx, |store, cx| {
                store.restart_connection(
                    Agent::Stub,
                    Rc::new(StubAgentServer::new(connection.clone())),
                    cx,
                );
            });
        });
        cx.run_until_parked();

        // Step 1: Open thread A and send a message.
        panel.update_in(&mut cx, |panel, window, cx| {
            panel.external_thread(
                Some(Agent::Stub),
                None,
                None,
                None,
                None,
                true,
                AgentThreadSource::AgentPanel,
                window,
                cx,
            );
        });
        cx.run_until_parked();
        send_message(&panel, &mut cx);

        let session_id_a = active_session_id(&panel, &cx);
        let _thread_id_a = active_thread_id(&panel, &cx);

        // Step 2: Open thread B → A goes to retained_threads.
        panel.update_in(&mut cx, |panel, window, cx| {
            panel.external_thread(
                Some(Agent::Stub),
                None,
                None,
                None,
                None,
                true,
                AgentThreadSource::AgentPanel,
                window,
                cx,
            );
        });
        cx.run_until_parked();
        send_message(&panel, &mut cx);

        // ADR-F36 Phase B: パネルはタブ列なので、B を開いただけでは A のタブが
        // 残る。この試験は「保持枠にある A」の競合なので、明示的に閉じる。
        close_panel_tab(&panel, _thread_id_a, &mut cx);

        // Confirm A is retained.
        panel.read_with(&cx, |panel, _cx| {
            assert!(
                panel.test_thread_is_in_background(_thread_id_a),
                "thread A should be in retained_threads after switching to B"
            );
        });

        // Step 3: Set a thread_error on retained A's active thread view.
        // This simulates an API error that occurred before the user switched
        // away, or a transient failure.
        let retained_conversation_a = panel.read_with(&cx, |panel, _cx| {
            panel
                .test_background_view(_thread_id_a)
                .expect("thread A should be retained")
        });
        retained_conversation_a.update(&mut cx, |conversation, cx| {
            if let Some(thread_view) = conversation.active_thread() {
                thread_view.update(cx, |view, cx| {
                    view.handle_thread_error(
                        crate::conversation_view::ThreadError::Other {
                            message: "simulated error".into(),
                            acp_error_code: None,
                        },
                        cx,
                    );
                });
            }
        });

        // Confirm the thread error is set.
        retained_conversation_a.read_with(&cx, |conversation, cx| {
            let connected = conversation.as_connected().expect("should be connected");
            assert!(
                connected.has_thread_error(cx),
                "retained A should have a thread error"
            );
        });

        // Step 4: Emit AgentServersUpdated → retained A's
        // handle_agent_servers_updated sees has_thread_error=true,
        // calls reset(), which closes session X and sets state=Loading.
        //
        // Critically, we do NOT call run_until_parked between the emit
        // and open_thread. The emit's synchronous effects (event delivery
        // → reset() → close_all_sessions → state=Loading) happen during
        // the update's flush_effects. But the async reload task spawned
        // by initial_state has NOT been polled yet.
        panel.update(&mut cx, |panel, cx| {
            panel.project.update(cx, |project, cx| {
                project
                    .agent_server_store()
                    .update(cx, |_store, cx| cx.emit(project::AgentServersUpdated));
            });
        });
        // After this update returns, the retained ConversationView is in
        // Loading state (reset ran synchronously), but its async reload
        // task hasn't executed yet.

        // Step 5: Immediately open thread X via open_thread, BEFORE
        // the retained view's async reload completes. load_agent_thread
        // checks retained A's has_session → returns false (state is
        // Loading) → creates a NEW ConversationView C for session X.
        panel.update_in(&mut cx, |panel, window, cx| {
            panel.open_thread(session_id_a.clone(), None, None, window, cx);
        });

        // NOW settle everything: both async tasks (A's reload and C's load)
        // complete, both register session X.
        cx.run_until_parked();

        // Verify session A is the active session via C.
        panel.read_with(&cx, |panel, cx| {
            let active_session = panel
                .active_agent_thread(cx)
                .map(|t| t.read(cx).session_id().clone());
            assert_eq!(
                active_session,
                Some(session_id_a.clone()),
                "session A should be the active session after open_thread"
            );
        });

        // Step 6: Force the retained ConversationView A to be dropped
        // while the active view (C) still has the same session.
        // We can't use remove_thread because C shares the same ThreadId
        // and remove_thread would kill the active view too. Instead,
        // directly remove from retained_threads and drop the handle
        // so on_release → close_all_sessions fires only on A.
        drop(retained_conversation_a);
        panel.update(&mut cx, |panel, _cx| {
            panel.retained_threads.remove(&_thread_id_a);
        });
        cx.run_until_parked();

        // The key assertion: sending messages on the ACTIVE view (C)
        // must succeed. If the session was disassociated by A's cleanup,
        // this will fail with "Session not found".
        send_message(&panel, &mut cx);
        send_message(&panel, &mut cx);

        let missing = connection.missing_prompt_sessions.lock().clone();
        assert!(
            missing.is_empty(),
            "session should not be disassociated after retained thread reset race, \
             got missing prompt sessions: {:?}",
            missing
        );

        panel.read_with(&cx, |panel, cx| {
            let active_view = panel
                .active_conversation_view()
                .expect("conversation should remain open");
            let connected = active_view
                .read(cx)
                .as_connected()
                .expect("conversation should be connected");
            assert!(
                !connected.has_thread_error(cx),
                "conversation should not have a thread error"
            );
        });
    }

    #[gpui::test]
    async fn test_initialize_from_source_transfers_draft_to_fresh_panel(cx: &mut TestAppContext) {
        init_test(cx);
        cx.update(|cx| {
            agent::ThreadStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/project_a", json!({ "file.txt": "" }))
            .await;
        fs.insert_tree("/project_b", json!({ "file.txt": "" }))
            .await;
        let project_a = Project::test(fs.clone(), [Path::new("/project_a")], cx).await;
        let project_b = Project::test(fs.clone(), [Path::new("/project_b")], cx).await;

        let multi_workspace =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));

        let workspace_a = multi_workspace
            .read_with(cx, |mw, _cx| mw.workspace().clone())
            .unwrap();

        let workspace_b = multi_workspace
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.test_add_workspace(project_b.clone(), window, cx)
            })
            .unwrap();

        let cx = &mut VisualTestContext::from_window(multi_workspace.into(), cx);

        // Set up panel_a with an active thread and type draft text.
        let panel_a = workspace_a.update_in(cx, |workspace, window, cx| {
            let panel = cx.new(|cx| AgentPanel::new(workspace, window, cx));
            workspace.add_panel(panel.clone(), window, cx);
            panel
        });
        cx.run_until_parked();

        panel_a.update_in(cx, |panel, window, cx| {
            panel.open_external_thread_with_server(
                Rc::new(StubAgentServer::default_response()),
                window,
                cx,
            );
        });
        cx.run_until_parked();

        let thread_view_a =
            panel_a.read_with(cx, |panel, cx| panel.active_thread_view(cx).unwrap());
        let editor_a = thread_view_a.read_with(cx, |view, _cx| view.message_editor.clone());
        editor_a.update_in(cx, |editor, window, cx| {
            editor.set_text("Draft from workspace A", window, cx);
        });

        // Set up panel_b on workspace_b — starts as a fresh, empty panel.
        let panel_b = workspace_b.update_in(cx, |workspace, window, cx| {
            let panel = cx.new(|cx| AgentPanel::new(workspace, window, cx));
            workspace.add_panel(panel.clone(), window, cx);
            panel
        });
        cx.run_until_parked();

        // Initializing panel_b from workspace_a should transfer the draft,
        // even if panel_b already has an auto-created empty draft thread
        // (which set_active creates during add_panel).
        let transferred = panel_b.update_in(cx, |panel, window, cx| {
            panel.initialize_from_source_workspace_if_needed(workspace_a.downgrade(), window, cx)
        });
        assert!(
            transferred,
            "fresh destination panel should accept source content"
        );

        // Verify the panel was initialized: the base_view should now be an
        // AgentThread (not Uninitialized) and a draft_thread should be set.
        // We can't check the message editor text directly because the thread
        // needs a connected server session (not available in unit tests without
        // a stub server). The `transferred == true` return already proves that
        // source_panel_initialization read the content successfully.
        panel_b.read_with(cx, |panel, _cx| {
            assert!(
                panel.active_conversation_view().is_some(),
                "panel_b should have a conversation view after initialization"
            );
            assert!(
                panel.draft_thread.is_some(),
                "panel_b should have a draft_thread set after initialization"
            );
        });
    }

    #[gpui::test]
    async fn test_initialize_from_source_inherits_agent_without_draft_content(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        cx.update(|cx| {
            agent::ThreadStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/project_a", json!({ "file.txt": "" }))
            .await;
        fs.insert_tree("/project_b", json!({ "file.txt": "" }))
            .await;
        let project_a = Project::test(fs.clone(), [Path::new("/project_a")], cx).await;
        let project_b = Project::test(fs.clone(), [Path::new("/project_b")], cx).await;

        let multi_workspace =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));

        let workspace_a = multi_workspace
            .read_with(cx, |mw, _cx| mw.workspace().clone())
            .unwrap();

        let workspace_b = multi_workspace
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.test_add_workspace(project_b.clone(), window, cx)
            })
            .unwrap();

        let cx = &mut VisualTestContext::from_window(multi_workspace.into(), cx);

        let panel_a = workspace_a.update_in(cx, |workspace, window, cx| {
            let panel = cx.new(|cx| AgentPanel::new(workspace, window, cx));
            workspace.add_panel(panel.clone(), window, cx);
            panel
        });

        panel_a.update(cx, |panel, _cx| {
            panel.selected_agent = Agent::Stub;
        });

        let panel_b = workspace_b.update_in(cx, |workspace, window, cx| {
            let panel = cx.new(|cx| AgentPanel::new(workspace, window, cx));
            workspace.add_panel(panel.clone(), window, cx);
            panel
        });

        let initialized = panel_b.update_in(cx, |panel, window, cx| {
            panel.initialize_from_source_workspace_if_needed(workspace_a.downgrade(), window, cx)
        });
        assert!(
            initialized,
            "fresh destination panel should inherit the source agent"
        );

        panel_b.read_with(cx, |panel, _cx| {
            assert_eq!(
                panel.selected_agent,
                Agent::Stub,
                "destination panel should inherit the source panel's selected agent"
            );
            assert!(
                panel.active_conversation_view().is_none(),
                "agent-only initialization should not create a draft thread"
            );
        });
    }

    #[gpui::test]
    async fn test_initialize_from_source_retargets_empty_destination_draft_agent(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        cx.update(|cx| {
            agent::ThreadStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);
        });

        let fs = FakeFs::new(cx.executor());
        cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
        fs.insert_tree("/project_a", json!({ "file.txt": "" }))
            .await;
        fs.insert_tree("/project_b", json!({ "file.txt": "" }))
            .await;
        let project_a = Project::test(fs.clone(), [Path::new("/project_a")], cx).await;
        let project_b = Project::test(fs.clone(), [Path::new("/project_b")], cx).await;

        let multi_workspace =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));

        let workspace_a = multi_workspace
            .read_with(cx, |mw, _cx| mw.workspace().clone())
            .unwrap();

        let workspace_b = multi_workspace
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.test_add_workspace(project_b.clone(), window, cx)
            })
            .unwrap();

        let cx = &mut VisualTestContext::from_window(multi_workspace.into(), cx);

        let panel_a = workspace_a.update_in(cx, |workspace, window, cx| {
            let panel = cx.new(|cx| AgentPanel::new(workspace, window, cx));
            workspace.add_panel(panel.clone(), window, cx);
            panel
        });

        panel_a.update(cx, |panel, _cx| {
            panel.selected_agent = Agent::Stub;
        });

        let panel_b = workspace_b.update_in(cx, |workspace, window, cx| {
            let panel = cx.new(|cx| AgentPanel::new(workspace, window, cx));
            workspace.add_panel(panel.clone(), window, cx);
            panel
        });
        panel_b.update_in(cx, |panel, window, cx| {
            panel.activate_new_thread(false, AgentThreadSource::AgentPanel, window, cx);
        });

        let original_draft = panel_b.read_with(cx, |panel, cx| {
            let draft = panel.draft_thread.as_ref().expect("draft should exist");
            assert_eq!(
                *draft.read(cx).agent_key(),
                Agent::NativeAgent,
                "destination draft should start on the default agent"
            );
            draft.entity_id()
        });

        let initialized = panel_b.update_in(cx, |panel, window, cx| {
            panel.initialize_from_source_workspace_if_needed(workspace_a.downgrade(), window, cx)
        });
        assert!(
            initialized,
            "fresh destination draft should inherit the source agent"
        );

        panel_b.read_with(cx, |panel, cx| {
            let draft = panel.draft_thread.as_ref().expect("draft should exist");
            assert_ne!(
                draft.entity_id(),
                original_draft,
                "empty destination draft should be replaced when the inherited agent differs"
            );
            assert_eq!(
                *draft.read(cx).agent_key(),
                Agent::Stub,
                "empty destination draft should be rebound to the inherited agent"
            );
        });
    }

    #[gpui::test]
    async fn test_initialize_from_source_does_not_overwrite_existing_content(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        cx.update(|cx| {
            agent::ThreadStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/project_a", json!({ "file.txt": "" }))
            .await;
        fs.insert_tree("/project_b", json!({ "file.txt": "" }))
            .await;
        let project_a = Project::test(fs.clone(), [Path::new("/project_a")], cx).await;
        let project_b = Project::test(fs.clone(), [Path::new("/project_b")], cx).await;

        let multi_workspace =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));

        let workspace_a = multi_workspace
            .read_with(cx, |mw, _cx| mw.workspace().clone())
            .unwrap();

        let workspace_b = multi_workspace
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.test_add_workspace(project_b.clone(), window, cx)
            })
            .unwrap();

        let cx = &mut VisualTestContext::from_window(multi_workspace.into(), cx);

        // Set up panel_a with draft text.
        let panel_a = workspace_a.update_in(cx, |workspace, window, cx| {
            let panel = cx.new(|cx| AgentPanel::new(workspace, window, cx));
            workspace.add_panel(panel.clone(), window, cx);
            panel
        });
        cx.run_until_parked();

        panel_a.update_in(cx, |panel, window, cx| {
            panel.open_external_thread_with_server(
                Rc::new(StubAgentServer::default_response()),
                window,
                cx,
            );
        });
        cx.run_until_parked();

        let thread_view_a =
            panel_a.read_with(cx, |panel, cx| panel.active_thread_view(cx).unwrap());
        let editor_a = thread_view_a.read_with(cx, |view, _cx| view.message_editor.clone());
        editor_a.update_in(cx, |editor, window, cx| {
            editor.set_text("Draft from workspace A", window, cx);
        });

        // Set up panel_b with its OWN content — this is a non-fresh panel.
        let panel_b = workspace_b.update_in(cx, |workspace, window, cx| {
            let panel = cx.new(|cx| AgentPanel::new(workspace, window, cx));
            workspace.add_panel(panel.clone(), window, cx);
            panel
        });
        cx.run_until_parked();

        panel_b.update_in(cx, |panel, window, cx| {
            panel.open_external_thread_with_server(
                Rc::new(StubAgentServer::default_response()),
                window,
                cx,
            );
        });
        cx.run_until_parked();

        let thread_view_b =
            panel_b.read_with(cx, |panel, cx| panel.active_thread_view(cx).unwrap());
        let editor_b = thread_view_b.read_with(cx, |view, _cx| view.message_editor.clone());
        editor_b.update_in(cx, |editor, window, cx| {
            editor.set_text("Existing work in workspace B", window, cx);
        });

        // Attempting to initialize panel_b from workspace_a should be rejected
        // because panel_b already has meaningful content.
        let transferred = panel_b.update_in(cx, |panel, window, cx| {
            panel.initialize_from_source_workspace_if_needed(workspace_a.downgrade(), window, cx)
        });
        assert!(
            !transferred,
            "destination panel with existing content should not be overwritten"
        );

        // Verify panel_b still has its original content.
        panel_b.read_with(cx, |panel, cx| {
            let thread_view = panel
                .active_thread_view(cx)
                .expect("panel_b should still have its thread view");
            let text = thread_view.read(cx).message_editor.read(cx).text(cx);
            assert_eq!(
                text, "Existing work in workspace B",
                "destination panel's content should be preserved"
            );
        });
    }

    #[gpui::test]
    async fn test_create_thread_with_options_retains_thread_and_restores_agent(
        cx: &mut TestAppContext,
    ) {
        let (panel, mut cx) = setup_panel(cx).await;
        let _stub_connection =
            crate::test_support::set_stub_agent_connection(StubAgentConnection::new());

        // Baseline: panel's selected_agent is the stub.
        panel.update(&mut cx, |panel, _cx| {
            panel.selected_agent = Agent::Stub;
        });

        // Case 1: no agent override. The new thread should land in
        // `retained_threads` and `selected_agent` should be unchanged.
        let no_override_id = panel.update_in(&mut cx, |panel, window, cx| {
            panel.create_thread_with_options(
                CreateThreadOptions::default(),
                AgentThreadSource::AgentPanel,
                window,
                cx,
            )
        });

        panel.read_with(&cx, |panel, _cx| {
            assert!(
                panel.test_thread_is_in_background(no_override_id),
                "thread created via create_thread_with_options should be retained"
            );
            assert_eq!(
                panel.selected_agent,
                Agent::Stub,
                "selected_agent should be unchanged when no agent override is requested"
            );
        });

        // Case 2: an explicit agent override that differs from the panel's
        // selection. `create_agent_thread_inner` updates `selected_agent` as a
        // side effect; `create_thread_with_options` must restore it so the
        // user's last-used agent isn't silently flipped by an agent-initiated
        // call.
        let override_agent = Agent::Custom {
            id: "override-agent".into(),
        };
        let override_id = panel.update_in(&mut cx, |panel, window, cx| {
            panel.create_thread_with_options(
                CreateThreadOptions {
                    agent: Some(override_agent.clone()),
                    ..CreateThreadOptions::default()
                },
                AgentThreadSource::AgentPanel,
                window,
                cx,
            )
        });

        panel.read_with(&cx, |panel, _cx| {
            assert!(
                panel.test_thread_is_in_background(override_id),
                "thread created with an agent override should also be retained"
            );
            assert_ne!(
                no_override_id, override_id,
                "each call should produce a distinct ThreadId"
            );
            assert_eq!(
                panel.selected_agent,
                Agent::Stub,
                "selected_agent should be restored to the original after an agent override"
            );
        });
    }

    // -- ADR-F36 Phase B: パネル内レイアウトの永続化 -------------------------

    /// 分割の形とタブの並びが JSON を往復しても崩れないこと。
    #[test]
    fn serialized_panel_layout_round_trip() {
        let layout = SerializedAgentPanelLayout {
            root: SerializedAgentPaneGroup::Group {
                axis: SerializedAxis(Axis::Horizontal),
                flexes: Some(vec![1.0, 2.0]),
                children: vec![
                    SerializedAgentPaneGroup::Pane(SerializedAgentPane {
                        active: false,
                        items: vec![SerializedPanelItem {
                            kind: Arc::from("AgentThread"),
                            item_id: 3,
                            active: true,
                            preview: false,
                        }],
                    }),
                    SerializedAgentPaneGroup::Pane(SerializedAgentPane {
                        active: true,
                        items: vec![
                            SerializedPanelItem {
                                kind: Arc::from("AgentThread"),
                                item_id: 7,
                                active: false,
                                preview: false,
                            },
                            SerializedPanelItem {
                                kind: Arc::from("Terminal"),
                                item_id: 9,
                                active: true,
                                preview: false,
                            },
                        ],
                    }),
                ],
            },
        };

        let json = serde_json::to_string(&layout).expect("the layout should serialize");
        let restored: SerializedAgentPanelLayout =
            serde_json::from_str(&json).expect("the layout should survive");

        let SerializedAgentPaneGroup::Group {
            axis,
            flexes,
            children,
        } = restored.root
        else {
            panic!("the root should still be a split");
        };
        assert!(matches!(axis.0, Axis::Horizontal));
        assert_eq!(flexes, Some(vec![1.0, 2.0]));
        assert_eq!(children.len(), 2);

        let SerializedAgentPaneGroup::Pane(second) = &children[1] else {
            panic!("the second child should be a pane");
        };
        assert!(second.active, "the active pane should be remembered");
        assert_eq!(second.items.len(), 2);
        assert_eq!(&*second.items[1].kind, "Terminal");
        assert!(
            second.items[1].active,
            "a pane remembers which of its tabs was in front"
        );
    }

    fn stub_agent() -> Agent {
        Agent::Custom { id: "Test".into() }
    }

    fn stub_connection_entry(
        panel: &Entity<AgentPanel>,
        cx: &VisualTestContext,
    ) -> Option<Entity<crate::agent_connection_store::AgentConnectionEntry>> {
        panel.read_with(cx, |panel, cx| {
            panel
                .connection_store()
                .read(cx)
                .entry(&stub_agent())
                .cloned()
        })
    }

    fn stub_connection(
        panel: &Entity<AgentPanel>,
        cx: &VisualTestContext,
    ) -> Rc<dyn AgentConnection> {
        let entry = stub_connection_entry(panel, cx).expect("the stub agent should be connected");
        entry.read_with(cx, |entry, _cx| match entry {
            crate::agent_connection_store::AgentConnectionEntry::Connected(state) => {
                state.connection.clone()
            }
            _ => panic!("the stub agent should be connected"),
        })
    }

    /// Opens a stub agent's thread in a panel the workspace knows about (so
    /// that closing a tab can hand the thread back), on a stub whose sessions
    /// can be read again — which is what lets a parked thread be let go of.
    ///
    /// Something is sent in it on purpose. An empty draft holds no agent at
    /// all now (ADR-F49, `ServerState::Parked`), so a thread nothing was ever
    /// sent in would take that shortcut instead of the parking slot these
    /// tests are about.
    async fn setup_panel_with_a_stub_thread(
        cx: &mut TestAppContext,
    ) -> (Entity<AgentPanel>, ThreadId, VisualTestContext) {
        let (panel, mut cx) = setup_visible_panel(cx).await;
        let connection = StubAgentConnection::new().with_supports_load_session(true);
        connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
            acp::ContentChunk::new("応答".into()),
        )]);
        open_thread_with_connection(&panel, connection, &mut cx);
        send_message(&panel, &mut cx);
        let thread_id = active_thread_id(&panel, &cx);
        (panel, thread_id, cx)
    }

    /// Closes one of the panel's thread tabs, the way its close button does:
    /// the view is handed back to the panel's parking slot rather than
    /// dropped.
    fn close_the_tab_of(
        panel: &Entity<AgentPanel>,
        thread_id: ThreadId,
        cx: &mut VisualTestContext,
    ) {
        let pane = panel.read_with(cx, |panel, _cx| panel.active_pane().clone());
        let item_id = pane.read_with(cx, |pane, cx| {
            pane.items_of_type::<crate::ThreadItem>()
                .find(|item| item.read(cx).thread_id() == thread_id)
                .expect("the thread should be in one of the panel's tabs")
                .entity_id()
        });
        pane.update_in(cx, |pane, window, cx| {
            pane.remove_item(item_id, false, false, window, cx);
        });
        cx.run_until_parked();
    }

    /// Moves the panel on to Zed's own agent, so that nothing of the stub
    /// agent is in front any more.
    fn move_on_to_the_native_agent(panel: &Entity<AgentPanel>, cx: &mut VisualTestContext) {
        panel.update_in(cx, |panel, window, cx| {
            panel.test_set_selected_agent(Agent::NativeAgent);
            panel.activate_draft(true, AgentThreadSource::AgentPanel, window, cx);
        });
        cx.run_until_parked();
    }

    /// ADR-F49 (new decision): closing a thread only parks it, so the agent in
    /// its sandbox went on running until the window closed. Once the grace has
    /// passed with no thread of that agent open, the connection goes — and the
    /// parked view that was holding it goes with it, or the agent would never
    /// hear about it.
    #[gpui::test]
    async fn test_a_connection_nothing_uses_is_dropped_after_the_grace(cx: &mut TestAppContext) {
        let (panel, thread_id, mut cx) = setup_panel_with_a_stub_thread(cx).await;
        let cx = &mut cx;
        assert!(
            stub_connection_entry(&panel, cx).is_some(),
            "opening a thread connects to its agent"
        );

        move_on_to_the_native_agent(&panel, cx);
        assert!(
            stub_connection_entry(&panel, cx).is_some(),
            "the stub agent's thread is still open in its own tab"
        );

        close_the_tab_of(&panel, thread_id, cx);
        panel.read_with(cx, |panel, _cx| {
            assert!(
                panel.test_retains_thread(thread_id),
                "a closed thread is parked in the panel, not dropped"
            );
        });

        cx.executor().advance_clock(Duration::from_secs(4 * 60));
        cx.run_until_parked();
        assert!(
            stub_connection_entry(&panel, cx).is_some(),
            "the connection is kept for the whole grace, so opening the thread              again within it costs nothing"
        );
        assert!(panel.read_with(cx, |panel, _cx| panel.test_retains_thread(thread_id)));

        cx.executor().advance_clock(Duration::from_secs(2 * 60));
        cx.run_until_parked();
        assert!(
            stub_connection_entry(&panel, cx).is_none(),
            "after the grace the unused connection is dropped"
        );
        assert!(
            !panel.read_with(cx, |panel, _cx| panel.test_retains_thread(thread_id)),
            "the parked view has to let go too, or the agent would keep running"
        );
    }

    /// ADR-F49 (new decision): opening the thread again within the grace takes
    /// the connection back rather than starting the agent — and signing in —
    /// anew.
    #[gpui::test]
    async fn test_reopening_a_thread_within_the_grace_keeps_its_connection(
        cx: &mut TestAppContext,
    ) {
        let (panel, thread_id, mut cx) = setup_panel_with_a_stub_thread(cx).await;
        let cx = &mut cx;
        let connection = stub_connection(&panel, cx);

        move_on_to_the_native_agent(&panel, cx);
        close_the_tab_of(&panel, thread_id, cx);
        cx.executor().advance_clock(Duration::from_secs(4 * 60));
        cx.run_until_parked();

        panel.update_in(cx, |panel, window, cx| {
            panel.activate_retained_thread(thread_id, true, window, cx);
        });
        cx.run_until_parked();

        cx.executor().advance_clock(Duration::from_secs(10 * 60));
        cx.run_until_parked();
        assert!(
            Rc::ptr_eq(&connection, &stub_connection(&panel, cx)),
            "a thread that was opened again keeps the connection it had"
        );
    }

    /// 旧形式 (レイアウトが無く、最後に見ていたスレッドが 1 本だけ書いてある)
    /// を 1 世代だけ読めること。読めた thread_id はタブ 1 枚として復元される。
    #[test]
    fn legacy_serialized_panel_without_a_layout_still_reads() {
        let thread_id = ThreadId::new();
        let legacy = serde_json::json!({
            "selected_agent": Agent::NativeAgent,
            "last_active_thread": {
                "session_id": "session-1",
                "thread_id": thread_id,
                "agent_type": Agent::NativeAgent,
                "title": "むかしのスレッド",
                "work_dirs": null,
            },
        });

        let restored: SerializedAgentPanel =
            serde_json::from_value(legacy).expect("the old panel record should still parse");
        let thread = restored
            .last_active_thread
            .expect("the old record carries the single active thread");
        assert_eq!(thread.thread_id, Some(thread_id));
        assert_eq!(thread.session_id.as_deref(), Some("session-1"));
        assert_eq!(thread.agent_type, Agent::NativeAgent);
    }
}
