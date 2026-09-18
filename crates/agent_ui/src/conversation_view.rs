use acp_thread::{
    AcpThread, AcpThreadEvent, AgentThreadEntry, AssistantMessage, AssistantMessageChunk,
    AuthRequired, ClientUserMessageId, ElicitationEntryId, ElicitationStatus, ElicitationStore,
    LoadError, MaxOutputTokensError, MentionUri, PermissionOptionChoice, PermissionOptions,
    PermissionPattern, RetryStatus, SelectedPermissionOutcome, ThreadStatus, ToolCall,
    ToolCallContent, ToolCallStatus,
};
use acp_thread::{AgentConnection, Plan};
use action_log::{ActionLog, ActionLogTelemetry, DiffStats};
use agent::{NativeAgentServer, NoModelConfiguredError, ThreadStore};
use agent_client_protocol::schema::v1 as acp;
#[cfg(test)]
use agent_servers::AgentServerDelegate;
use agent_servers::{AcpConnection, AgentServer, GEMINI_TERMINAL_AUTH_METHOD_ID};
use agent_settings::{AgentProfileId, AgentSettings};
use anyhow::{Result, anyhow};
#[cfg(feature = "audio")]
use audio::{Audio, Sound};
use buffer_diff::BufferDiff;
use client::zed_urls;
use collections::{HashMap, HashSet, IndexMap};
use editor::scroll::Autoscroll;
use editor::{
    Editor, EditorEvent, EditorMode, MultiBuffer, PathKey, SelectionEffects, SizingBehavior,
};
use file_icons::FileIcons;
use fs::Fs;
use futures::FutureExt as _;
use gpui::{
    Action, Animation, AnimationExt, AnyWindowHandle, App, ClickEvent, ClipboardItem, CursorStyle,
    ElementId, Empty, Entity, EventEmitter, FocusHandle, Focusable, Hsla, ListOffset, ListState,
    ObjectFit, PlatformDisplay, PromptButton, PromptLevel, ScrollHandle, SharedString, StyledText,
    Subscription, Task, TextRun, TextStyle, WeakEntity, Window, WindowHandle, div, ease_in_out,
    img, linear_color_stop, linear_gradient, list, pulsating_between,
};
use language::{Buffer, Language, Rope};
use language_model::LanguageModelCompletionError;
use markdown::{
    CodeBlockRenderer, CopyButtonVisibility, Markdown, MarkdownElement, MarkdownFont, MarkdownStyle,
};
use parking_lot::{Mutex, RwLock};
use project::{AgentId, AgentServerStore, Project, ProjectEntryId, ProjectPath};

use crate::conversation_view::elicitation::{
    ElicitationCard, ElicitationCardHandlers, ElicitationFormState, should_render_elicitation,
};
use crate::message_editor::SessionCapabilities;
use crate::{AgentThreadSource, DEFAULT_THREAD_TITLE, resolve_agent_image};
use lru::LruCache;
use rope::Point;
use settings::{NotifyWhenAgentWaiting, Settings as _, SettingsStore};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use std::{rc::Rc, time::Duration};
use terminal_view::terminal_panel::TerminalPanel;
use text::Anchor;
use theme_settings::{AgentBufferFontSize, AgentUiFontSize};
use ui::{
    Callout, CircularProgress, CommonAnimationExt, ConfiguredApiCard, ContextMenu,
    ContextMenuEntry, CopyButton, DecoratedIcon, DiffStat, Disclosure, Divider, DividerColor,
    IconDecoration, IconDecorationKind, KeyBinding, PopoverMenu, PopoverMenuHandle, TintColor,
    Tooltip, WithScrollbar, prelude::*, right_click_menu,
};
use util::{
    ResultExt, debug_panic, defer,
    paths::{PathStyle, PathWithPosition},
    rel_path::RelPath,
    size::format_file_size,
    time::duration_alt_display,
};
use workspace::{
    CollaboratorId, MultiWorkspace, NewTerminal, PathList, Workspace, path_link::sanitize_path_text,
};
use zed_actions::agent::{Chat, ToggleModelSelector};

use super::config_options::ConfigOptionsView;
use super::entry_view_state::EntryViewState;
use crate::ModeSelector;
use crate::ModelSelectorPopover;
use crate::agent_connection_store::{
    AgentConnectedState, AgentConnectionEntryEvent, AgentConnectionStore,
    OutsideSandboxConnectionId,
};
use crate::agent_diff::AgentDiff;
use crate::completion_provider::{AgentContextSelection, AvailableSkill};
use crate::entry_view_state::{EntryViewEvent, ViewEvent};
use crate::message_editor::{InputAttempt, MessageEditor, MessageEditorEvent};
use crate::profile_selector::{ProfileProvider, ProfileSelector};

use crate::thread_item::ThreadItem;
use crate::thread_metadata_store::{ThreadId, ThreadMetadataStore};
use crate::ui::{AgentNotification, AgentNotificationEvent};
use crate::{
    Agent, AgentDiffPane, AgentInitialContent, AgentPanel, AgentPanelEvent, AllowAlways, AllowOnce,
    AuthorizeToolCall, ClearMessageQueue, CycleFavoriteModels, CycleModeSelector,
    CycleThinkingEffort, EditFirstQueuedMessage, ExpandMessageEditor, Follow, KeepAll, NewThread,
    OpenAddContextMenu, OpenAgentDiff, ReconnectParkedAgent, RejectAll, RejectOnce,
    RemoveFirstQueuedMessage, ScrollOutputLineDown, ScrollOutputLineUp, ScrollOutputPageDown,
    ScrollOutputPageUp, ScrollOutputToBottom, ScrollOutputToNextMessage,
    ScrollOutputToPreviousMessage, ScrollOutputToTop, SendImmediately, SendNextQueuedMessage,
    ToggleAutoApproveToolActions, ToggleFastMode, ToggleProfileSelector,
    ToggleSteerFirstQueuedMessage, ToggleThinkingEffortMenu, ToggleThinkingMode,
    ToggleToolPermissionSelector, UndoLastReject,
};

const STOPWATCH_THRESHOLD: Duration = Duration::from_secs(30);
const TOKEN_THRESHOLD: u64 = 250;

pub(crate) const DRAFT_PROMPT_PERSIST_DEBOUNCE: Duration = Duration::from_millis(250);

const START_OUTSIDE_SANDBOX_LABEL: &str = "サンドボックスの外で起動 (今回のみ)";
const OUTSIDE_SANDBOX_RUNNING_LABEL: &str =
    "このスレッドのエージェントはサンドボックスの外で実行中です";
const OUTSIDE_SANDBOX_EXPLANATION: &str = "サンドボックスの外で起動すると、このエージェントは \
     Zed 本体の権限で動き、プロジェクトのフックや MCP サーバーも実行できます。起動の前に毎回確認します。";
const AGENT_HOSTS_MESSAGE: &str = "エージェントの通信に許可リストへの追加が必要です";
const ADD_AGENT_HOSTS_LABEL: &str = "許可リストに追加して起動";
/// The same button after a consent that did not carry through: by then the
/// hosts are usually already in `network_allowlist.json` and only the running
/// policy is behind, so "追加して" would send the user looking for a problem
/// that is not there.
const RETRY_AGENT_HOSTS_LABEL: &str = "もう一度試す";
const DECLINE_AGENT_HOSTS_LABEL: &str = "追加せず中止";
/// ADR-F49 (new decision): the way back on a card that otherwise offers none
/// — see [`ConversationView::render_load_error`]. Worded as the allowlist
/// card's second attempt is: both mean "start it again as it stands".
const RETRY_AGENT_START_LABEL: &str = "もう一度試す";
/// How a test finds that button in the frame the view just drew.
const RETRY_AGENT_START_SELECTOR: &str = "RETRY-AGENT-START";

/// ADR-F55: サブスクリプションでのサインインを設定で切っているエージェントで、
/// それでも要求が届いたときに断る文言。
const SUBSCRIPTION_SIGN_IN_REFUSED: &str = "サブスクリプションでのサインインは、\
     このエージェントの設定 (`allow_subscription_sign_in`) で無効にされています。";

/// ADR-F55: 「Claude Code でログイン (サブスクリプション)」。エージェントが
/// 出す英語のラベルの代わりにこちらを出す (未改変の CLI 自身が Anthropic の
/// 画面でサインインする、という導線であることを日本語で言い切るため)。
const SUBSCRIPTION_SIGN_IN_LABEL: &str = "Claude Code でログイン (サブスクリプション)";

/// How a test finds the fork's own part of an agent's authentication card —
/// the note about a subscription and the API key field — in the frame the
/// view just drew.
const FORK_AUTH_NOTICE_SELECTOR: &str = "AUTH-FORK-NOTICE";

/// How a test finds the button that starts the official CLI, which the card
/// offers only where the agent advertises no sign-in of its own.
const LAUNCH_CLAUDE_CODE_SELECTOR: &str = "AUTH-LAUNCH-CLAUDE-CODE";

/// How a test finds the line the card adds when a key and a subscription
/// sign-in are both on offer, which says which of the two the CLI takes.
const API_KEY_WINS_SELECTOR: &str = "AUTH-API-KEY-WINS";

/// ADR-F53 決定 4 / ADR-F55: 同じカードの API キー入力欄・保存ボタン・
/// 「設定済み」の表示・「キーを削除」を、テストがその場のフレームから
/// 見つけるための目印。
const API_KEY_INPUT_SELECTOR: &str = "AUTH-API-KEY-INPUT";
const API_KEY_SAVE_SELECTOR: &str = "AUTH-API-KEY-SAVE";
const API_KEY_CONFIGURED_SELECTOR: &str = "AUTH-API-KEY-CONFIGURED";
const API_KEY_REMOVE_SELECTOR: &str = "AUTH-API-KEY-REMOVE";
const API_KEY_CANCEL_SELECTOR: &str = "AUTH-API-KEY-CANCEL";
/// 保存中は「保存」の代わりにこちらが出るので、目印も別にする: テストは
/// 「押せない」ことを disabled 属性ではなく、どちらが出ているかで見る。
const API_KEY_SAVING_SELECTOR: &str = "AUTH-API-KEY-SAVING";

/// ADR-F55: 認証カードに出す注記を決める、サブスクリプションのサインインの
/// 状態。「出ている」と「出せない」を混ぜない: 切ったのか、エージェントが
/// そもそも出していないのかで、利用者が次にすることが違うため。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SubscriptionSignIn {
    /// カードにサインインのボタンが出ている。
    Offered,
    /// 設定 (`allow_subscription_sign_in: false`) で落とした。
    TurnedOff,
    /// エージェントがサインインのメソッドを出していない。
    NotAdvertised,
}

impl SubscriptionSignIn {
    /// How a test tells which of the three notices the card drew: the wording
    /// differs by state and a bounds lookup cannot read it, so the state is
    /// part of the mark.
    fn notice_selector(self) -> &'static str {
        match self {
            Self::Offered => "AUTH-FORK-NOTICE-OFFERED",
            Self::TurnedOff => "AUTH-FORK-NOTICE-TURNED-OFF",
            Self::NotAdvertised => "AUTH-FORK-NOTICE-NOT-ADVERTISED",
        }
    }

    /// 認証カードの 1 行の注記。3 つを別々の文言にするのは、利用者が次にする
    /// ことが違うため: 出ているなら押す、切ってあるなら設定を戻す、出ていない
    /// なら公式の CLI でログインする。
    fn notice(self) -> &'static str {
        match self {
            Self::Offered => {
                "サブスクリプション (Claude Pro / Max など) での利用は、\
                 Anthropic が 2026-06-15 に「これまでどおり」と告知しています。\
                 設定で無効にできます。"
            }
            Self::TurnedOff => {
                "サブスクリプションでのサインインは、設定の\
                 `allow_subscription_sign_in` で無効にしています。\
                 API キーでの利用はこのまま使えます。"
            }
            Self::NotAdvertised => {
                "このエージェントはサブスクリプションでのサインインを\
                 出していません。サブスクリプションで使うには、公式の\
                 Claude Code を起動してその画面でログインしてください。"
            }
        }
    }
}

/// The fixed strings a finished sign-in prints, which the terminal running it
/// is watched for.
///
/// ADR-F55 「やらないこと」: サインインの端末の出力はこの固定文字列と
/// 照合するだけで、中身を解析しない・取り出さない・保存しない。トークンは
/// CLI が自分の設定フォルダーへ書くもので、こちらは触れない。切り出して
/// あるのは、それをテストで固定できるようにするため。
fn login_success_patterns(method_id: &str) -> Vec<String> {
    match method_id {
        agent_servers::CLAUDE_SUBSCRIPTION_AUTH_METHOD | GEMINI_TERMINAL_AUTH_METHOD_ID => {
            vec![
                "Login successful".to_string(),
                "Type your message".to_string(),
            ]
        }
        _ => Vec::new(),
    }
}

/// How a test finds one authentication method's button in the frame the view
/// just drew, so that a method the card must not offer can be shown absent.
fn auth_method_selector(method_id: &str) -> String {
    format!("AUTH-METHOD-{method_id}")
}

/// ADR-F49: the consent's detail, as for the sandboxed Claude Code CLI: the
/// hosts go into the IDE-wide allowlist, where the built-in agent's commands
/// can reach them too.
fn agent_hosts_detail(agent: &AgentId, hosts: &[SharedString], missing: &[SharedString]) -> String {
    let hosts = hosts
        .iter()
        .map(|host| {
            let state = if missing.contains(host) {
                "未登録"
            } else {
                "登録済み"
            };
            format!("{host} ({state})")
        })
        .collect::<Vec<_>>()
        .join("、");
    format!(
        "サンドボックスの {agent} は次のホストにだけ接続します。これらのホストは IDE 全体の\
         許可リストに追加し、内蔵エージェントのコマンドにも適用されます。\n許可が必要なホスト: {hosts}"
    )
}

pub(crate) mod elicitation;
mod message_queue;
mod thread_search_bar;
mod thread_view;
pub use message_queue::*;
pub use thread_view::*;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum ThreadFeedback {
    Positive,
    Negative,
}

#[derive(Debug)]
pub(crate) enum ThreadError {
    PaymentRequired,
    DataRetentionConsentRequired,
    Refusal,
    AuthenticationRequired(SharedString),
    RateLimitExceeded {
        provider: SharedString,
    },
    ServerOverloaded {
        provider: SharedString,
    },
    PromptTooLarge,
    NoCredentials {
        provider: SharedString,
    },
    StreamError {
        provider: SharedString,
    },
    AuthenticationFailed {
        provider: SharedString,
    },
    PermissionDenied {
        provider: SharedString,
        message: Option<SharedString>,
    },
    RequestFailed,
    MaxOutputTokens,
    NoModelSelected,
    ApiError {
        provider: SharedString,
    },
    Other {
        message: SharedString,
        acp_error_code: Option<SharedString>,
    },
}

impl From<anyhow::Error> for ThreadError {
    fn from(error: anyhow::Error) -> Self {
        if error.is::<MaxOutputTokensError>() {
            Self::MaxOutputTokens
        } else if error.is::<NoModelConfiguredError>() {
            Self::NoModelSelected
        } else if let Some(acp_error) = error.downcast_ref::<acp::Error>()
            && acp_error.code == acp::ErrorCode::AuthRequired
        {
            Self::AuthenticationRequired(acp_error.message.clone().into())
        } else if let Some(lm_error) = error.downcast_ref::<LanguageModelCompletionError>() {
            use LanguageModelCompletionError::*;
            match lm_error {
                RateLimitExceeded { provider, .. } => Self::RateLimitExceeded {
                    provider: provider.to_string().into(),
                },
                ServerOverloaded { provider, .. } | ApiInternalServerError { provider, .. } => {
                    Self::ServerOverloaded {
                        provider: provider.to_string().into(),
                    }
                }
                PromptTooLarge { .. } => Self::PromptTooLarge,
                PaymentRequired => Self::PaymentRequired,
                NoApiKey { provider } => Self::NoCredentials {
                    provider: provider.to_string().into(),
                },
                StreamEndedUnexpectedly { provider }
                | ApiReadResponseError { provider, .. }
                | DeserializeResponse { provider, .. }
                | HttpSend { provider, .. } => Self::StreamError {
                    provider: provider.to_string().into(),
                },
                AuthenticationError { provider, .. } => Self::AuthenticationFailed {
                    provider: provider.to_string().into(),
                },
                PermissionError { provider, message } => Self::PermissionDenied {
                    provider: provider.to_string().into(),
                    message: Some(message.clone().into()),
                },
                UpstreamProviderError { .. } => Self::RequestFailed,
                DataRetentionConsentRequired { .. } => Self::DataRetentionConsentRequired,
                BadRequestFormat { provider, .. }
                | HttpResponseError { provider, .. }
                | ApiEndpointNotFound { provider } => Self::ApiError {
                    provider: provider.to_string().into(),
                },
                _ => {
                    let message: SharedString = format!("{:#}", error).into();
                    Self::Other {
                        message,
                        acp_error_code: None,
                    }
                }
            }
        } else {
            let message: SharedString = format!("{:#}", error).into();

            // Extract ACP error code if available
            let acp_error_code = error
                .downcast_ref::<acp::Error>()
                .map(|acp_error| SharedString::from(acp_error.code.to_string()));

            Self::Other {
                message,
                acp_error_code,
            }
        }
    }
}

impl ProfileProvider for Entity<agent::Thread> {
    fn profile_id(&self, cx: &App) -> AgentProfileId {
        self.read(cx).profile(cx)
    }

    fn set_profile(&self, profile_id: AgentProfileId, cx: &mut App) {
        self.update(cx, |thread, cx| {
            // Apply the profile and let the thread swap to its default model.
            thread.set_profile(profile_id, cx);
        });
    }

    fn profiles_supported(&self, cx: &App) -> bool {
        self.read(cx)
            .model()
            .is_some_and(|model| model.supports_tools())
    }

    fn model_selected(&self, cx: &App) -> bool {
        self.read(cx).model().is_some()
    }

    fn is_restricted(&self, cx: &App) -> bool {
        project::trusted_worktrees::TrustedWorktrees::has_restricted_worktrees(
            &self.read(cx).project().read(cx).worktree_store(),
            cx,
        )
    }

    fn profile_downgraded(&self, cx: &App) -> bool {
        self.read(cx).profile_was_downgraded(cx)
    }
}

#[derive(Default)]
pub(crate) struct Conversation {
    threads: HashMap<acp::SessionId, Entity<AcpThread>>,
    permission_requests: IndexMap<acp::SessionId, Vec<acp::ToolCallId>>,
    elicitation_requests: IndexMap<acp::SessionId, Vec<ElicitationEntryId>>,
    subscriptions: Vec<Subscription>,
    updated_at: Option<Instant>,
}

impl Conversation {
    pub fn register_thread(&mut self, thread: Entity<AcpThread>, cx: &mut Context<Self>) {
        let session_id = thread.read(cx).session_id().clone();
        let subscription = cx.subscribe(&thread, {
            let session_id = session_id.clone();
            move |this, _thread, event, _cx| {
                this.updated_at = Some(Instant::now());
                match event {
                    AcpThreadEvent::ToolAuthorizationRequested(id) => {
                        this.permission_requests
                            .entry(session_id.clone())
                            .or_default()
                            .push(id.clone());
                    }
                    AcpThreadEvent::ToolAuthorizationReceived(id) => {
                        if let Some(tool_calls) = this.permission_requests.get_mut(&session_id) {
                            tool_calls.retain(|tool_call_id| tool_call_id != id);
                            if tool_calls.is_empty() {
                                this.permission_requests.shift_remove(&session_id);
                            }
                        }
                    }
                    AcpThreadEvent::ElicitationRequested(id) => {
                        this.elicitation_requests
                            .entry(session_id.clone())
                            .or_default()
                            .push(id.clone());
                    }
                    AcpThreadEvent::ElicitationResponded(id) => {
                        if let Some(elicitations) = this.elicitation_requests.get_mut(&session_id) {
                            elicitations.retain(|elicitation_id| elicitation_id != id);
                            if elicitations.is_empty() {
                                this.elicitation_requests.shift_remove(&session_id);
                            }
                        }
                    }
                    AcpThreadEvent::NewEntry
                    | AcpThreadEvent::StatusChanged
                    | AcpThreadEvent::TitleUpdated
                    | AcpThreadEvent::TokenUsageUpdated
                    | AcpThreadEvent::EntryUpdated(_)
                    | AcpThreadEvent::EntriesRemoved(_)
                    | AcpThreadEvent::Retry(_)
                    | AcpThreadEvent::SubagentSpawned(_)
                    | AcpThreadEvent::Stopped(_)
                    | AcpThreadEvent::Error
                    | AcpThreadEvent::LoadError(_)
                    | AcpThreadEvent::PromptCapabilitiesUpdated
                    | AcpThreadEvent::Refusal
                    | AcpThreadEvent::AvailableCommandsUpdated(_)
                    | AcpThreadEvent::ModeUpdated(_)
                    | AcpThreadEvent::ConfigOptionsUpdated(_)
                    | AcpThreadEvent::WorkingDirectoriesUpdated
                    | AcpThreadEvent::PromptUpdated => {}
                }
            }
        });
        self.subscriptions.push(subscription);
        self.threads.insert(session_id, thread);
    }

    pub fn permission_options_for_tool_call<'a>(
        &'a self,
        session_id: &acp::SessionId,
        tool_call_id: acp::ToolCallId,
        cx: &'a App,
    ) -> Option<&'a PermissionOptions> {
        let thread = self.threads.get(session_id)?;
        let (_, tool_call) = thread.read(cx).tool_call(&tool_call_id)?;
        let ToolCallStatus::WaitingForConfirmation { options, .. } = &tool_call.status else {
            return None;
        };
        Some(options)
    }

    pub fn pending_tool_call<'a>(
        &'a self,
        session_id: &acp::SessionId,
        cx: &'a App,
    ) -> Option<(acp::SessionId, acp::ToolCallId, &'a PermissionOptions)> {
        let thread = self.threads.get(session_id)?;
        let is_subagent = thread.read(cx).parent_session_id().is_some();
        let (result_session_id, thread, tool_id) = if is_subagent {
            let id = self.permission_requests.get(session_id)?.iter().next()?;
            (session_id.clone(), thread, id)
        } else {
            let (id, tool_calls) = self.permission_requests.first()?;
            let thread = self.threads.get(id)?;
            let tool_id = tool_calls.iter().next()?;
            (id.clone(), thread, tool_id)
        };
        let (_, tool_call) = thread.read(cx).tool_call(tool_id)?;

        let ToolCallStatus::WaitingForConfirmation { options, .. } = &tool_call.status else {
            return None;
        };
        Some((result_session_id, tool_id.clone(), options))
    }

    pub fn subagents_awaiting_permission(&self, cx: &App) -> Vec<(acp::SessionId, usize)> {
        self.permission_requests
            .iter()
            .filter_map(|(session_id, tool_call_ids)| {
                let thread = self.threads.get(session_id)?;
                if thread.read(cx).parent_session_id().is_some() && !tool_call_ids.is_empty() {
                    Some((session_id.clone(), tool_call_ids.len()))
                } else {
                    None
                }
            })
            .collect()
    }

    /// Returns the first pending tool call request for exactly `session_id`.
    /// Unlike `pending_tool_call`, this does not use the global FIFO pending
    /// request for non-subagent sessions.
    pub fn pending_tool_call_for_session(
        &self,
        session_id: &acp::SessionId,
        cx: &App,
    ) -> Option<acp::ToolCallId> {
        let thread = self.threads.get(session_id)?;
        let tool_call_id = self.permission_requests.get(session_id)?.iter().next()?;
        let (_, tool_call) = thread.read(cx).tool_call(tool_call_id)?;
        if !matches!(
            tool_call.status,
            ToolCallStatus::WaitingForConfirmation { .. }
        ) {
            return None;
        }
        Some(tool_call_id.clone())
    }

    pub fn pending_tool_call_count_for_session(&self, session_id: &acp::SessionId) -> usize {
        self.permission_requests
            .get(session_id)
            .map(|tool_call_ids| tool_call_ids.len())
            .unwrap_or(0)
    }

    pub fn respond_to_elicitation(
        &mut self,
        session_id: acp::SessionId,
        elicitation_id: ElicitationEntryId,
        response: acp::CreateElicitationResponse,
        cx: &mut Context<Self>,
    ) -> Option<()> {
        let thread = self.threads.get(&session_id)?.clone();
        thread.update(cx, |thread, cx| {
            thread.respond_to_elicitation(&elicitation_id, response, cx);
        });
        Some(())
    }

    pub fn authorize_pending_tool_call(
        &mut self,
        session_id: &acp::SessionId,
        kind: acp::PermissionOptionKind,
        cx: &mut Context<Self>,
    ) -> Option<()> {
        let (authorize_session_id, tool_call_id, options) =
            self.pending_tool_call(session_id, cx)?;
        let option = permission_option_for_action(options, kind)?;
        self.authorize_tool_call(
            authorize_session_id,
            tool_call_id,
            SelectedPermissionOutcome::new(option.option_id.clone(), option.kind),
            cx,
        );
        Some(())
    }

    pub fn authorize_with_granularity(
        &mut self,
        session_id: acp::SessionId,
        tool_call_id: acp::ToolCallId,
        selection: Option<&thread_view::PermissionSelection>,
        is_allow: bool,
        cx: &mut Context<Self>,
    ) -> Option<()> {
        let options =
            self.permission_options_for_tool_call(&session_id, tool_call_id.clone(), cx)?;
        let outcome = resolve_outcome_from_selection(options, selection, is_allow)?;
        self.authorize_tool_call(session_id, tool_call_id, outcome, cx);
        Some(())
    }

    pub fn authorize_tool_call(
        &mut self,
        session_id: acp::SessionId,
        tool_call_id: acp::ToolCallId,
        outcome: SelectedPermissionOutcome,
        cx: &mut Context<Self>,
    ) {
        let Some(thread) = self.threads.get(&session_id) else {
            return;
        };
        let agent_telemetry_id = thread.read(cx).connection().telemetry_id();
        let session_id = thread.read(cx).session_id().clone();

        telemetry::event!(
            "Agent Tool Call Authorized",
            agent = agent_telemetry_id,
            session = session_id,
            option = outcome.option_kind
        );

        thread.update(cx, |thread, cx| {
            thread.authorize_tool_call(tool_call_id, outcome, cx);
        });
        cx.notify();
    }

    fn set_work_dirs(&mut self, work_dirs: PathList, cx: &mut Context<Self>) {
        for thread in self.threads.values() {
            thread.update(cx, |thread, cx| {
                thread.set_work_dirs(work_dirs.clone(), cx);
            });
        }
    }
}

pub(crate) struct RootThreadUpdated;

impl EventEmitter<RootThreadUpdated> for ConversationView {}

fn permission_option_for_action(
    options: &PermissionOptions,
    kind: acp::PermissionOptionKind,
) -> Option<&acp::PermissionOption> {
    if kind == acp::PermissionOptionKind::AllowAlways
        && let PermissionOptions::Flat(options) = options
        && let Some(option) = options.iter().find(|option| {
            option.option_id.0.as_ref() == acp_thread::SandboxPermission::AllowAlways.as_id()
        })
    {
        return Some(option);
    }

    options.first_option_of_kind(kind)
}

pub struct StateChange;

impl EventEmitter<StateChange> for ConversationView {}

fn resolve_outcome_from_selection(
    options: &PermissionOptions,
    selection: Option<&thread_view::PermissionSelection>,
    is_allow: bool,
) -> Option<SelectedPermissionOutcome> {
    let choices = match options {
        PermissionOptions::Dropdown(choices) => choices.as_slice(),
        PermissionOptions::DropdownWithPatterns { choices, .. } => choices.as_slice(),
        PermissionOptions::Flat(_) => {
            let kind = if is_allow {
                acp::PermissionOptionKind::AllowOnce
            } else {
                acp::PermissionOptionKind::RejectOnce
            };
            let option = options.first_option_of_kind(kind)?;
            return Some(SelectedPermissionOutcome::new(
                option.option_id.clone(),
                option.kind,
            ));
        }
    };

    // When in per-command pattern mode, use the checked patterns.
    if let Some(thread_view::PermissionSelection::SelectedPatterns(checked)) = selection {
        if let Some(outcome) = options.build_outcome_for_checked_patterns(checked, is_allow) {
            return Some(outcome);
        }
    }

    // Use the selected granularity choice ("ターミナルは常に" or "今回のみ").
    let selected_index = selection
        .and_then(|s| s.choice_index())
        .unwrap_or_else(|| choices.len().saturating_sub(1));
    let selected_choice = choices.get(selected_index).or(choices.last())?;
    Some(selected_choice.build_outcome(is_allow))
}

fn affects_thread_metadata(event: &AcpThreadEvent) -> bool {
    match event {
        AcpThreadEvent::NewEntry
        | AcpThreadEvent::TitleUpdated
        | AcpThreadEvent::ToolAuthorizationRequested(_)
        | AcpThreadEvent::ToolAuthorizationReceived(_)
        | AcpThreadEvent::ElicitationRequested(_)
        | AcpThreadEvent::ElicitationResponded(_)
        | AcpThreadEvent::Stopped(_)
        | AcpThreadEvent::Error
        | AcpThreadEvent::LoadError(_)
        | AcpThreadEvent::Refusal
        | AcpThreadEvent::WorkingDirectoriesUpdated => true,
        // --
        AcpThreadEvent::EntryUpdated(_)
        | AcpThreadEvent::StatusChanged
        | AcpThreadEvent::EntriesRemoved(_)
        | AcpThreadEvent::Retry(_)
        | AcpThreadEvent::TokenUsageUpdated
        | AcpThreadEvent::PromptCapabilitiesUpdated
        | AcpThreadEvent::AvailableCommandsUpdated(_)
        | AcpThreadEvent::ModeUpdated(_)
        | AcpThreadEvent::ConfigOptionsUpdated(_)
        | AcpThreadEvent::SubagentSpawned(_)
        | AcpThreadEvent::PromptUpdated => false,
    }
}

pub enum AcpServerViewEvent {
    ActiveThreadChanged,
}

impl EventEmitter<AcpServerViewEvent> for ConversationView {}

/// 1 つの通知ポップアップに張った購読一式と、それを張り直すのに要る材料。
/// どの購読も登録先のウィンドウに紐づくので、ADR-F36 でタブが別ウィンドウへ
/// 移ったときは材料から作り直す。
struct NotificationBinding {
    root_thread_id: ThreadId,
    root_work_dirs: Option<PathList>,
    root_title: Option<SharedString>,
    _subscriptions: Vec<Subscription>,
}

pub struct ConversationView {
    agent: Rc<dyn AgentServer>,
    connection_store: Entity<AgentConnectionStore>,
    connection_key: Agent,
    /// ADR-F49 decision 19: this view's own connection, started outside the
    /// agent's sandbox after the user confirmed it here. Other views and new
    /// threads keep connecting in the sandbox.
    outside_sandbox: Option<OutsideSandboxConnectionId>,
    agent_server_store: Entity<AgentServerStore>,
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    thread_store: Option<Entity<ThreadStore>>,
    pub(crate) thread_id: ThreadId,
    pub(crate) root_session_id: Option<acp::SessionId>,
    server_state: ServerState,
    focus_handle: FocusHandle,
    notifications: Vec<WindowHandle<AgentNotification>>,
    notification_subscriptions: HashMap<WindowHandle<AgentNotification>, NotificationBinding>,
    /// ADR-F36: サーバーがまだ繋がっていない (= [`ThreadView`] がまだ無い)
    /// うちにタブが別ウィンドウへ移されたときの張り直し予約。接続できた時点で
    /// [`ConversationView::apply_pending_window_rebind`] が消化する。
    pending_window_rebind: Option<AnyWindowHandle>,
    auth_task: Option<Task<()>>,
    loading_status: Option<SharedString>,
    /// When settings change, use this to see if the theme has changed (which
    /// causes mermaid diagrams to re-render).
    last_theme_id: Option<String>,
    draft_prompt_persist_task: Option<Task<()>>,
    /// Cache + worktree snapshot for resolving paths in markdown code spans.
    /// Shared with the child [`ThreadView`] when one is constructed.
    pub(crate) code_span_resolver: AgentCodeSpanResolver,
    request_elicitation_form_states: HashMap<ElicitationEntryId, ElicitationFormState>,
    /// Guards [`Self::ask_to_allow_agent_hosts`]'s write-and-wait against a
    /// second click while the first is still in flight (F1's
    /// `confirm_install`/`consent.waiting` shape).
    agent_hosts_consent_pending: bool,
    /// Why the last consent did not carry through, kept next to the card that
    /// asked for it rather than replacing it (F1's `consent.error` shape), so
    /// the button that starts it is still there to try again with.
    agent_hosts_consent_error: Option<SharedString>,
    /// ADR-F53 決定 4: API キーの入力欄。毎フレーム作り直さないようここに
    /// 持つ。キーを `env` から読むエージェント
    /// ([`agent_servers::api_key_variable`]) のビューだけが持ち、ほかは
    /// `None`。
    api_key_editor: Option<Entity<Editor>>,
    /// API キーを保存できなかった理由。**キーそのものは決してここへ入れない**
    /// (ここは画面に出る)。
    api_key_error: Option<SharedString>,
    /// 設定済みのキーを入れ替えている最中。設定済みのときは入力欄ではなく
    /// 「設定済み」のカードを出すので、その切り替え。
    api_key_editing: bool,
    /// キーを書く / 消す仕事。どちらも同じ枠を使うので、片方が走っている
    /// あいだはもう片方を始めない (settings.json への書き込みが交差しない)。
    api_key_save_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl ConversationView {
    pub fn has_auth_methods(&self) -> bool {
        self.as_connected().map_or(false, |connected| {
            !connected.connection.auth_methods().is_empty()
        })
    }

    pub fn supports_logout(&self) -> bool {
        self.as_connected().is_some_and(|connected| {
            connected.auth_state.is_ok() && connected.connection.supports_logout()
        })
    }

    pub fn active_thread(&self) -> Option<&Entity<ThreadView>> {
        match &self.server_state {
            ServerState::Connected(connected) => connected.active_view(),
            _ => None,
        }
    }

    pub fn pending_tool_call<'a>(
        &'a self,
        cx: &'a App,
    ) -> Option<(acp::SessionId, acp::ToolCallId, &'a PermissionOptions)> {
        let session_id = self.active_thread()?.read(cx).session_id.clone();
        self.as_connected()?
            .conversation
            .read(cx)
            .pending_tool_call(&session_id, cx)
    }

    pub fn root_thread_has_pending_tool_call(&self, cx: &App) -> bool {
        let Some(root_thread) = self.root_thread_view() else {
            return false;
        };
        let root_session_id = root_thread.read(cx).thread.read(cx).session_id().clone();
        self.as_connected().is_some_and(|connected| {
            connected
                .conversation
                .read(cx)
                .pending_tool_call(&root_session_id, cx)
                .is_some()
        })
    }

    pub(crate) fn root_thread(&self, cx: &App) -> Option<Entity<AcpThread>> {
        self.root_thread_view()
            .map(|view| view.read(cx).thread.clone())
    }

    pub fn root_thread_view(&self) -> Option<Entity<ThreadView>> {
        self.root_session_id
            .as_ref()
            .and_then(|id| self.thread_view(id))
    }

    pub fn thread_view(&self, session_id: &acp::SessionId) -> Option<Entity<ThreadView>> {
        let connected = self.as_connected()?;
        connected.threads.get(session_id).cloned()
    }

    pub fn as_connected(&self) -> Option<&ConnectedServerState> {
        match &self.server_state {
            ServerState::Connected(connected) => Some(connected),
            _ => None,
        }
    }

    pub fn as_connected_mut(&mut self) -> Option<&mut ConnectedServerState> {
        match &mut self.server_state {
            ServerState::Connected(connected) => Some(connected),
            _ => None,
        }
    }

    pub fn updated_at(&self, cx: &App) -> Option<Instant> {
        self.as_connected()
            .and_then(|connected| connected.conversation.read(cx).updated_at)
    }

    pub fn navigate_to_thread(
        &mut self,
        session_id: acp::SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(connected) = self.as_connected_mut() else {
            return;
        };

        connected.navigate_to_thread(session_id);
        if let Some(view) = self.active_thread() {
            view.read(cx).activation_focus_handle(cx).focus(window, cx);
        }
        cx.emit(AcpServerViewEvent::ActiveThreadChanged);
        cx.notify();
    }

    pub fn set_work_dirs(&mut self, work_dirs: PathList, cx: &mut Context<Self>) {
        if let Some(connected) = self.as_connected() {
            connected.conversation.update(cx, |conversation, cx| {
                conversation.set_work_dirs(work_dirs.clone(), cx);
            });
        }
    }
}

/// ADR-F49 decision 16: holds the sandbox a sign-in was wrapped in for as
/// long as its terminal runs, and releases it (and the network proxy that
/// sandbox owns) when the sign-in is over — whether it succeeded, failed, or
/// was dropped with the view.
struct LoginSandboxGuard {
    connection: Rc<AcpConnection>,
    task_id: String,
}

impl LoginSandboxGuard {
    fn new(connection: Rc<AcpConnection>, task_id: String) -> Self {
        Self {
            connection,
            task_id,
        }
    }
}

impl Drop for LoginSandboxGuard {
    fn drop(&mut self) {
        self.connection.release_terminal_auth_sandbox(&self.task_id);
    }
}

enum ServerState {
    Loading {
        _loading: Entity<LoadingView>,
        connection: Option<Rc<dyn AgentConnection>>,
        _request_elicitation_subscription: Option<Subscription>,
        /// ADR-F49 (new decision): the folders this start was asked for. A
        /// start that never reaches a thread is the only one that could not
        /// say afterwards which folders it was for, and the thread built next
        /// has to be built on the same ones — see
        /// [`ConversationView::work_dirs_in_hand`].
        work_dirs: Option<PathList>,
    },
    LoadError {
        error: LoadError,
        work_dirs: Option<PathList>,
    },
    Connected(ConnectedServerState),
    Parked(ParkedServerState),
}

/// ADR-F49 (new decision): a draft whose agent the idle disconnect stopped,
/// still on screen and still the user's to type in.
///
/// The view keeps its tab, its row and the editor it was written in — text,
/// mentions and all — and holds nothing else: no [`AcpThread`], no session and
/// no `Rc` on the connection, so the store dropping its entry really reaches
/// the agent's process. The same editor is handed to the thread that comes
/// back when the agent is asked for again ([`ConversationView::unpark`]),
/// which is why nothing has to be copied out of it here.
struct ParkedServerState {
    message_editor: Entity<MessageEditor>,
    /// The folders the session had been given. They are the user's choice as
    /// much as the text is, and the session that is opened again has to be
    /// opened on the same ones.
    work_dirs: Option<PathList>,
    _editor_subscription: Subscription,
}

/// What a parked view is being started again for: the user pressed send, or
/// they asked for the agent back without sending anything
/// ([`ConversationView::render_parked_notice`]).
#[derive(Clone, Copy, PartialEq)]
enum Unpark {
    Only,
    AndSend,
}

/// The draft editor a parked view hands to the thread it reconnects into, so
/// that coming back does not rebuild the box the user was writing in.
pub(crate) struct AdoptedDraft {
    editor: Entity<MessageEditor>,
    /// Whether a send the user already asked for is still owed once the thread
    /// is there.
    auto_submit: bool,
}

// current -> Entity
// hashmap of threads, current becomes session_id
pub struct ConnectedServerState {
    auth_state: AuthState,
    active_id: Option<acp::SessionId>,
    pub(crate) threads: HashMap<acp::SessionId, Entity<ThreadView>>,
    connection: Rc<dyn AgentConnection>,
    conversation: Entity<Conversation>,
    _connection_entry_subscription: Subscription,
    _request_elicitation_subscription: Option<Subscription>,
}

enum AuthState {
    Ok,
    Unauthenticated {
        description: Option<Entity<Markdown>>,
        pending_auth_method: Option<acp::AuthMethodId>,
    },
}

impl AuthState {
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok)
    }
}

struct LoadingView {
    _load_task: Task<()>,
}

impl ConnectedServerState {
    pub fn active_view(&self) -> Option<&Entity<ThreadView>> {
        self.active_id.as_ref().and_then(|id| self.threads.get(id))
    }

    pub fn has_thread_error(&self, cx: &App) -> bool {
        self.active_view()
            .map_or(false, |view| view.read(cx).thread_error.is_some())
    }

    pub fn navigate_to_thread(&mut self, session_id: acp::SessionId) {
        if self.threads.contains_key(&session_id) {
            self.active_id = Some(session_id);
        }
    }

    pub fn close_all_sessions(&self, cx: &mut App) -> Task<()> {
        let tasks = self.threads.values().filter_map(|view| {
            if self.connection.supports_close_session() {
                let session_id = view.read(cx).thread.read(cx).session_id().clone();
                Some(self.connection.clone().close_session(&session_id, cx))
            } else {
                None
            }
        });
        let task = futures::future::join_all(tasks);
        cx.background_spawn(async move {
            task.await;
        })
    }
}

impl ConversationView {
    pub fn new(
        agent: Rc<dyn AgentServer>,
        connection_store: Entity<AgentConnectionStore>,
        connection_key: Agent,
        resume_session_id: Option<acp::SessionId>,
        thread_id: Option<ThreadId>,
        work_dirs: Option<PathList>,
        title: Option<SharedString>,
        initial_content: Option<AgentInitialContent>,
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        thread_store: Option<Entity<ThreadStore>>,
        source: AgentThreadSource,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let agent_server_store = project.read(cx).agent_server_store().clone();
        let code_span_resolver = AgentCodeSpanResolver::new(&project.downgrade(), cx);
        let mut subscriptions = vec![
            cx.observe_global_in::<SettingsStore>(window, Self::agent_ui_font_size_changed),
            cx.observe_global_in::<SettingsStore>(window, Self::invalidate_mermaid_caches),
            cx.observe_global_in::<AgentUiFontSize>(window, Self::agent_ui_font_size_changed),
            cx.observe_global_in::<AgentBufferFontSize>(window, Self::agent_ui_font_size_changed),
            cx.subscribe_in(
                &agent_server_store,
                window,
                Self::handle_agent_servers_updated,
            ),
        ];
        subscriptions.push(cx.subscribe(&project, {
            let resolver = code_span_resolver.clone();
            move |_this: &mut Self, _project, event: &project::Event, cx| {
                if matches!(
                    event,
                    project::Event::WorktreeAdded(_)
                        | project::Event::WorktreeRemoved(_)
                        | project::Event::WorktreeUpdatedEntries(_, _)
                ) {
                    resolver.clear_cache();
                    cx.notify();
                }
            }
        }));

        cx.on_release(|this, cx| {
            this.request_elicitation_form_states.clear();
            if let Some(connected) = this.as_connected() {
                connected.close_all_sessions(cx).detach();
            }
            for window in this.notifications.drain(..) {
                window
                    .update(cx, |_, window, _| {
                        window.remove_window();
                    })
                    .ok();
            }
        })
        .detach();

        let thread_id = thread_id.unwrap_or_else(ThreadId::new);

        // ADR-F53 決定 4: 入力欄を持つのは、キーを `env` から読むエージェント
        // のビューだけ。ほかのビューに空の Editor を配らない。サインインを
        // 出すかどうか (ADR-F55) とは無関係: 両方出ていても、キーを入れる
        // 口は残る。
        let api_key_editor = agent_servers::api_key_variable(agent.agent_id().0.as_ref())
            .is_some()
            .then(|| {
                cx.new(|cx| {
                    let mut editor = Editor::single_line(window, cx);
                    editor.set_placeholder_text("sk-ant-…", window, cx);
                    editor.set_masked(true, cx);
                    editor
                })
            });

        Self {
            agent: agent.clone(),
            connection_store: connection_store.clone(),
            connection_key: connection_key.clone(),
            outside_sandbox: None,
            agent_server_store,
            workspace,
            project: project.clone(),
            thread_store,
            thread_id,
            root_session_id: resume_session_id.clone(),
            server_state: Self::initial_state(
                agent.clone(),
                connection_store,
                connection_key,
                None,
                resume_session_id,
                work_dirs,
                title,
                project,
                initial_content,
                None,
                source,
                window,
                cx,
            ),
            notifications: Vec::new(),
            notification_subscriptions: HashMap::default(),
            pending_window_rebind: None,
            auth_task: None,
            loading_status: None,
            last_theme_id: Some(cx.theme().id.clone()),
            draft_prompt_persist_task: None,
            code_span_resolver,
            request_elicitation_form_states: HashMap::default(),
            agent_hosts_consent_pending: false,
            agent_hosts_consent_error: None,
            api_key_editor,
            api_key_error: None,
            api_key_editing: false,
            api_key_save_task: None,
            _subscriptions: subscriptions,
            focus_handle: cx.focus_handle(),
        }
    }

    fn set_server_state(&mut self, state: ServerState, cx: &mut Context<Self>) {
        let previous_request_elicitation_connection = self.request_elicitation_connection();
        let next_request_elicitation_connection =
            Self::request_elicitation_connection_for_state(&state);

        if let Some(connected) = self.as_connected() {
            connected.close_all_sessions(cx).detach();
        }

        if let Some(connection) = previous_request_elicitation_connection
            && !next_request_elicitation_connection
                .as_ref()
                .is_some_and(|next_connection| Rc::ptr_eq(&connection, next_connection))
        {
            self.request_elicitation_form_states.clear();
        }

        self.server_state = state;
        cx.emit(StateChange);
        cx.emit(AcpServerViewEvent::ActiveThreadChanged);
        if matches!(&self.server_state, ServerState::Connected(_)) {
            cx.emit(RootThreadUpdated);
            self.apply_pending_window_rebind(cx);
        }
        cx.notify();
    }

    fn request_elicitation_subscription(
        connection: &Rc<dyn AgentConnection>,
        cx: &mut Context<Self>,
    ) -> Option<Subscription> {
        let store = connection.request_elicitations()?;
        Some(cx.observe(&store, |this, _store, cx| {
            if let Some(active_thread) = this.active_thread().cloned() {
                active_thread.update(cx, |_thread, cx| cx.notify());
            }
            cx.notify();
        }))
    }

    fn request_elicitation_connection(&self) -> Option<Rc<dyn AgentConnection>> {
        Self::request_elicitation_connection_for_state(&self.server_state)
    }

    fn active_thread_renders_request_elicitations(&self) -> bool {
        match &self.server_state {
            ServerState::Connected(connected) => {
                connected.auth_state.is_ok() && connected.active_view().is_some()
            }
            _ => false,
        }
    }

    fn request_elicitation_connection_for_state(
        state: &ServerState,
    ) -> Option<Rc<dyn AgentConnection>> {
        match state {
            ServerState::Loading {
                connection: Some(connection),
                ..
            } => Some(connection.clone()),
            ServerState::Connected(connected) => Some(connected.connection.clone()),
            ServerState::Loading {
                connection: None, ..
            }
            | ServerState::LoadError { .. }
            | ServerState::Parked(_) => None,
        }
    }

    fn request_elicitation_store(&self) -> Option<Entity<ElicitationStore>> {
        self.request_elicitation_connection()?
            .request_elicitations()
    }

    fn reset(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (resume_session_id, work_dirs, title) = self
            .root_thread_view()
            .map(|thread_view| {
                let tv = thread_view.read(cx);
                let thread = tv.thread.read(cx);
                (
                    Some(thread.session_id().clone()),
                    thread.work_dirs().cloned(),
                    thread.title(),
                )
            })
            .unwrap_or_else(|| {
                let session_id = self.root_session_id.clone();
                let (work_dirs, title) = session_id
                    .as_ref()
                    .and_then(|id| {
                        let store = ThreadMetadataStore::try_global(cx)?;
                        let entry = store.read(cx).entry_by_session(id)?;
                        Some((Some(entry.folder_paths().clone()), entry.title()))
                    })
                    .unwrap_or((None, None));
                (session_id, work_dirs, title)
            });
        // ADR-F49 (new decision): a view with no thread to ask — it let its
        // agent go, or the way back failed — still knows which folders the
        // session was on, and the next one opens on those rather than on the
        // project's default.
        let work_dirs = work_dirs.or_else(|| self.work_dirs_in_hand(cx));

        // ADR-F49 (new decision): with no session to resume this builds a
        // draft from nothing, and its box would come up empty — which after a
        // start that failed is the text the user had written gone, the parked
        // draft's included ([`Self::unpark`] writes it out before asking for
        // the agent). The panel fills a draft it rebuilds from the same store
        // ([`AgentPanel::restore_new_draft`]), so this does too.
        let initial_content = if resume_session_id.is_none() {
            crate::draft_prompt_store::read(self.thread_id, cx).map(|blocks| {
                AgentInitialContent::ContentBlock {
                    blocks,
                    auto_submit: false,
                }
            })
        } else {
            None
        };

        self.clear_resolved_request_elicitations(cx);
        self.loading_status = None;

        // A connection that has gone (it failed to start, or the agent was
        // removed) takes the view back into the sandbox, and another start
        // outside it has to be confirmed again.
        if self.outside_sandbox.is_some_and(|id| {
            self.connection_store
                .read(cx)
                .outside_sandbox_connection(id)
                .is_none()
        }) {
            self.outside_sandbox = None;
        }

        let state = Self::initial_state(
            self.agent.clone(),
            self.connection_store.clone(),
            self.connection_key.clone(),
            self.outside_sandbox,
            resume_session_id,
            work_dirs,
            title,
            self.project.clone(),
            initial_content,
            None,
            AgentThreadSource::AgentPanel,
            window,
            cx,
        );
        self.set_server_state(state, cx);

        if let Some(view) = self.root_thread_view() {
            view.update(cx, |this, cx| {
                this.message_editor.update(cx, |editor, cx| {
                    editor.set_session_capabilities(this.session_capabilities.clone(), cx);
                });
            });
        }
        cx.notify();
    }

    fn initial_state(
        agent: Rc<dyn AgentServer>,
        connection_store: Entity<AgentConnectionStore>,
        connection_key: Agent,
        outside_sandbox: Option<OutsideSandboxConnectionId>,
        resume_session_id: Option<acp::SessionId>,
        work_dirs: Option<PathList>,
        title: Option<SharedString>,
        project: Entity<Project>,
        initial_content: Option<AgentInitialContent>,
        adopted_draft: Option<AdoptedDraft>,
        source: AgentThreadSource,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> ServerState {
        if project.read(cx).is_via_collab()
            && agent.clone().downcast::<NativeAgentServer>().is_none()
        {
            return ServerState::LoadError {
                error: LoadError::Other(
                    "共有プロジェクトでは外部エージェントはまだサポートされていません。".into(),
                ),
                work_dirs,
            };
        }
        let session_work_dirs = work_dirs.unwrap_or_else(|| project.read(cx).default_path_list(cx));
        let started_work_dirs = session_work_dirs.clone();

        let view = cx.weak_entity();
        let connection_entry = connection_store.update(cx, |store, cx| {
            // ADR-F49 (new decision): the store keeps this connection for as
            // long as this view uses it, and drops it once nothing has for a
            // while.
            store.hold_connection(view, &connection_key, outside_sandbox, cx);
            outside_sandbox
                .and_then(|id| store.outside_sandbox_connection(id))
                .unwrap_or_else(|| store.request_connection(connection_key, agent.clone(), cx))
        });

        let connection_entry_subscription =
            cx.subscribe(&connection_entry, |this, _entry, event, cx| match event {
                AgentConnectionEntryEvent::NewVersionAvailable(version) => {
                    if let Some(thread) = this.root_thread_view() {
                        thread.update(cx, |thread, cx| {
                            thread.new_server_version_available = Some(version.clone());
                            cx.notify();
                        });
                    }
                }
                AgentConnectionEntryEvent::LoadingStatusChanged(status) => {
                    this.loading_status = status.clone();
                    cx.notify();
                }
            });

        let connect_result = connection_entry.read(cx).wait_for_connection();

        let side = crate::agent_sidebar_side(cx);
        let thread_location = "current_worktree";

        let load_task = cx.spawn_in(window, async move |this, cx| {
            let connection = match connect_result.await {
                Ok(AgentConnectedState { connection, .. }) => connection,
                Err(err) => {
                    this.update_in(cx, |this, window, cx| {
                        this.handle_load_error(err, window, cx);
                        cx.notify();
                    })
                    .log_err();
                    return;
                }
            };

            this.update_in(cx, |this, _window, cx| {
                let request_elicitation_subscription =
                    Self::request_elicitation_subscription(&connection, cx);
                if let ServerState::Loading {
                    connection: loading_connection,
                    _request_elicitation_subscription,
                    ..
                } = &mut this.server_state
                {
                    *loading_connection = Some(connection.clone());
                    *_request_elicitation_subscription = request_elicitation_subscription;
                    cx.notify();
                }
            })
            .log_err();

            telemetry::event!(
                "Agent Thread Started",
                agent = connection.telemetry_id(),
                source = source.as_str(),
                side = side,
                thread_location = thread_location
            );

            let mut resumed_without_history = false;
            let result = if let Some(session_id) = resume_session_id.clone() {
                cx.update(|_, cx| {
                    if connection.supports_load_session() {
                        connection.clone().load_session(
                            session_id,
                            project.clone(),
                            session_work_dirs,
                            title,
                            cx,
                        )
                    } else if connection.supports_resume_session() {
                        resumed_without_history = true;
                        connection.clone().resume_session(
                            session_id,
                            project.clone(),
                            session_work_dirs,
                            title,
                            cx,
                        )
                    } else {
                        Task::ready(Err(anyhow!(LoadError::Other(
                            "このエージェントはセッションの読み込みや再開をサポートしていません。"
                                .into()
                        ))))
                    }
                })
                .log_err()
            } else {
                cx.update(|_, cx| {
                    connection
                        .clone()
                        .new_session(project.clone(), session_work_dirs, cx)
                })
                .log_err()
            };

            let Some(result) = result else {
                return;
            };

            let result = match result.await {
                Err(e) => match e.downcast::<acp_thread::AuthRequired>() {
                    Ok(err) => {
                        cx.update(|window, cx| {
                            Self::handle_auth_required(this, err, connection, window, cx)
                        })
                        .log_err();
                        return;
                    }
                    Err(err) => Err(err),
                },
                Ok(thread) => Ok(thread),
            };

            this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(thread) => {
                        this.clear_resolved_request_elicitations_for_connection(&connection, cx);
                        let root_session_id = thread.read(cx).session_id().clone();

                        let conversation = cx.new(|cx| {
                            let mut conversation = Conversation::default();
                            conversation.register_thread(thread.clone(), cx);
                            conversation
                        });

                        let current = this.new_thread_view(
                            thread,
                            conversation.clone(),
                            resumed_without_history,
                            initial_content,
                            adopted_draft,
                            window,
                            cx,
                        );

                        if this.focus_handle.contains_focused(window, cx) {
                            current
                                .read(cx)
                                .message_editor
                                .focus_handle(cx)
                                .focus(window, cx);
                        }

                        this.root_session_id = Some(root_session_id.clone());
                        let request_elicitation_subscription =
                            Self::request_elicitation_subscription(&connection, cx);
                        this.set_server_state(
                            ServerState::Connected(ConnectedServerState {
                                connection,
                                auth_state: AuthState::Ok,
                                active_id: Some(root_session_id.clone()),
                                threads: HashMap::from_iter([(root_session_id, current)]),
                                conversation,
                                _connection_entry_subscription: connection_entry_subscription,
                                _request_elicitation_subscription: request_elicitation_subscription,
                            }),
                            cx,
                        );
                    }
                    Err(err) => {
                        this.handle_load_error(
                            LoadError::Other(err.to_string().into()),
                            window,
                            cx,
                        );
                    }
                };
            })
            .log_err();
        });

        let loading_view = cx.new(|_cx| LoadingView {
            _load_task: load_task,
        });

        ServerState::Loading {
            _loading: loading_view,
            connection: None,
            _request_elicitation_subscription: None,
            work_dirs: Some(started_work_dirs),
        }
    }

    fn new_thread_view(
        &self,
        thread: Entity<AcpThread>,
        conversation: Entity<Conversation>,
        resumed_without_history: bool,
        initial_content: Option<AgentInitialContent>,
        adopted_draft: Option<AdoptedDraft>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<ThreadView> {
        let agent_id = self.agent.agent_id();
        let connection = thread.read(cx).connection().clone();
        let session_id = thread.read(cx).session_id().clone();
        let available_skills = connection
            .clone()
            .downcast::<agent::NativeAgentConnection>()
            .map(|native_connection| native_available_skills(&native_connection, &session_id, cx))
            .unwrap_or_default();
        let session_capabilities = Arc::new(RwLock::new(SessionCapabilities::new(
            thread.read(cx).prompt_capabilities(),
            thread.read(cx).available_commands().to_vec(),
            available_skills,
        )));

        let action_log = thread.read(cx).action_log().clone();

        let entry_view_state = cx.new(|_| {
            EntryViewState::new(
                self.workspace.clone(),
                self.project.downgrade(),
                self.thread_store.clone(),
                session_capabilities.clone(),
                self.agent.agent_id(),
            )
        });

        let count = thread.read(cx).entries().len();
        let list_state = ListState::new(0, gpui::ListAlignment::Top, px(2048.0));
        list_state.set_follow_mode(gpui::FollowMode::Tail);

        entry_view_state.update(cx, |view_state, cx| {
            for ix in 0..count {
                view_state.sync_entry(ix, &thread, window, cx);
            }
            list_state.splice_focusable(
                0..0,
                (0..count).map(|ix| view_state.entry(ix)?.focus_handle(cx)),
            );
        });

        if let Some(scroll_position) = thread.read(cx).ui_scroll_position() {
            list_state.scroll_to(scroll_position);
        } else {
            list_state.scroll_to_end();
        }

        AgentDiff::set_active_thread(&self.workspace, thread.clone(), window, cx);

        let connection = thread.read(cx).connection().clone();
        let session_id = thread.read(cx).session_id().clone();

        // Check for config options first
        // Config options take precedence over legacy mode/model selectors
        let config_options_provider = connection.session_config_options(&session_id, cx);

        let config_options_view;
        let mode_selector;
        let model_selector;
        if let Some(config_options) = config_options_provider {
            // Use config options - don't create mode_selector or model_selector
            let agent_server = self.agent.clone();
            let fs = self.project.read(cx).fs().clone();
            config_options_view =
                Some(cx.new(|cx| {
                    ConfigOptionsView::new(config_options, agent_server, fs, window, cx)
                }));
            model_selector = None;
            mode_selector = None;
        } else {
            // Fall back to dedicated mode/model selectors
            config_options_view = None;
            model_selector = connection.model_selector(&session_id).map(|selector| {
                cx.new(|cx| {
                    ModelSelectorPopover::new(
                        selector,
                        PopoverMenuHandle::default(),
                        self.focus_handle(cx),
                        window,
                        cx,
                    )
                })
            });

            mode_selector = connection
                .session_modes(&session_id, cx)
                .map(|session_modes| {
                    let fs = self.project.read(cx).fs().clone();
                    cx.new(|_cx| ModeSelector::new(session_modes, self.agent.clone(), fs))
                });
        }

        let subscriptions = vec![
            cx.subscribe_in(&thread, window, Self::handle_thread_event),
            cx.observe(&action_log, |_, _, cx| cx.notify()),
        ];

        let subagent_sessions = thread
            .read(cx)
            .entries()
            .iter()
            .filter_map(|entry| match entry {
                AgentThreadEntry::ToolCall(call) => call
                    .subagent_session_info
                    .as_ref()
                    .map(|i| i.session_id.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();

        if !subagent_sessions.is_empty() {
            let parent_session_id = thread.read(cx).session_id().clone();
            cx.spawn_in(window, async move |this, cx| {
                this.update_in(cx, |this, window, cx| {
                    for subagent_id in subagent_sessions {
                        this.load_subagent_session(
                            subagent_id,
                            parent_session_id.clone(),
                            window,
                            cx,
                        );
                    }
                })
            })
            .detach();
        }

        let profile_selector: Option<Rc<agent::NativeAgentConnection>> =
            connection.clone().downcast();
        let profile_selector = profile_selector
            .and_then(|native_connection| native_connection.thread(&session_id, cx))
            .map(|native_thread| {
                cx.new(|cx| {
                    ProfileSelector::new(
                        <dyn Fs>::global(cx),
                        Arc::new(native_thread),
                        self.focus_handle(cx),
                        cx,
                    )
                })
            });

        let agent_display_name = self
            .agent_server_store
            .read(cx)
            .agent_display_name(&agent_id.clone())
            .unwrap_or_else(|| agent_id.0.clone());

        let agent_icon = self.agent.logo();
        let agent_icon_from_external_svg = self
            .agent_server_store
            .read(cx)
            .agent_icon(&self.agent.agent_id())
            .or_else(|| {
                project::AgentRegistryStore::try_global(cx).and_then(|store| {
                    store
                        .read(cx)
                        .agent(&self.agent.agent_id())
                        .and_then(|a| a.icon_path().cloned())
                })
            });

        let weak = cx.weak_entity();
        cx.new(|cx| {
            ThreadView::new(
                self.thread_id,
                thread,
                conversation,
                weak,
                agent_icon,
                agent_icon_from_external_svg,
                agent_id,
                agent_display_name,
                self.workspace.clone(),
                entry_view_state,
                config_options_view,
                mode_selector,
                model_selector,
                profile_selector,
                list_state,
                session_capabilities,
                resumed_without_history,
                self.project.downgrade(),
                self.code_span_resolver.clone(),
                self.thread_store.clone(),
                initial_content,
                adopted_draft,
                subscriptions,
                window,
                cx,
            )
        })
    }

    fn handle_auth_required(
        this: WeakEntity<Self>,
        err: AuthRequired,
        connection: Rc<dyn AgentConnection>,
        window: &mut Window,
        cx: &mut App,
    ) {
        this.update(cx, |this, cx| {
            let description = err
                .description
                .map(|desc| cx.new(|cx| Markdown::new(desc.into(), None, None, cx)));
            let auth_state = AuthState::Unauthenticated {
                pending_auth_method: None,
                description,
            };
            if let Some(connected) = this.as_connected_mut() {
                connected.auth_state = auth_state;
                cx.emit(StateChange);
                if let Some(view) = connected.active_view()
                    && view
                        .read(cx)
                        .message_editor
                        .focus_handle(cx)
                        .is_focused(window)
                {
                    this.focus_handle.focus(window, cx)
                }
            } else {
                let request_elicitation_subscription =
                    Self::request_elicitation_subscription(&connection, cx);
                this.set_server_state(
                    ServerState::Connected(ConnectedServerState {
                        auth_state,
                        active_id: None,
                        threads: HashMap::default(),
                        connection,
                        conversation: cx.new(|_cx| Conversation::default()),
                        _connection_entry_subscription: Subscription::new(|| {}),
                        _request_elicitation_subscription: request_elicitation_subscription,
                    }),
                    cx,
                );
            }
            cx.notify();
        })
        .ok();
    }

    fn handle_load_error(&mut self, err: LoadError, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.root_thread_view() {
            if view
                .read(cx)
                .message_editor
                .focus_handle(cx)
                .is_focused(window)
            {
                self.focus_handle.focus(window, cx)
            }
        }
        self.emit_load_error_telemetry(&err);
        // A fresh attempt failed, so whatever the last consent ran into is
        // stale; `ask_to_allow_agent_hosts` never comes through here.
        self.agent_hosts_consent_error = None;
        let work_dirs = self.work_dirs_in_hand(cx);
        self.set_server_state(
            ServerState::LoadError {
                error: err,
                work_dirs,
            },
            cx,
        );
    }

    /// ADR-F49 (new decision): the folders the session this view holds is on —
    /// or, where it holds none, the folders the one it is trying to hold was
    /// asked for.
    ///
    /// Until the idle disconnect there was always a thread to ask by the time
    /// anybody did. A view that let its agent go, or whose way back failed,
    /// has none, and the thread built for it next
    /// ([`Self::reset`], [`Self::unpark`]) has to open on the folders the user
    /// chose rather than on the project's default.
    fn work_dirs_in_hand(&self, cx: &App) -> Option<PathList> {
        match &self.server_state {
            ServerState::Connected(_) => self
                .root_thread_view()
                .and_then(|thread_view| thread_view.read(cx).thread.read(cx).work_dirs().cloned()),
            ServerState::Loading { work_dirs, .. } | ServerState::LoadError { work_dirs, .. } => {
                work_dirs.clone()
            }
            ServerState::Parked(parked) => parked.work_dirs.clone(),
        }
    }

    fn handle_agent_servers_updated(
        &mut self,
        _agent_server_store: &Entity<project::AgentServerStore>,
        _event: &project::AgentServersUpdated,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // If we're in a LoadError state OR have a thread_error set (which can happen
        // when agent.connect() fails during loading), retry loading the thread.
        // This handles the case where a thread is restored before authentication completes.
        let should_retry = match &self.server_state {
            ServerState::Loading { .. } => false,
            // A parked draft is not waiting for the agent to come back; the
            // next send starts it.
            ServerState::Parked(_) => false,
            ServerState::LoadError { .. } => true,
            ServerState::Connected(connected) => {
                connected.auth_state.is_ok() && connected.has_thread_error(cx)
            }
        };

        if should_retry {
            if let Some(active) = self.root_thread_view() {
                active.update(cx, |active, cx| {
                    active.clear_thread_error(cx);
                });
            }
            self.reset(window, cx);
        }
    }

    /// ADR-F49 (new decision): whether this view is using its agent's
    /// connection.
    ///
    /// Being alive is not enough. A thread whose tab was closed is parked in
    /// the panel with its connection still in hand — which is exactly what the
    /// idle disconnect is for — so what counts is that the thread is open
    /// somewhere (any window, any pane, the panel's own tabs included), or
    /// that this view is still connecting or signing in. An empty draft counts
    /// nowhere, on screen included: see [`Self::is_releasable_empty_draft`].
    pub(crate) fn is_using_connection(&self, cx: &App) -> bool {
        if self.auth_task.is_some() {
            return true;
        }
        match &self.server_state {
            ServerState::Loading { .. } => return true,
            // Nothing left to use: the connection was already let go of.
            ServerState::Parked(_) => return false,
            ServerState::Connected(connected) if !connected.auth_state.is_ok() => return true,
            ServerState::Connected(_) | ServerState::LoadError { .. } => {}
        }
        if self.is_releasable_empty_draft(cx) {
            return false;
        }
        ThreadItem::is_open_in_a_tab(self.thread_id, cx)
    }

    /// ADR-F49 (new decision): whether this view holds an empty draft — one
    /// its agent can be taken away from without the user losing anything.
    ///
    /// Nothing was ever sent in it, so there is no session to read back and
    /// `supports_load_session` has no say: the next send starts the agent
    /// again and opens a session of its own. What makes that safe is only that
    /// the draft is empty and still, so each of those is asked for here:
    ///
    /// * it is still a draft — sending turns it into a thread with a history,
    ///   which is read back from the agent rather than started over;
    /// * the agent is not answering — a draft is generating for the length of
    ///   the turn the first message starts;
    /// * nothing is on its way out of the editor (`is_loading_contents`): an
    ///   empty editor is then a message in flight rather than a draft nobody
    ///   wrote in;
    /// * the editor is empty, and so is the prompt the thread holds for it.
    ///
    /// Being on screen is deliberately not asked about. A draft the user is
    /// looking at is one they may type in at any moment, and typing in it is
    /// what a parked view still allows — what goes is the connection, not the
    /// box.
    fn is_releasable_empty_draft(&self, cx: &App) -> bool {
        let Some(thread_view) = self.root_thread_view() else {
            return false;
        };
        let thread_view = thread_view.read(cx);
        if thread_view.is_loading_contents {
            return false;
        }
        let thread = thread_view.thread.read(cx);
        if !thread.is_draft_thread() || thread.status() != ThreadStatus::Idle {
            return false;
        }
        if thread
            .draft_prompt()
            .is_some_and(|blocks| !blocks.is_empty())
        {
            return false;
        }
        thread_view
            .message_editor
            .read(cx)
            .text(cx)
            .trim()
            .is_empty()
    }

    pub(crate) fn is_parked(&self) -> bool {
        matches!(self.server_state, ServerState::Parked(_))
    }

    /// ADR-F49 (new decision): lets go of a connection nothing has used, so
    /// that the store dropping it really ends the agent. Answers whether it
    /// could.
    ///
    /// There are two ways to let go. A thread the panel is only parking leaves
    /// the parking slot and is read back from the agent when it is opened
    /// again, the way the panel's own pruning
    /// ([`AgentPanel::cleanup_retained_threads`]) already relies on. An empty
    /// draft instead stays exactly where it is — its tab, its row and the
    /// editor it was written in — and drops the connection out from under
    /// itself ([`Self::park`]). A thread that cannot be read back, or one
    /// whose agent is still answering, is kept instead: losing a conversation
    /// is never worth an idle process.
    ///
    /// Takes the view rather than `&mut self` because the panel reads the view
    /// to decide, which it could not do while the view was leased to an update
    /// of its own.
    pub(crate) fn release_idle_connection(view: &Entity<Self>, cx: &mut App) -> bool {
        let view_id = view.entity_id();
        let (thread_id, panel, already_parked) = {
            let this = view.read(cx);
            (this.thread_id, this.parking_panel(cx), this.is_parked())
        };
        if already_parked {
            return true;
        }
        let Some(panel) = panel else {
            return false;
        };
        if !view.read(cx).can_release_idle_connection(view_id, cx) {
            return false;
        }
        if panel.update(cx, |panel, cx| {
            panel.release_parked_thread(thread_id, view_id, cx)
        }) {
            return true;
        }
        // Parking subscribes to the draft editor for the send that starts the
        // agent again, and a subscription belongs to a window — which this
        // sweep, running from a timer, does not have. A window that closed
        // while it ran took its panel with it, which `unwrap_or_default` reads
        // as "nothing was let go of here".
        let window = panel.read(cx).window_handle();
        let parked = window
            .update(cx, |_root, window, cx| {
                view.update(cx, |this, cx| this.park(window, cx))
            })
            .unwrap_or_default();
        if parked {
            // The panel follows its draft through that draft's `AcpThread`,
            // and the thread is what just went.
            panel.update(cx, |panel, cx| panel.reobserve_draft_editor(view, cx));
        }
        parked
    }

    /// ADR-F49 (new decision): drops this view's connection while keeping the
    /// draft that was written in it. Answers whether there was one to drop.
    ///
    /// Replacing the state is what lets the connection go: the [`AcpThread`],
    /// the [`ThreadView`] and the `Rc` on the connection all belong to
    /// [`ConnectedServerState`], and [`Self::set_server_state`] closes the
    /// session before dropping them. The editor is lifted out first and is the
    /// only thing that survives, which is what keeps the user's text, their
    /// mentions and their caret.
    fn park(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some((message_editor, work_dirs)) = self
            .as_connected()
            .and_then(|connected| connected.active_view())
            .map(|thread_view| {
                let thread_view = thread_view.read(cx);
                (
                    thread_view.message_editor.clone(),
                    thread_view.thread.read(cx).work_dirs().cloned(),
                )
            })
        else {
            return false;
        };
        let editor_subscription = cx.subscribe_in(
            &message_editor,
            window,
            |this, _editor, event, window, cx| match event {
                MessageEditorEvent::Send | MessageEditorEvent::SendImmediately => {
                    this.unpark(Unpark::AndSend, window, cx);
                }
                // Typing is not a reason to start the agent again — a user may
                // draft for minutes — but what is typed is persisted the way a
                // connected draft's is, so a window that closes before the
                // send does not take the text with it.
                MessageEditorEvent::Edited => this.schedule_draft_prompt_persist(cx),
                _ => {}
            },
        );
        // The session this view had went with the connection, so it must not
        // be written out or resumed from.
        self.root_session_id = None;
        self.set_server_state(
            ServerState::Parked(ParkedServerState {
                message_editor,
                work_dirs,
                _editor_subscription: editor_subscription,
            }),
            cx,
        );
        log::info!(
            "parked the empty draft of {}: nothing has used its connection",
            self.agent.agent_id()
        );
        true
    }

    /// ADR-F49 (new decision): starts the agent again for a parked draft, along
    /// the path a brand new draft already takes. The editor goes with it, so
    /// whatever the user put there — before the agent was let go of and while
    /// it was gone — is what the thread comes back into, and a send carries
    /// it.
    fn unpark(&mut self, then: Unpark, window: &mut Window, cx: &mut Context<Self>) {
        let ServerState::Parked(parked) = &self.server_state else {
            return;
        };
        let adopted = AdoptedDraft {
            editor: parked.message_editor.clone(),
            auto_submit: matches!(then, Unpark::AndSend),
        };
        let work_dirs = parked.work_dirs.clone();
        // A start that does not come back replaces this view with a card of
        // its own (the sign-in, the allowlist, the plain failure), and the box
        // the user was writing in goes with it. What is in it is written out
        // here rather than left to the persist debounce, which a send one
        // keystroke after the last one outruns: the text is then in the store
        // the next draft under this id is built from, wherever it is built
        // ([`Self::reset`], [`AgentPanel::restore_new_draft`]).
        let draft_blocks = parked
            .message_editor
            .read(cx)
            .draft_content_blocks_snapshot(cx);
        if !draft_blocks.is_empty() {
            crate::draft_prompt_store::write(self.thread_id, &draft_blocks, cx)
                .detach_and_log_err(cx);
        }
        // The same rule [`Self::reset`] follows: a start outside the sandbox
        // that the idle disconnect ended is not one this view may take back on
        // its own, or the banner would go on claiming the agent runs outside
        // the sandbox while it is being started inside it.
        if self.outside_sandbox.is_some_and(|id| {
            self.connection_store
                .read(cx)
                .outside_sandbox_connection(id)
                .is_none()
        }) {
            self.outside_sandbox = None;
        }
        let state = Self::initial_state(
            self.agent.clone(),
            self.connection_store.clone(),
            self.connection_key.clone(),
            self.outside_sandbox,
            None,
            work_dirs,
            None,
            self.project.clone(),
            None,
            Some(adopted),
            AgentThreadSource::AgentPanel,
            window,
            cx,
        );
        self.set_server_state(state, cx);
    }

    /// The editor a draft is written in, wherever this view keeps it. Parking
    /// moves it out of the [`ThreadView`], and everything that reads or writes
    /// the draft has to follow it there.
    ///
    /// It is the root thread that is asked, not whichever thread is in front:
    /// a draft has no subagents to navigate to, and the callers that read an
    /// unsent prompt out of a view mean the thread the panel keeps for it.
    pub(crate) fn draft_message_editor(&self, cx: &App) -> Option<Entity<MessageEditor>> {
        match &self.server_state {
            ServerState::Parked(parked) => Some(parked.message_editor.clone()),
            _ => self
                .root_thread_view()
                .map(|thread_view| thread_view.read(cx).message_editor.clone()),
        }
    }

    /// Whether this view holds a thread nothing has been sent in yet. A parked
    /// view has no thread to ask, and parking only ever happens to a draft.
    pub(crate) fn is_draft(&self, cx: &App) -> bool {
        match &self.server_state {
            ServerState::Parked(_) => true,
            _ => self
                .root_thread(cx)
                .is_some_and(|thread| thread.read(cx).is_draft_thread()),
        }
    }

    /// Whether [`Self::release_idle_connection`] would let go, asked without
    /// letting go: a connection is only dropped once every view on it can, so
    /// that a view is never parked out of the panel for a connection that then
    /// stays anyway. `view` is this view's own entity id, which the store that
    /// asks already has.
    pub(crate) fn can_release_idle_connection(&self, view: gpui::EntityId, cx: &App) -> bool {
        if self.is_using_connection(cx) {
            return false;
        }
        if self.is_parked() {
            return true;
        }
        // An empty draft: parking needs the panel only for the window its
        // subscription is bound to, not for a slot to leave.
        if self.is_releasable_empty_draft(cx) {
            return self.parking_panel(cx).is_some();
        }
        let Some(panel) = self.parking_panel(cx) else {
            return false;
        };
        let Some(thread_view) = self.root_thread_view() else {
            // A draft that never started a session: there is nothing to read
            // back, and its unsent prompt would go with the view.
            return false;
        };
        let thread = thread_view.read(cx).thread.read(cx);
        if thread.status() != ThreadStatus::Idle || !thread.connection().supports_load_session() {
            return false;
        }
        panel.read(cx).is_parking_thread(self.thread_id, view)
    }

    fn parking_panel(&self, cx: &App) -> Option<Entity<AgentPanel>> {
        self.workspace.upgrade()?.read(cx).panel::<AgentPanel>(cx)
    }

    pub fn agent_key(&self) -> &Agent {
        &self.connection_key
    }

    pub fn title(&self, cx: &App) -> SharedString {
        match &self.server_state {
            ServerState::Connected(view) => view
                .active_view()
                .and_then(|v| v.read(cx).thread.read(cx).title())
                .unwrap_or_else(|| DEFAULT_THREAD_TITLE.into()),
            ServerState::Loading { .. } => self
                .loading_status
                .clone()
                .unwrap_or_else(|| "読み込み中…".into()),
            // A parked draft keeps the name its row carries, so its tab does
            // not rename itself when its agent is let go of.
            ServerState::Parked(_) => ThreadMetadataStore::try_global(cx)
                .and_then(|store| {
                    store
                        .read(cx)
                        .entry(self.thread_id)
                        .and_then(|entry| entry.title())
                })
                .unwrap_or_else(|| DEFAULT_THREAD_TITLE.into()),
            ServerState::LoadError { error, .. } => match error {
                LoadError::Unsupported { .. } => {
                    format!("{} をアップグレード", self.agent.agent_id()).into()
                }
                LoadError::FailedToInstall(_) => {
                    format!("{} のインストールに失敗しました", self.agent.agent_id()).into()
                }
                LoadError::Exited { .. } => {
                    format!("{} が終了しました", self.agent.agent_id()).into()
                }
                LoadError::Other(_) => format!("{} の読み込みエラー", self.agent.agent_id()).into(),
                LoadError::SandboxUnavailable(_) => {
                    format!("{} をサンドボックスで起動できません", self.agent.agent_id()).into()
                }
                LoadError::SandboxHostsNotAllowed(_) => format!(
                    "{} の通信に許可リストへの追加が必要です",
                    self.agent.agent_id()
                )
                .into(),
            },
        }
    }

    pub fn cancel_generation(&mut self, cx: &mut Context<Self>) {
        if let Some(active) = self.active_thread() {
            active.update(cx, |active, cx| {
                active.cancel_generation(cx);
            });
        }
    }

    pub fn parent_id(&self) -> ThreadId {
        self.thread_id
    }

    pub fn is_loading(&self) -> bool {
        matches!(self.server_state, ServerState::Loading { .. })
    }

    fn handle_thread_event(
        &mut self,
        thread: &Entity<AcpThread>,
        event: &AcpThreadEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let session_id = thread.read(cx).session_id().clone();
        let has_thread = self
            .as_connected()
            .is_some_and(|connected| connected.threads.contains_key(&session_id));
        if !has_thread {
            return;
        };
        let is_subagent = thread.read(cx).parent_session_id().is_some();
        if !is_subagent && affects_thread_metadata(event) {
            cx.emit(RootThreadUpdated);
        }
        match event {
            AcpThreadEvent::StatusChanged => {
                if let Some(active) = self.thread_view(&session_id) {
                    active.update(cx, |active, cx| {
                        active.sync_generating_indicator(cx);
                    });
                }
            }
            AcpThreadEvent::NewEntry => {
                let len = thread.read(cx).entries().len();
                let index = len - 1;
                if let Some(active) = self.thread_view(&session_id) {
                    let entry_view_state = active.read(cx).entry_view_state.clone();
                    let list_state = active.read(cx).list_state.clone();
                    entry_view_state.update(cx, |view_state, cx| {
                        view_state.sync_entry(index, thread, window, cx);
                        list_state.splice_focusable(
                            index..index,
                            [view_state
                                .entry(index)
                                .and_then(|entry| entry.focus_handle(cx))],
                        );
                    });
                    active.update(cx, |active, cx| {
                        active.sync_elicitation_state_for_entry(index, window, cx);
                        active.sync_editor_mode(cx);
                        active.sync_generating_indicator(cx);
                    });
                }
            }
            AcpThreadEvent::EntryUpdated(index) => {
                if let Some(active) = self.thread_view(&session_id) {
                    let entry_view_state = active.read(cx).entry_view_state.clone();
                    let list_state = active.read(cx).list_state.clone();
                    entry_view_state.update(cx, |view_state, cx| {
                        view_state.sync_entry(*index, thread, window, cx);
                    });
                    list_state.remeasure_items(*index..*index + 1);
                    active.update(cx, |active, cx| {
                        active.sync_elicitation_state_for_entry(*index, window, cx);
                        active.auto_expand_streaming_thought(cx);
                        active.sync_generating_indicator(cx);
                    });
                }
            }
            AcpThreadEvent::EntriesRemoved(range) => {
                if let Some(active) = self.thread_view(&session_id) {
                    let entry_view_state = active.read(cx).entry_view_state.clone();
                    let list_state = active.read(cx).list_state.clone();
                    entry_view_state.update(cx, |view_state, _cx| view_state.remove(range.clone()));
                    list_state.splice(range.clone(), 0);
                    active.update(cx, |active, cx| {
                        active.sync_editor_mode(cx);
                    });
                }
            }
            AcpThreadEvent::SubagentSpawned(subagent_session_id) => {
                self.load_subagent_session(subagent_session_id.clone(), session_id, window, cx)
            }
            AcpThreadEvent::ToolAuthorizationRequested(_) => {
                self.notify_with_sound("ツールの確認を待っています", IconName::Info, window, cx);
            }
            AcpThreadEvent::ToolAuthorizationReceived(_) => {}
            AcpThreadEvent::ElicitationRequested(_) => {
                self.notify_with_sound("入力を待っています", IconName::Info, window, cx);
            }
            AcpThreadEvent::ElicitationResponded(_) => {}
            AcpThreadEvent::Retry(retry) => {
                if let Some(active) = self.thread_view(&session_id) {
                    active.update(cx, |active, _cx| {
                        active.thread_retry_status = Some(retry.clone());
                    });
                }
            }
            AcpThreadEvent::Stopped(stop_reason) => {
                if let Some(active) = self.thread_view(&session_id) {
                    let is_generating =
                        matches!(thread.read(cx).status(), ThreadStatus::Generating);
                    active.update(cx, |active, cx| {
                        if !is_generating {
                            active.thread_retry_status.take();
                            active.clear_auto_expand_tracking(cx);
                            if active.list_state.is_following_tail() {
                                active.list_state.scroll_to_end();
                            }
                        }
                        active.sync_generating_indicator(cx);
                    });
                }
                if is_subagent {
                    if *stop_reason == acp::StopReason::EndTurn {
                        thread.update(cx, |thread, cx| {
                            thread.mark_as_subagent_output(cx);
                        });
                    }
                    return;
                }

                let sent_queued_message = if let Some(active) = self.root_thread_view() {
                    active.update(cx, |active, cx| {
                        // Don't auto-send while the user is editing the next message.
                        let is_first_editor_focused = active
                            .message_queue
                            .first()
                            .is_some_and(|entry| entry.editor.focus_handle(cx).is_focused(window));
                        if let Some(entry) = active
                            .message_queue
                            .on_generation_stopped(is_first_editor_focused)
                        {
                            active.dispatch_queued_entry(entry, window, cx);
                            true
                        } else {
                            false
                        }
                    })
                } else {
                    false
                };

                // Skip notifying when a queued message was just auto-sent: the agent
                // is not actually idle and a notification here would fire just before the
                // next turn starts.
                if !sent_queued_message {
                    let used_tools = thread.read(cx).used_tools_since_last_user_message();
                    self.notify_with_sound(
                        if used_tools {
                            "ツールの実行が完了しました"
                        } else {
                            "新しいメッセージ"
                        },
                        IconName::ZedAssistant,
                        window,
                        cx,
                    );
                }
            }
            AcpThreadEvent::Refusal => {
                let error = ThreadError::Refusal;
                if let Some(active) = self.thread_view(&session_id) {
                    active.update(cx, |active, cx| {
                        active.handle_thread_error(error, cx);
                        active.thread_retry_status.take();
                    });
                }
                if !is_subagent {
                    let model_or_agent_name = self.current_model_name(cx);
                    let notification_message = format!(
                        "{} がこのリクエストへの応答を拒否しました",
                        model_or_agent_name
                    );
                    self.notify_with_sound(&notification_message, IconName::Warning, window, cx);
                }
            }
            AcpThreadEvent::Error => {
                if let Some(active) = self.thread_view(&session_id) {
                    let is_generating =
                        matches!(thread.read(cx).status(), ThreadStatus::Generating);
                    active.update(cx, |active, cx| {
                        if !is_generating {
                            active.thread_retry_status.take();
                            if active.list_state.is_following_tail() {
                                active.list_state.scroll_to_end();
                            }
                        }
                        active.sync_generating_indicator(cx);
                    });
                }
                if !is_subagent {
                    self.notify_with_sound(
                        "エラーによりエージェントが停止しました",
                        IconName::Warning,
                        window,
                        cx,
                    );
                }
            }
            AcpThreadEvent::LoadError(error) => {
                if let Some(view) = self.root_thread_view() {
                    if view
                        .read(cx)
                        .message_editor
                        .focus_handle(cx)
                        .is_focused(window)
                    {
                        self.focus_handle.focus(window, cx)
                    }
                }
                let work_dirs = self.work_dirs_in_hand(cx);
                self.set_server_state(
                    ServerState::LoadError {
                        error: error.clone(),
                        work_dirs,
                    },
                    cx,
                );
            }
            AcpThreadEvent::TitleUpdated => {
                let override_title = ThreadMetadataStore::try_global(cx).and_then(|store| {
                    store
                        .read(cx)
                        .entry(self.thread_id)
                        .and_then(|m| m.title_override.clone())
                });
                let title = override_title.or_else(|| thread.read(cx).title());
                if let Some(title) = title
                    && let Some(active_thread) = self.thread_view(&session_id)
                {
                    let title_editor = active_thread.read(cx).title_editor.clone();
                    title_editor.update(cx, |editor, cx| {
                        if editor.text(cx) != title {
                            editor.set_text(title, window, cx);
                        }
                    });
                }
                cx.notify();
            }
            AcpThreadEvent::PromptCapabilitiesUpdated => {
                if let Some(active) = self.thread_view(&session_id) {
                    active.update(cx, |active, _cx| {
                        active
                            .session_capabilities
                            .write()
                            .set_prompt_capabilities(thread.read(_cx).prompt_capabilities());
                    });
                }
            }
            AcpThreadEvent::TokenUsageUpdated => {
                if let Some(active) = self.thread_view(&session_id) {
                    active.update(cx, |active, cx| {
                        active.update_turn_tokens(cx);
                    });
                }
            }
            AcpThreadEvent::AvailableCommandsUpdated(available_commands) => {
                if let Some(thread_view) = self.thread_view(&session_id) {
                    let available_skills = thread
                        .read(cx)
                        .connection()
                        .clone()
                        .downcast::<agent::NativeAgentConnection>()
                        .map(|native_connection| {
                            native_available_skills(&native_connection, &session_id, cx)
                        })
                        .unwrap_or_default();
                    let has_slash_completions =
                        !available_commands.is_empty() || !available_skills.is_empty();

                    let agent_display_name = self
                        .agent_server_store
                        .read(cx)
                        .agent_display_name(&self.agent.agent_id())
                        .unwrap_or_else(|| self.agent.agent_id().0.to_string().into());

                    let new_placeholder =
                        placeholder_text(agent_display_name.as_ref(), has_slash_completions);

                    thread_view.update(cx, |thread_view, cx| {
                        let mut session_capabilities = thread_view.session_capabilities.write();
                        session_capabilities.set_available_commands(available_commands.clone());
                        session_capabilities.set_available_skills(available_skills);
                        thread_view.message_editor.update(cx, |editor, cx| {
                            editor.set_placeholder_text(&new_placeholder, window, cx);
                        });
                    });
                }
            }
            AcpThreadEvent::ModeUpdated(_mode) => {
                // The connection keeps track of the mode
                cx.notify();
            }
            AcpThreadEvent::ConfigOptionsUpdated(_) => {
                // The watch task in ConfigOptionsView handles rebuilding selectors
                cx.notify();
            }
            AcpThreadEvent::WorkingDirectoriesUpdated => {
                cx.notify();
            }
            AcpThreadEvent::PromptUpdated => {
                if !is_subagent && thread.read(cx).is_draft_thread() {
                    self.schedule_draft_prompt_persist(cx);
                }
                cx.notify();
            }
        }
        cx.notify();
    }

    fn schedule_draft_prompt_persist(&mut self, cx: &mut Context<Self>) {
        let thread_id = self.thread_id;
        self.draft_prompt_persist_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(DRAFT_PROMPT_PERSIST_DEBOUNCE)
                .await;
            let persist = this.update(cx, |this, cx| {
                // ADR-F49 (new decision): a parked draft has no thread to hold
                // its prompt, so the editor is read directly. Everything else
                // about the write — the key, the debounce, the delete when it
                // ends up empty — is the same, so a draft that was parked is
                // read back on the next start like any other.
                if let ServerState::Parked(parked) = &this.server_state {
                    let snapshot = parked
                        .message_editor
                        .read(cx)
                        .draft_content_blocks_snapshot(cx);
                    return Some(if snapshot.is_empty() {
                        crate::draft_prompt_store::delete(thread_id, cx)
                    } else {
                        crate::draft_prompt_store::write(thread_id, &snapshot, cx)
                    });
                }
                let thread = this.root_thread(cx)?;
                let thread = thread.read(cx);
                if !thread.is_draft_thread() {
                    return None;
                }
                let snapshot: Vec<acp::ContentBlock> = thread
                    .draft_prompt()
                    .map(|p| p.to_vec())
                    .unwrap_or_default();
                Some(if snapshot.is_empty() {
                    crate::draft_prompt_store::delete(thread_id, cx)
                } else {
                    crate::draft_prompt_store::write(thread_id, &snapshot, cx)
                })
            });
            if let Ok(Some(persist)) = persist {
                persist.await.log_err();
            }
        }));
    }

    fn authenticate(
        &mut self,
        method: acp::AuthMethodId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let agent_id = self.agent.agent_id();
        // ADR-F55: the card leaves a subscription sign-in off an agent whose
        // settings turn it off, so nothing should ask for one; a keybinding
        // or a card drawn before the setting changed still could, and it is
        // refused here rather than at the terminal.
        if agent_servers::is_refused_subscription_auth_method(
            agent_id.0.as_ref(),
            method.0.as_ref(),
            cx,
        ) {
            let refusal = anyhow!(SUBSCRIPTION_SIGN_IN_REFUSED);
            match self.root_thread_view() {
                Some(view) => view.update(cx, |view, cx| view.handle_thread_error(refusal, cx)),
                None => log::warn!("{refusal}"),
            }
            return;
        }
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let Some(connected) = self.as_connected_mut() else {
            return;
        };
        let connection = connected.connection.clone();

        let AuthState::Unauthenticated {
            pending_auth_method,
            ..
        } = &mut connected.auth_state
        else {
            return;
        };

        let agent_telemetry_id = connection.telemetry_id();
        let runs_in_sandbox = connection.terminal_auth_runs_in_sandbox();
        // ADR-F49 decision 16: the sandbox the sign-in is wrapped in is the
        // connection's until this terminal is over.
        let sandboxed_login = runs_in_sandbox
            .then(|| connection.clone().downcast::<AcpConnection>())
            .flatten();

        if let Some(login_task) = connection.terminal_auth_task(&method, cx) {
            pending_auth_method.replace(method.clone());

            let project = self.project.clone();
            cx.emit(StateChange);
            cx.notify();
            self.auth_task = Some(cx.spawn_in(window, {
                async move |this, cx| {
                    let result = async {
                        let login = login_task.await?;
                        this.update_in(cx, |_this, window, cx| {
                            Self::spawn_external_agent_login(
                                login,
                                workspace,
                                project,
                                agent_id,
                                method.clone(),
                                false,
                                runs_in_sandbox,
                                sandboxed_login,
                                window,
                                cx,
                            )
                        })?
                        .await
                    }
                    .await;

                    match &result {
                        Ok(_) => telemetry::event!(
                            "Authenticate Agent Succeeded",
                            agent = agent_telemetry_id
                        ),
                        Err(_) => {
                            telemetry::event!(
                                "Authenticate Agent Failed",
                                agent = agent_telemetry_id,
                            )
                        }
                    }

                    this.update_in(cx, |this, window, cx| {
                        if let Err(err) = result {
                            this.cancel_request_elicitations(cx);
                            if let Some(ConnectedServerState {
                                auth_state:
                                    AuthState::Unauthenticated {
                                        pending_auth_method,
                                        ..
                                    },
                                ..
                            }) = this.as_connected_mut()
                            {
                                pending_auth_method.take();
                                cx.emit(StateChange);
                            }
                            if let Some(active) = this.root_thread_view() {
                                active.update(cx, |active, cx| {
                                    active.handle_thread_error(err, cx);
                                })
                            }
                        } else {
                            this.reset(window, cx);
                        }
                        this.auth_task.take()
                    })
                    .ok();
                }
            }));
            return;
        }

        pending_auth_method.replace(method.clone());

        let authenticate = connection.authenticate(method, cx);
        cx.emit(StateChange);
        cx.notify();
        self.auth_task = Some(cx.spawn_in(window, {
            async move |this, cx| {
                let result = authenticate.await;

                match &result {
                    Ok(_) => telemetry::event!(
                        "Authenticate Agent Succeeded",
                        agent = agent_telemetry_id
                    ),
                    Err(_) => {
                        telemetry::event!("Authenticate Agent Failed", agent = agent_telemetry_id,)
                    }
                }

                this.update_in(cx, |this, window, cx| {
                    if let Err(err) = result {
                        this.cancel_request_elicitations(cx);
                        if let Some(ConnectedServerState {
                            auth_state:
                                AuthState::Unauthenticated {
                                    pending_auth_method,
                                    ..
                                },
                            ..
                        }) = this.as_connected_mut()
                        {
                            pending_auth_method.take();
                            cx.emit(StateChange);
                        }
                        if let Some(active) = this.root_thread_view() {
                            active.update(cx, |active, cx| active.handle_thread_error(err, cx));
                        }
                    } else {
                        this.reset(window, cx);
                    }
                    this.auth_task.take()
                })
                .ok();
            }
        }));
    }

    fn load_subagent_session(
        &mut self,
        subagent_id: acp::SessionId,
        parent_session_id: acp::SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(connected) = self.as_connected() else {
            return;
        };
        if connected.threads.contains_key(&subagent_id)
            || !connected.connection.supports_load_session()
        {
            return;
        }
        let Some(parent_thread) = connected.threads.get(&parent_session_id) else {
            return;
        };
        let work_dirs = parent_thread
            .read(cx)
            .thread
            .read(cx)
            .work_dirs()
            .cloned()
            .unwrap_or_else(|| self.project.read(cx).default_path_list(cx));

        let subagent_thread_task = connected.connection.clone().load_session(
            subagent_id,
            self.project.clone(),
            work_dirs,
            None,
            cx,
        );

        cx.spawn_in(window, async move |this, cx| {
            let subagent_thread = subagent_thread_task.await?;
            this.update_in(cx, |this, window, cx| {
                let Some(conversation) = this
                    .as_connected()
                    .map(|connected| connected.conversation.clone())
                else {
                    return;
                };
                let subagent_session_id = subagent_thread.read(cx).session_id().clone();
                conversation.update(cx, |conversation, cx| {
                    conversation.register_thread(subagent_thread.clone(), cx);
                });
                let view = this.new_thread_view(
                    subagent_thread,
                    conversation,
                    false,
                    None,
                    None,
                    window,
                    cx,
                );
                let Some(connected) = this.as_connected_mut() else {
                    return;
                };
                connected.threads.insert(subagent_session_id, view);
            })
        })
        .detach();
    }

    #[allow(clippy::too_many_arguments, reason = "one sign-in's whole description")]
    fn spawn_external_agent_login(
        login: task::SpawnInTerminal,
        workspace: Entity<Workspace>,
        project: Entity<Project>,
        agent_id: AgentId,
        method: acp::AuthMethodId,
        previous_attempt: bool,
        runs_in_sandbox: bool,
        sandboxed_login: Option<Rc<AcpConnection>>,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<()>> {
        // ADR-F55: the last gate before a terminal is opened for a sign-in,
        // whatever asked for it.
        if agent_servers::is_refused_subscription_auth_method(
            agent_id.0.as_ref(),
            method.0.as_ref(),
            cx,
        ) {
            return Task::ready(Err(anyhow!(SUBSCRIPTION_SIGN_IN_REFUSED)));
        }
        let Some(terminal_panel) = workspace.read(cx).panel::<TerminalPanel>(cx) else {
            return Task::ready(Err(anyhow!("Terminal panel is unavailable")));
        };
        // Dropped when this sign-in is over, however it ends: the sandbox it
        // was wrapped in goes with it, and with that its network proxy.
        let sandbox_guard = sandboxed_login
            .clone()
            .map(|connection| LoginSandboxGuard::new(connection, login.id.0.clone()));

        window.spawn(cx, async move |cx| {
            let _sandbox_guard = sandbox_guard;
            let terminal = if runs_in_sandbox {
                // ADR-F49: already wrapped in the agent's sandbox, so it starts
                // as it is, with nothing of the terminal's own around it.
                let task = login.clone();
                let reveal = task.reveal;
                terminal_panel
                    .update_in(cx, |terminal_panel, window, cx| {
                        terminal_panel.add_terminal_with(reveal, window, cx, move |project, cx| {
                            project.create_sandboxed_terminal_task(task, cx)
                        })
                    })?
                    .await?
            } else {
                let mut task = login.clone();
                if let Some(cmd) = &task.command {
                    // Have "node" command use Zed's managed Node runtime by default
                    if cmd == "node" {
                        let resolved_node_runtime = project.update(cx, |project, cx| {
                            let agent_server_store = project.agent_server_store().clone();
                            agent_server_store.update(cx, |store, cx| {
                                store.node_runtime().map(|node_runtime| {
                                    cx.background_spawn(
                                        async move { node_runtime.binary_path().await },
                                    )
                                })
                            })
                        });

                        if let Some(resolve_task) = resolved_node_runtime {
                            if let Ok(node_path) = resolve_task.await {
                                task.command = Some(node_path.to_string_lossy().to_string());
                            }
                        }
                    }
                }
                task.shell = task::Shell::WithArguments {
                    program: task.command.take().expect("login command should be set"),
                    args: std::mem::take(&mut task.args),
                    title_override: None,
                };

                terminal_panel
                    .update_in(cx, |terminal_panel, window, cx| {
                        terminal_panel.spawn_task(&task, window, cx)
                    })?
                    .await?
            };

            let success_patterns = login_success_patterns(method.0.as_ref());
            if success_patterns.is_empty() {
                // No success patterns specified: wait for the process to exit and check exit code
                let exit_status = terminal
                    .read_with(cx, |terminal, cx| terminal.wait_for_completed_task(cx))?
                    .await;

                match exit_status {
                    Some(status) if status.success() => Ok(()),
                    Some(status) => Err(anyhow!(
                        "Login command failed with exit code: {:?}",
                        status.code()
                    )),
                    None => Err(anyhow!("Login command terminated without exit status")),
                }
            } else {
                // Look for specific output patterns to detect successful login
                let mut exit_status = terminal
                    .read_with(cx, |terminal, cx| terminal.wait_for_completed_task(cx))?
                    .fuse();

                let logged_in = cx
                    .spawn({
                        let terminal = terminal.clone();
                        async move |cx| {
                            loop {
                                cx.background_executor().timer(Duration::from_secs(1)).await;
                                let content =
                                    terminal.update(cx, |terminal, _cx| terminal.get_content())?;
                                if success_patterns
                                    .iter()
                                    .any(|pattern| content.contains(pattern))
                                {
                                    return anyhow::Ok(());
                                }
                            }
                        }
                    })
                    .fuse();
                futures::pin_mut!(logged_in);
                futures::select_biased! {
                    result = logged_in => {
                        if let Err(e) = result {
                            log::error!("{e}");
                            return Err(anyhow!("exited before logging in"));
                        }
                    }
                    _ = exit_status => {
                        if !previous_attempt
                            && project.read_with(cx, |project, _| project.is_via_remote_server())
                            && method.0.as_ref() == GEMINI_TERMINAL_AUTH_METHOD_ID
                        {
                            return cx
                                .update(|window, cx| {
                                    Self::spawn_external_agent_login(
                                        login,
                                        workspace,
                                        project.clone(),
                                        agent_id,
                                        method,
                                        true,
                                        runs_in_sandbox,
                                        sandboxed_login,
                                        window,
                                        cx,
                                    )
                                })?
                                .await;
                        }
                        return Err(anyhow!("exited before logging in"));
                    }
                }
                terminal.update(cx, |terminal, _| terminal.kill_active_task())?;
                Ok(())
            }
        })
    }

    pub fn has_user_submitted_prompt(&self, cx: &App) -> bool {
        self.root_thread_view().is_some_and(|active| {
            active
                .read(cx)
                .thread
                .read(cx)
                .entries()
                .iter()
                .any(|entry| matches!(entry, AgentThreadEntry::UserMessage(_)))
        })
    }

    fn render_auth_required_state(
        &self,
        connection: &Rc<dyn AgentConnection>,
        description: Option<&Entity<Markdown>>,
        pending_auth_method: Option<&acp::AuthMethodId>,
        window: &mut Window,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let agent_id = self.agent.agent_id();
        // ADR-F55: a subscription sign-in is offered again — the unmodified
        // binary signs itself in and this IDE never sees the credentials —
        // unless the agent's own settings turn it off, in which case it is
        // left off the card as well as refused.
        let sign_in_disabled =
            agent_servers::subscription_sign_in_disabled(agent_id.0.as_ref(), cx);
        let offered_methods = || {
            connection
                .auth_methods()
                .iter()
                .filter(|method| {
                    !sign_in_disabled
                        || !agent_servers::looks_like_subscription_sign_in(method.id().0.as_ref())
                })
                .collect::<Vec<_>>()
        };
        let auth_methods = offered_methods();
        // ADR-F53 決定 4: キーを `env` から読むエージェントだけが、このフォーク
        // の注記と入力欄を持つ。
        let show_fork_auth_notice = agent_servers::api_key_variable(agent_id.0.as_ref()).is_some();
        let subscription_sign_in =
            if sign_in_disabled {
                SubscriptionSignIn::TurnedOff
            } else if connection.auth_methods().iter().any(|method| {
                agent_servers::looks_like_subscription_sign_in(method.id().0.as_ref())
            }) {
                SubscriptionSignIn::Offered
            } else {
                SubscriptionSignIn::NotAdvertised
            };

        let agent_display_name = self
            .agent_server_store
            .read(cx)
            .agent_display_name(&self.agent.agent_id())
            .unwrap_or_else(|| self.agent.agent_id().0);

        let show_fallback_description = auth_methods.len() > 1
            && description.is_none()
            && pending_auth_method.is_none()
            && !show_fork_auth_notice;

        let auth_buttons = || {
            h_flex().justify_end().flex_wrap().gap_1().children(
                offered_methods()
                    .into_iter()
                    .enumerate()
                    .rev()
                    .map(|(ix, method)| {
                        let method_id = method.id().0.clone();
                        // ADR-F55: the vendor's label for the sign-in is
                        // English and says nothing of where it happens. Only
                        // this agent's is renamed: another vendor's sign-in is
                        // not Claude Code's, whatever its id reads like.
                        let name = if agent_id.0.as_ref() == agent_servers::CLAUDE_AGENT_ID
                            && agent_servers::looks_like_subscription_sign_in(&method_id)
                        {
                            SUBSCRIPTION_SIGN_IN_LABEL.to_string()
                        } else {
                            method.name().to_string()
                        };
                        let agent_telemetry_id = connection.telemetry_id();
                        let selector = auth_method_selector(&method_id);

                        div().debug_selector(|| selector).child(
                            Button::new(method_id.clone(), name)
                                .label_size(LabelSize::Small)
                                .map(|this| {
                                    if ix == 0 {
                                        this.style(ButtonStyle::Tinted(TintColor::Accent))
                                    } else {
                                        this.style(ButtonStyle::Outlined)
                                    }
                                })
                                .when_some(method.description(), |this, description| {
                                    this.tooltip(Tooltip::text(description.to_string()))
                                })
                                .on_click({
                                    cx.listener(move |this, _, window, cx| {
                                        telemetry::event!(
                                            "Authenticate Agent Started",
                                            agent = agent_telemetry_id,
                                            method = method_id
                                        );

                                        this.authenticate(
                                            acp::AuthMethodId::new(method_id.clone()),
                                            window,
                                            cx,
                                        )
                                    })
                                }),
                        )
                    }),
            )
        };

        if pending_auth_method.is_some() {
            return Callout::new()
                .icon(IconName::Info)
                .title(format!("{} を認証中…", agent_display_name))
                .actions_slot(
                    Icon::new(IconName::ArrowCircle)
                        .size(IconSize::Small)
                        .color(Color::Muted)
                        .with_rotate_animation(2)
                        .into_any_element(),
                )
                .into_any_element();
        }

        Callout::new()
            .icon(IconName::Info)
            .title(format!("{} の認証", agent_display_name))
            .when(auth_methods.len() == 1, |this| {
                this.actions_slot(auth_buttons())
            })
            .description_slot(
                v_flex()
                    .text_ui(cx)
                    .map(|this| {
                        if show_fallback_description {
                            this.child(
                                Label::new("次の認証方法から 1 つを選択してください:")
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                        } else if show_fork_auth_notice {
                            // The agent's own description is the vendor's
                            // English text about signing in; the note below
                            // says the same in Japanese and adds what this
                            // fork does about it, so it replaces rather than
                            // doubles it.
                            this
                        } else {
                            this.children(description.map(|desc| {
                                self.render_markdown(
                                    desc.clone(),
                                    MarkdownStyle::themed(MarkdownFont::Agent, window, cx),
                                    cx,
                                )
                            }))
                        }
                    })
                    .when(show_fork_auth_notice, |this| {
                        this.gap_1()
                            .child(self.render_fork_auth_notice(subscription_sign_in, cx))
                    })
                    .when(auth_methods.len() > 1, |this| {
                        this.gap_1().child(auth_buttons())
                    }),
            )
            .into_any_element()
    }

    /// ADR-F55: what this fork adds to an agent's authentication card — one
    /// line about the subscription sign-in, and the API key field.
    ///
    /// サインインのボタン自体はカードの通常の認証メソッドの列が出す。
    /// キーを入れる部分は [`Self::render_api_key_field`] が丸ごと持っているので、
    /// ここは置き場所と注記だけを決める。
    fn render_fork_auth_notice(
        &self,
        sign_in: SubscriptionSignIn,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        // ADR-F55: どちらを使うかを決めるのは起動される CLI で、この IDE では
        // ない。両方そろっているカードで「ログイン」を押した利用者が、成功して
        // なお API キー側に課金される、という取り違えだけを先回りして断る。
        let api_key_configured = self.api_key_variable().is_some_and(|variable| {
            agent_servers::has_env_var(self.agent.agent_id().0.as_ref(), variable, cx)
        });

        v_flex()
            .debug_selector(|| FORK_AUTH_NOTICE_SELECTOR.to_string())
            .gap_1()
            .child(
                div()
                    .debug_selector(move || sign_in.notice_selector().to_string())
                    .child(
                        Label::new(sign_in.notice())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .when(
                sign_in == SubscriptionSignIn::Offered && api_key_configured,
                |this| {
                    this.child(
                        div()
                            .debug_selector(|| API_KEY_WINS_SELECTOR.to_string())
                            .child(
                                Label::new(
                                    "API キーが設定されているときは、Claude Code は\
                                     そちらを優先します。サブスクリプションで使うには\
                                     キーを削除してください。",
                                )
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                            ),
                    )
                },
            )
            .children(self.render_api_key_field(cx))
            .when(sign_in == SubscriptionSignIn::NotAdvertised, |this| {
                this.child(
                    h_flex()
                        .justify_end()
                        .debug_selector(|| LAUNCH_CLAUDE_CODE_SELECTOR.to_string())
                        .child(
                            Button::new("launch-claude-code", "Claude Code を起動")
                                .label_size(LabelSize::Small)
                                .style(ButtonStyle::Outlined)
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(
                                        zed_actions::terminal::LaunchClaudeCode { effort: None }
                                            .boxed_clone(),
                                        cx,
                                    );
                                }),
                        ),
                )
            })
    }

    /// ADR-F53 決定 4: エージェントの `env` へ API キーを入れるための部品
    /// 一式 (案内・マスクした 1 行の入力欄・「保存」・断り書き・設定済みの
    /// 表示)。
    ///
    /// ここはエージェントがキーを `env` から読むかどうか
    /// ([`agent_servers::api_key_variable`]) しか見ない。サインインを出すか
    /// どうか (ADR-F55) とは独立していて、カードのどこへ
    /// 置いてもよい。キーを読まないエージェントのビューは入力欄そのものを
    /// 持たないので `None`。
    fn render_api_key_field(&self, cx: &Context<Self>) -> Option<impl IntoElement> {
        let editor = self.api_key_editor.clone()?;
        let variable = self.api_key_variable()?;
        let configured = agent_servers::has_env_var(self.agent.agent_id().0.as_ref(), variable, cx);
        let show_editor = !configured || self.api_key_editing;
        let saving = self.api_key_save_task.is_some();

        Some(
            v_flex()
                .gap_1()
                .map(|this| {
                    if show_editor {
                        this.child(
                            Label::new(
                                "API キー (従量課金) で使う場合は、下の欄に入力して保存\
                                 してください。",
                            )
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                        )
                        .child(
                            Label::new(format!(
                                "キーは settings.json の `agent_servers` → `{}` → `env` \
                                 → `{variable}` に平文で保存されます。",
                                self.agent.agent_id().0
                            ))
                            .size(LabelSize::Small)
                            .color(Color::Warning),
                        )
                        .child(
                            h_flex()
                                .w_full()
                                .gap_1()
                                .capture_action(cx.listener(
                                    |this, _: &editor::actions::Newline, window, cx| {
                                        this.submit_api_key(window, cx);
                                    },
                                ))
                                .child(
                                    div()
                                        .flex_1()
                                        .debug_selector(|| API_KEY_INPUT_SELECTOR.to_string())
                                        .rounded_sm()
                                        .border_1()
                                        .border_color(cx.theme().colors().border)
                                        .bg(cx.theme().colors().editor_background)
                                        .px_1()
                                        .py_0p5()
                                        .text_xs()
                                        .child(editor.into_any_element()),
                                )
                                .map(|this| {
                                    // 保存中は押せないことを見せる: 以前は 2 度目の
                                    // 押下が黙って捨てられ、何も起きていないように見えていた。
                                    if saving {
                                        this.child(
                                            div()
                                                .debug_selector(|| {
                                                    API_KEY_SAVING_SELECTOR.to_string()
                                                })
                                                .child(
                                                    Button::new("save-agent-api-key", "保存中…")
                                                        .label_size(LabelSize::Small)
                                                        .style(ButtonStyle::Outlined)
                                                        .disabled(true),
                                                ),
                                        )
                                    } else {
                                        this.child(
                                            div()
                                                .debug_selector(|| {
                                                    API_KEY_SAVE_SELECTOR.to_string()
                                                })
                                                .child(
                                                    Button::new("save-agent-api-key", "保存")
                                                        .label_size(LabelSize::Small)
                                                        .style(ButtonStyle::Outlined)
                                                        .on_click(cx.listener(
                                                            |this, _, window, cx| {
                                                                this.submit_api_key(window, cx);
                                                            },
                                                        )),
                                                ),
                                        )
                                    }
                                })
                                // 設定済みのキーを「変更」したあと、保存せずに戻る道。
                                // これが無いと `api_key_editing` が立ったまま残り、次に
                                // カードを見たときに「設定済み」ではなく空欄が出る。
                                .when(configured && !saving, |this| {
                                    this.child(
                                        div()
                                            .debug_selector(|| API_KEY_CANCEL_SELECTOR.to_string())
                                            .child(
                                                Button::new("cancel-agent-api-key", "取り消し")
                                                    .label_size(LabelSize::Small)
                                                    .style(ButtonStyle::Subtle)
                                                    .on_click(cx.listener(
                                                        |this, _, window, cx| {
                                                            this.cancel_api_key_edit(window, cx);
                                                        },
                                                    )),
                                            ),
                                    )
                                }),
                        )
                    } else {
                        this.child(
                            h_flex()
                                .w_full()
                                .gap_1()
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .debug_selector(|| API_KEY_CONFIGURED_SELECTOR.to_string())
                                        .child(
                                            ConfiguredApiCard::new(
                                                "agent-api-key-configured",
                                                "API キーは設定済みです (settings.json)",
                                            )
                                            .button_label("変更")
                                            .on_click(
                                                cx.listener(|this, _, window, cx| {
                                                    this.api_key_editing = true;
                                                    this.api_key_error = None;
                                                    if let Some(editor) =
                                                        this.api_key_editor.clone()
                                                    {
                                                        editor.update(cx, |editor, cx| {
                                                            editor.set_text("", window, cx);
                                                            // 「変更」を押した先は空の欄
                                                            // だけなので、押した流れで
                                                            // そのまま打てるようにする。
                                                            window.focus(
                                                                &editor.focus_handle(cx),
                                                                cx,
                                                            );
                                                        });
                                                    }
                                                    cx.notify();
                                                }),
                                            ),
                                        ),
                                )
                                .child(
                                    div()
                                        .debug_selector(|| API_KEY_REMOVE_SELECTOR.to_string())
                                        .child(
                                            Button::new("remove-agent-api-key", "キーを削除")
                                                .label_size(LabelSize::Small)
                                                .style(ButtonStyle::Outlined)
                                                .disabled(saving)
                                                .tooltip(Tooltip::text(
                                                    "settings.json の env からこの変数を外します",
                                                ))
                                                .on_click(cx.listener(|this, _, window, cx| {
                                                    this.remove_api_key(window, cx);
                                                })),
                                        ),
                                ),
                        )
                    }
                })
                .children(
                    self.api_key_error
                        .clone()
                        .map(|error| Label::new(error).size(LabelSize::Small).color(Color::Error)),
                ),
        )
    }

    /// ADR-F53 決定 4: このビューのエージェントが `env` から読む API キーの
    /// 変数名。読まないエージェントでは `None`。
    fn api_key_variable(&self) -> Option<&'static str> {
        agent_servers::api_key_variable(self.agent.agent_id().0.as_ref())
    }

    /// ADR-F55: 「変更」をやめて「設定済み」の表示へ戻る。
    ///
    /// 入力欄の中身も消す: 書きかけのキーをビューに残しておかない。
    /// 保存が走っている間は何もしない: その書き込みの結果が自分で
    /// 表示を戻すので、途中で切り替えると結果と画面が食い違う。
    fn cancel_api_key_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.api_key_save_task.is_some() {
            return;
        }
        self.api_key_editing = false;
        self.api_key_error = None;
        if let Some(editor) = self.api_key_editor.clone() {
            editor.update(cx, |editor, cx| {
                editor.set_text("", window, cx);
            });
        }
        cx.notify();
    }

    /// ADR-F55: 設定済みのキーを `agent_servers.<id>.env` から外す。
    ///
    /// 確認は取らない: 消しても失われるのは設定 1 行で、同じカードの入力欄から
    /// すぐに入れ直せる。消せたらカードは「設定済み」から入力欄へ戻る
    /// ([`Self::render_api_key_field`] が `has_env_var` を見ているため)。
    fn remove_api_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(variable) = self.api_key_variable() else {
            return;
        };
        if self.api_key_save_task.is_some() {
            return;
        }
        let fs = self.project.read(cx).fs().clone();
        let removed =
            agent_servers::remove_env_var(self.agent.agent_id().0.as_ref(), variable, fs, cx);

        self.api_key_error = None;
        cx.notify();
        self.api_key_save_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = removed.await;
            this.update(cx, |this, cx| {
                this.api_key_save_task = None;
                match result {
                    Ok(()) => {
                        this.api_key_editing = false;
                        this.api_key_error = None;
                    }
                    Err(error) => {
                        this.api_key_error =
                            Some(format!("API キーを削除できませんでした: {error}").into());
                    }
                }
                cx.notify();
            })
            .log_err();
        }));
    }

    /// ADR-F53 決定 4: 入力されたキーを `agent_servers.<id>.env` へ書く。
    ///
    /// キーの文字列はここから先へ持ち出さない: ログにもトーストにも載せず、
    /// 保存できたら入力欄からも消す。画面に残す唯一の文字列は
    /// [`Self::api_key_error`] で、そこへ値を入れることは決してない。
    fn submit_api_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.api_key_editor.clone() else {
            return;
        };
        let Some(variable) = self.api_key_variable() else {
            return;
        };
        if self.api_key_save_task.is_some() {
            return;
        }
        let key = editor.read(cx).text(cx);
        let fs = self.project.read(cx).fs().clone();

        let written = match agent_servers::set_env_var(
            self.agent.agent_id().0.as_ref(),
            variable,
            &key,
            fs,
            cx,
        ) {
            Ok(written) => written,
            Err(error) => {
                self.api_key_error = Some(format!("{error}").into());
                cx.notify();
                return;
            }
        };

        self.api_key_error = None;
        cx.notify();
        // 書き込みの失敗理由は設定側が作る文字列で、このコードが中身を決め
        // られない。万一そこへ値が混ざっても画面には出ないよう、伏せてから
        // 載せる。
        let saved = key.trim().to_string();
        self.api_key_save_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = written.await;
            this.update_in(cx, |this, window, cx| {
                this.api_key_save_task = None;
                match result {
                    Ok(()) => {
                        this.api_key_editing = false;
                        this.api_key_error = None;
                        if let Some(editor) = this.api_key_editor.clone() {
                            editor.update(cx, |editor, cx| {
                                editor.set_text("", window, cx);
                            });
                        }
                    }
                    Err(error) => {
                        let mut reason = format!("API キーを保存できませんでした: {error}");
                        if !saved.is_empty() {
                            reason = reason.replace(&saved, "***");
                        }
                        this.api_key_error = Some(reason.into());
                    }
                }
                cx.notify();
            })
            .log_err();
        }));
    }

    fn sync_request_elicitation_states(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(store) = self.request_elicitation_store() else {
            self.request_elicitation_form_states.clear();
            return;
        };

        let elicitations = store
            .read(cx)
            .elicitations()
            .iter()
            .map(|elicitation| {
                let is_pending = matches!(elicitation.status, ElicitationStatus::Pending { .. });
                let schema = match &elicitation.request.mode {
                    acp::ElicitationMode::Form(mode) => Some(mode.requested_schema.clone()),
                    _ => None,
                };
                (elicitation.id.clone(), is_pending, schema)
            })
            .collect::<Vec<_>>();

        let known_ids = elicitations
            .iter()
            .map(|(id, _, _)| id.clone())
            .collect::<HashSet<_>>();
        self.request_elicitation_form_states
            .retain(|id, _| known_ids.contains(id));

        for (id, is_pending, schema) in elicitations {
            if is_pending
                && let Some(schema) = schema
                && !self.request_elicitation_form_states.contains_key(&id)
            {
                self.request_elicitation_form_states
                    .insert(id, ElicitationFormState::new(&schema, window, cx));
            } else if !is_pending {
                self.request_elicitation_form_states.remove(&id);
            }
        }
    }

    fn render_request_elicitations(
        &self,
        connection: &Rc<dyn AgentConnection>,
        view: WeakEntity<Self>,
        cx: &App,
    ) -> Vec<AnyElement> {
        let Some(store) = connection.request_elicitations() else {
            return Vec::new();
        };

        let handlers = Self::request_elicitation_card_handlers(view);
        let agent_display_name = self
            .agent_server_store
            .read(cx)
            .agent_display_name(&self.agent.agent_id())
            .unwrap_or_else(|| self.agent.agent_id().0);

        store
            .read(cx)
            .elicitations()
            .iter()
            .enumerate()
            .filter(|(_, elicitation)| should_render_elicitation(elicitation))
            .map(|(ix, elicitation)| {
                ElicitationCard::new(
                    ix,
                    elicitation,
                    agent_display_name.clone(),
                    self.request_elicitation_form_states.get(&elicitation.id),
                    handlers.clone(),
                )
                .render(cx)
                .into_any_element()
            })
            .collect()
    }

    fn request_elicitation_card_handlers(view: WeakEntity<Self>) -> ElicitationCardHandlers {
        ElicitationCardHandlers::new(
            {
                let view = view.clone();
                move |elicitation_id, window, cx| {
                    view.update(cx, |this, cx| {
                        this.submit_request_elicitation(elicitation_id, window, cx);
                    })
                    .log_err();
                }
            },
            {
                let view = view.clone();
                move |elicitation_id, window, cx| {
                    view.update(cx, |this, cx| {
                        this.decline_request_elicitation(elicitation_id, window, cx);
                    })
                    .log_err();
                }
            },
            {
                let view = view.clone();
                move |elicitation_id, window, cx| {
                    view.update(cx, |this, cx| {
                        this.cancel_request_elicitation(elicitation_id, window, cx);
                    })
                    .log_err();
                }
            },
            {
                let view = view.clone();
                move |elicitation_id, _window, cx| {
                    view.update(cx, |this, cx| {
                        this.dismiss_request_url_elicitation(elicitation_id, cx);
                    })
                    .log_err();
                }
            },
            move |_elicitation_id, url, _window, cx| cx.open_url(&url),
            {
                let view = view.clone();
                move |elicitation_id, field_name, value, cx| {
                    view.update(cx, |this, cx| {
                        this.update_request_elicitation_form_state(
                            &elicitation_id,
                            |form| form.set_boolean(&field_name, value),
                            cx,
                        );
                    })
                    .log_err();
                }
            },
            {
                let view = view.clone();
                move |elicitation_id, field_name, value, cx| {
                    view.update(cx, |this, cx| {
                        this.update_request_elicitation_form_state(
                            &elicitation_id,
                            |form| form.set_single_select(&field_name, value),
                            cx,
                        );
                    })
                    .log_err();
                }
            },
            move |elicitation_id, field_name, value, selected, cx| {
                view.update(cx, |this, cx| {
                    this.update_request_elicitation_form_state(
                        &elicitation_id,
                        |form| form.set_multi_select(&field_name, value, selected),
                        cx,
                    );
                })
                .log_err();
            },
        )
    }

    fn update_request_elicitation_form_state(
        &mut self,
        elicitation_id: &ElicitationEntryId,
        update: impl FnOnce(&mut ElicitationFormState),
        cx: &mut Context<Self>,
    ) {
        if let Some(form) = self.request_elicitation_form_states.get_mut(elicitation_id) {
            update(form);
            self.notify_request_elicitation_renderers(cx);
        }
    }

    fn notify_request_elicitation_renderers(&self, cx: &mut Context<Self>) {
        if let Some(active_thread) = self.active_thread().cloned() {
            active_thread.update(cx, |_thread, cx| cx.notify());
        }
        cx.notify();
    }

    fn submit_request_elicitation(
        &mut self,
        elicitation_id: ElicitationEntryId,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(store) = self.request_elicitation_store() else {
            return;
        };

        let mode = store
            .read(cx)
            .elicitation(&elicitation_id)
            .map(|(_, elicitation)| elicitation.request.mode.clone());
        let Some(mode) = mode else {
            return;
        };

        match mode {
            acp::ElicitationMode::Form(mode) => {
                let Some(state) = self
                    .request_elicitation_form_states
                    .get_mut(&elicitation_id)
                else {
                    return;
                };
                let Some(submission) = state.begin_submission(cx) else {
                    return;
                };
                let schema = mode.requested_schema;
                let validation_task = cx.background_spawn(async move {
                    let result = submission.validate(&schema);
                    (submission, result)
                });
                self.notify_request_elicitation_renderers(cx);
                cx.spawn(async move |this, cx| {
                    let (submission, result) = validation_task.await;
                    this.update(cx, |this, cx| {
                        let is_current = this
                            .request_elicitation_form_states
                            .get_mut(&elicitation_id)
                            .is_some_and(|state| {
                                state.validation_matches_current_values(&submission, cx)
                            });
                        if !is_current {
                            this.notify_request_elicitation_renderers(cx);
                            return;
                        }
                        match result {
                            Ok(content) => {
                                this.respond_to_request_elicitation(
                                    elicitation_id,
                                    acp::CreateElicitationResponse::new(
                                        acp::ElicitationAction::Accept(
                                            acp::ElicitationAcceptAction::new().content(content),
                                        ),
                                    ),
                                    cx,
                                );
                            }
                            Err(errors) => {
                                this.update_request_elicitation_form_state(
                                    &elicitation_id,
                                    |state| state.set_errors(errors),
                                    cx,
                                );
                            }
                        }
                    })
                    .log_err();
                })
                .detach();
            }
            acp::ElicitationMode::Url(_) => {
                self.respond_to_request_elicitation(
                    elicitation_id,
                    acp::CreateElicitationResponse::new(acp::ElicitationAction::Accept(
                        acp::ElicitationAcceptAction::new(),
                    )),
                    cx,
                );
            }
            _ => {}
        }
    }

    fn decline_request_elicitation(
        &mut self,
        elicitation_id: ElicitationEntryId,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.respond_to_request_elicitation(
            elicitation_id,
            acp::CreateElicitationResponse::new(acp::ElicitationAction::Decline),
            cx,
        );
    }

    fn cancel_request_elicitation(
        &mut self,
        elicitation_id: ElicitationEntryId,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.respond_to_request_elicitation(
            elicitation_id,
            acp::CreateElicitationResponse::new(acp::ElicitationAction::Cancel),
            cx,
        );
    }

    fn dismiss_request_url_elicitation(
        &mut self,
        elicitation_id: ElicitationEntryId,
        cx: &mut Context<Self>,
    ) {
        self.request_elicitation_form_states.remove(&elicitation_id);
        if let Some(store) = self.request_elicitation_store() {
            store.update(cx, |store, cx| {
                store.cancel_elicitation(&elicitation_id, cx);
            });
        }
        self.notify_request_elicitation_renderers(cx);
    }

    fn respond_to_request_elicitation(
        &mut self,
        elicitation_id: ElicitationEntryId,
        response: acp::CreateElicitationResponse,
        cx: &mut Context<Self>,
    ) {
        self.request_elicitation_form_states.remove(&elicitation_id);
        if let Some(store) = self.request_elicitation_store() {
            store.update(cx, |store, cx| {
                store.respond_to_elicitation(&elicitation_id, response, cx);
            });
        }
        cx.notify();
    }

    fn cancel_request_elicitations(&mut self, cx: &mut App) {
        self.request_elicitation_form_states.clear();
        if let Some(store) = self.request_elicitation_store() {
            store.update(cx, |store, cx| store.clear(cx));
        }
    }

    fn clear_resolved_request_elicitations(&mut self, cx: &mut App) {
        if let Some(connection) = self.request_elicitation_connection() {
            self.clear_resolved_request_elicitations_for_connection(&connection, cx);
        }
    }

    fn clear_resolved_request_elicitations_for_connection(
        &mut self,
        connection: &Rc<dyn AgentConnection>,
        cx: &mut App,
    ) {
        let Some(store) = connection.request_elicitations() else {
            return;
        };
        let cleared_ids = store.update(cx, |store, cx| store.clear_resolved(cx));
        for id in cleared_ids {
            self.request_elicitation_form_states.remove(&id);
        }
    }

    fn emit_load_error_telemetry(&self, error: &LoadError) {
        let error_kind = match error {
            LoadError::Unsupported { .. } => "unsupported",
            LoadError::FailedToInstall(_) => "failed_to_install",
            LoadError::Exited { .. } => "exited",
            LoadError::Other(_) => "other",
            LoadError::SandboxUnavailable(_) => "sandbox_unavailable",
            LoadError::SandboxHostsNotAllowed(_) => "sandbox_hosts_not_allowed",
        };

        let agent_name = self.agent.agent_id();

        telemetry::event!(
            "Agent Panel Error Shown",
            agent = agent_name,
            kind = error_kind,
            message = error.to_string(),
        );
    }

    fn render_load_error(
        &self,
        error: &LoadError,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (title, message, action_slot): (_, SharedString, _) = match error {
            LoadError::Unsupported {
                command: path,
                current_version,
                minimum_version,
            } => {
                return self.render_unsupported(path, current_version, minimum_version, window, cx);
            }
            LoadError::FailedToInstall(msg) => (
                "インストールに失敗しました",
                msg.into(),
                Some(self.create_copy_button(msg.to_string()).into_any_element()),
            ),
            LoadError::Exited { status, stderr } => {
                let mut message = format!("サーバーがステータス {status} で終了しました");
                if let Some(stderr) = stderr {
                    message.push_str("\n");
                    message.push_str(stderr);
                };
                let action_slot = stderr
                    .is_some()
                    .then(|| self.create_copy_button(message.clone()).into_any_element());
                ("起動に失敗しました", message.into(), action_slot)
            }
            LoadError::Other(msg) => (
                "起動に失敗しました",
                msg.into(),
                Some(self.create_copy_button(msg.to_string()).into_any_element()),
            ),
            LoadError::SandboxUnavailable(reason) => (
                "サンドボックスで起動できません",
                format!("{reason}\n{OUTSIDE_SANDBOX_EXPLANATION}").into(),
                Some(
                    Button::new("start-agent-outside-sandbox", START_OUTSIDE_SANDBOX_LABEL)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.start_agent_outside_sandbox_once(window, cx)
                        }))
                        .into_any_element(),
                ),
            ),
            LoadError::SandboxHostsNotAllowed(hosts) => {
                let mut message = format!(
                    "サンドボックスの {} は次のホストに接続しますが、IDE 全体の許可リストに\
                     ありません: {}",
                    self.agent.agent_id(),
                    hosts.join("、")
                );
                // The last attempt's reason belongs on this card, not on one
                // of its own: the allowlist still refuses these hosts, so the
                // way out is the same button, and replacing the card with a
                // plain error would take it away (F1's `consent.error`).
                if let Some(error) = &self.agent_hosts_consent_error {
                    message.push('\n');
                    message.push_str(error);
                } else if self.agent_hosts_consent_pending {
                    message.push('\n');
                    message.push_str(crate::agent_registry_ui::ALLOWLIST_PICKUP_PENDING);
                }
                (
                    "許可リストへの追加が必要です",
                    message.into(),
                    Some(
                        Button::new(
                            "allow-agent-sandbox-hosts",
                            match self.agent_hosts_consent_error {
                                Some(_) => RETRY_AGENT_HOSTS_LABEL,
                                None => ADD_AGENT_HOSTS_LABEL,
                            },
                        )
                        // A confirm already in flight owns this consent
                        // (`ask_to_allow_agent_hosts`'s `agent_hosts_consent_pending`
                        // guard); disabling the button here avoids opening a
                        // second prompt for the same hosts from a stray click.
                        .disabled(self.agent_hosts_consent_pending)
                        .on_click(cx.listener({
                            let hosts = hosts.clone();
                            move |this, _, window, cx| {
                                this.ask_to_allow_agent_hosts(hosts.clone(), window, cx)
                            }
                        }))
                        .into_any_element(),
                    ),
                )
            }
        };

        // ADR-F49 (new decision): the plain failures — the agent did not come
        // up, it exited, it could not be installed — had nothing on their card
        // but the error itself, and the idle disconnect turned that into a
        // dead end a user reaches by sending in a draft: the panel hands the
        // same failed view back for as long as that agent is the selected one.
        // The other errors each lead somewhere of their own (sign in, allow
        // the hosts, start outside the sandbox, upgrade), so only these get a
        // plain way back. It builds the thread again, unsent text and all
        // ([`Self::reset`] reads the draft-prompt store).
        let can_retry = matches!(
            error,
            LoadError::Other(_) | LoadError::Exited { .. } | LoadError::FailedToInstall(_)
        );

        Callout::new()
            .severity(Severity::Error)
            .icon(IconName::XCircleFilled)
            .title(title)
            .description(message)
            .actions_slot(
                h_flex()
                    .gap_1()
                    .children(action_slot)
                    .when(can_retry, |this| {
                        this.child(
                            div()
                                .debug_selector(|| RETRY_AGENT_START_SELECTOR.to_string())
                                .child(
                                    Button::new("retry-agent-start", RETRY_AGENT_START_LABEL)
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.reset(window, cx)
                                        })),
                                ),
                        )
                    }),
            )
            .into_any_element()
    }

    fn render_unsupported(
        &self,
        path: &SharedString,
        version: &SharedString,
        minimum_version: &SharedString,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (heading_label, description_label) = (
            format!(
                "Zed で使うには {} をアップグレードしてください",
                self.agent.agent_id()
            ),
            if version.is_empty() {
                format!(
                    "現在 {} を使用していますが、有効な --version を返しません",
                    path
                )
            } else {
                format!(
                    "現在 {} を使用していますが、バージョンが {} です ({minimum_version} 以上が必要です)",
                    path, version
                )
            },
        );

        v_flex()
            .w_full()
            .p_3p5()
            .gap_2p5()
            .border_t_1()
            .border_color(cx.theme().colors().border)
            .bg(linear_gradient(
                180.,
                linear_color_stop(cx.theme().colors().editor_background.opacity(0.4), 4.),
                linear_color_stop(cx.theme().status().info_background.opacity(0.), 0.),
            ))
            .child(
                v_flex().gap_0p5().child(Label::new(heading_label)).child(
                    Label::new(description_label)
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
            )
            .into_any_element()
    }

    pub(crate) fn as_native_connection(
        &self,
        cx: &App,
    ) -> Option<Rc<agent::NativeAgentConnection>> {
        self.root_thread(cx)?
            .read(cx)
            .connection()
            .clone()
            .downcast()
    }

    pub fn as_native_thread(&self, cx: &App) -> Option<Entity<agent::Thread>> {
        self.as_native_connection(cx)?
            .thread(self.root_session_id.as_ref()?, cx)
    }

    fn render_markdown(
        &self,
        markdown: Entity<Markdown>,
        style: MarkdownStyle,
        cx: &App,
    ) -> MarkdownElement {
        render_agent_markdown(
            markdown,
            style,
            &self.workspace,
            &self.code_span_resolver,
            cx,
        )
    }

    fn notify_with_sound(
        &mut self,
        caption: impl Into<SharedString>,
        icon: IconName,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        #[cfg(feature = "audio")]
        self.play_notification_sound(window, cx);
        self.show_notification(caption, icon, window, cx);
    }

    fn is_visible(&self, multi_workspace: &Entity<MultiWorkspace>, cx: &Context<Self>) -> bool {
        let Some(workspace) = self.workspace.upgrade() else {
            return false;
        };

        let multi_workspace = multi_workspace.read(cx);
        multi_workspace.sidebar_open() && multi_workspace.is_threads_list_view_active(cx)
            || multi_workspace.workspace() == &workspace
                && self.is_visible_in_agent_panel(&workspace, cx)
    }

    fn is_visible_in_agent_panel(&self, workspace: &Entity<Workspace>, cx: &Context<Self>) -> bool {
        AgentPanel::is_visible(workspace, cx)
            && workspace
                .read(cx)
                .panel::<AgentPanel>(cx)
                .is_some_and(|panel| {
                    panel
                        .read(cx)
                        .visible_conversation_view()
                        .map(|conversation_view| conversation_view.entity_id())
                        == Some(cx.entity_id())
                })
    }

    fn agent_status_visible(&self, window: &Window, cx: &Context<Self>) -> bool {
        if !window.is_window_active() {
            return false;
        }

        if let Some(multi_workspace) = window.root::<MultiWorkspace>().flatten() {
            self.is_visible(&multi_workspace, cx)
        } else {
            self.workspace
                .upgrade()
                .is_some_and(|workspace| self.is_visible_in_agent_panel(&workspace, cx))
        }
    }

    #[cfg(feature = "audio")]
    fn play_notification_sound(&self, window: &Window, cx: &mut Context<Self>) {
        let visible = window.is_window_active()
            && if let Some(mw) = window.root::<MultiWorkspace>().flatten() {
                self.is_visible(&mw, cx)
            } else {
                self.workspace
                    .upgrade()
                    .is_some_and(|workspace| self.is_visible_in_agent_panel(&workspace, cx))
            };
        let settings = AgentSettings::get_global(cx);
        if settings.play_sound_when_agent_done.should_play(visible) {
            Audio::play_sound(Sound::AgentDone, cx);
        }
    }

    fn show_notification(
        &mut self,
        caption: impl Into<SharedString>,
        icon: IconName,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.notifications.is_empty() {
            return;
        }

        let settings = AgentSettings::get_global(cx);

        let should_notify = !self.agent_status_visible(window, cx);

        if !should_notify {
            return;
        }

        let Some(root_thread) = self.root_thread_view() else {
            return;
        };
        let root_thread = root_thread.read(cx).thread.read(cx);
        let root_thread_id = self.thread_id;
        let root_work_dirs = root_thread.work_dirs().cloned();
        let root_title = root_thread.title();

        let title = root_title
            .clone()
            .unwrap_or_else(|| self.agent.agent_id().0);

        match settings.notify_when_agent_waiting {
            NotifyWhenAgentWaiting::PrimaryScreen => {
                window.request_attention();
                if let Some(primary) = cx.primary_display() {
                    self.pop_up(
                        icon,
                        caption.into(),
                        title,
                        root_thread_id,
                        root_work_dirs,
                        root_title,
                        window,
                        primary,
                        cx,
                    );
                }
            }
            NotifyWhenAgentWaiting::AllScreens => {
                window.request_attention();
                let caption = caption.into();
                for screen in cx.displays() {
                    self.pop_up(
                        icon,
                        caption.clone(),
                        title.clone(),
                        root_thread_id,
                        root_work_dirs.clone(),
                        root_title.clone(),
                        window,
                        screen,
                        cx,
                    );
                }
            }
            NotifyWhenAgentWaiting::Never => {
                // Don't show anything
            }
        }
    }

    fn pop_up(
        &mut self,
        icon: IconName,
        caption: SharedString,
        title: SharedString,
        root_thread_id: ThreadId,
        root_work_dirs: Option<PathList>,
        root_title: Option<SharedString>,
        window: &mut Window,
        screen: Rc<dyn PlatformDisplay>,
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

        if let Some(screen_window) = cx
            .open_window(options, |_window, cx| {
                cx.new(|_cx| {
                    AgentNotification::new(title.clone(), Some(caption.clone()), icon, project_name)
                })
            })
            .log_err()
            && let Some(pop_up) = screen_window.entity(cx).log_err()
        {
            self.notifications.push(screen_window);
            self.bind_notification_subscriptions(
                screen_window,
                &pop_up,
                root_thread_id,
                root_work_dirs,
                root_title,
                window,
                cx,
            );
        }
    }

    /// 通知ポップアップに張る購読は 4 本ともウィンドウに紐づく (受諾したときに
    /// 前へ出すウィンドウ / 本体が見えたら消す判定 / パネルのイベント)。タブが
    /// 別ウィンドウへ移ったら丸ごと張り直せるよう、材料ごとここにまとめる
    /// (ADR-F36)。
    fn bind_notification_subscriptions(
        &mut self,
        screen_window: WindowHandle<AgentNotification>,
        pop_up: &Entity<AgentNotification>,
        root_thread_id: ThreadId,
        root_work_dirs: Option<PathList>,
        root_title: Option<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut subscriptions = Vec::new();

        subscriptions.push(cx.subscribe_in(pop_up, window, {
            let root_work_dirs = root_work_dirs.clone();
            let root_title = root_title.clone();
            move |this, _, event, window, cx| match event {
                AgentNotificationEvent::Accepted => {
                    let Some(handle) = window.window_handle().downcast::<MultiWorkspace>() else {
                        log::error!("root view should be a MultiWorkspace");
                        return;
                    };
                    cx.activate(true);

                    let workspace_handle = this.workspace.clone();
                    let agent = this.connection_key.clone();
                    let root_work_dirs = root_work_dirs.clone();
                    let root_title = root_title.clone();

                    cx.defer(move |cx| {
                        handle
                            .update(cx, |multi_workspace, window, cx| {
                                window.activate_window();
                                if let Some(workspace) = workspace_handle.upgrade() {
                                    multi_workspace.activate(workspace.clone(), None, window, cx);
                                    workspace.update(cx, |workspace, cx| {
                                        workspace.reveal_panel::<AgentPanel>(window, cx);
                                        if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
                                            panel.update(cx, |panel, cx| {
                                                panel.load_agent_thread(
                                                    agent.clone(),
                                                    root_thread_id,
                                                    root_work_dirs.clone(),
                                                    root_title.clone(),
                                                    true,
                                                    AgentThreadSource::AgentPanel,
                                                    window,
                                                    cx,
                                                );
                                            });
                                        }
                                        workspace.focus_panel::<AgentPanel>(window, cx);
                                    });
                                }
                            })
                            .log_err();
                    });

                    this.dismiss_notifications(cx);
                }
                AgentNotificationEvent::Dismissed => {
                    this.dismiss_notifications(cx);
                }
            }
        }));

        let dismiss_if_visible = {
            let pop_up_weak = pop_up.downgrade();
            move |this: &ConversationView,
                  window: &mut Window,
                  cx: &mut Context<ConversationView>| {
                if this.agent_status_visible(window, cx)
                    && let Some(pop_up) = pop_up_weak.upgrade()
                {
                    pop_up.update(cx, |notification, cx| {
                        notification.dismiss(cx);
                    });
                }
            }
        };

        subscriptions.push({
            let dismiss_if_visible = dismiss_if_visible.clone();
            cx.observe_window_activation(window, move |this, window, cx| {
                dismiss_if_visible(this, window, cx);
            })
        });

        if let Some(multi_workspace) = window.root::<MultiWorkspace>().flatten() {
            let dismiss_if_visible = dismiss_if_visible.clone();
            subscriptions.push(cx.observe_in(
                &multi_workspace,
                window,
                move |this, _, window, cx| {
                    dismiss_if_visible(this, window, cx);
                },
            ));
        }

        if let Some(panel) = self
            .workspace
            .upgrade()
            .and_then(|workspace| workspace.read(cx).panel::<AgentPanel>(cx))
        {
            subscriptions.push(cx.subscribe_in(
                &panel,
                window,
                move |this, _, event: &AgentPanelEvent, window, cx| match event {
                    AgentPanelEvent::ActiveViewChanged | AgentPanelEvent::ActiveViewFocused => {
                        dismiss_if_visible(this, window, cx);
                    }
                    AgentPanelEvent::EntryChanged
                    | AgentPanelEvent::TerminalCloseRequested { .. }
                    | AgentPanelEvent::ThreadInteracted { .. } => {}
                },
            ));
        }

        self.notification_subscriptions.insert(
            screen_window,
            NotificationBinding {
                root_thread_id,
                root_work_dirs,
                root_title,
                _subscriptions: subscriptions,
            },
        );
    }

    /// 開いたままの通知ポップアップの購読を、いまのウィンドウで張り直す
    /// (ADR-F36)。
    fn rebind_notification_subscriptions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let bindings = self
            .notification_subscriptions
            .iter()
            .map(|(screen_window, binding)| {
                (
                    *screen_window,
                    binding.root_thread_id,
                    binding.root_work_dirs.clone(),
                    binding.root_title.clone(),
                )
            })
            .collect::<Vec<_>>();

        for (screen_window, root_thread_id, root_work_dirs, root_title) in bindings {
            let Some(pop_up) = screen_window.entity(cx).log_err() else {
                continue;
            };
            self.bind_notification_subscriptions(
                screen_window,
                &pop_up,
                root_thread_id,
                root_work_dirs,
                root_title,
                window,
                cx,
            );
        }
    }

    pub(crate) fn dismiss_notifications(&mut self, cx: &mut Context<Self>) -> bool {
        let had_notifications = !self.notifications.is_empty();
        for window in self.notifications.drain(..) {
            window
                .update(cx, |_, window, _| {
                    window.remove_window();
                })
                .ok();

            self.notification_subscriptions.remove(&window);
        }
        had_notifications
    }

    fn agent_ui_font_size_changed(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(entry_view_state) = self
            .active_thread()
            .map(|active| active.read(cx).entry_view_state.clone())
        {
            entry_view_state.update(cx, |entry_view_state, cx| {
                entry_view_state.agent_ui_font_size_changed(cx);
            });
        }
    }

    fn invalidate_mermaid_caches(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let current_theme_id = cx.theme().id.clone();
        if self.last_theme_id.as_ref() == Some(&current_theme_id) {
            return;
        }
        self.last_theme_id = Some(current_theme_id);

        if let Some(connected) = self.as_connected() {
            let threads: Vec<_> = connected
                .conversation
                .read(cx)
                .threads
                .values()
                .cloned()
                .collect();
            for thread in threads {
                thread.update(cx, |thread, cx| {
                    thread.invalidate_mermaid_caches(cx);
                });
            }
        }
    }

    pub(crate) fn insert_dragged_files(
        &self,
        paths: Vec<project::ProjectPath>,
        added_worktrees: Vec<Entity<project::Worktree>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // ADR-F49 (new decision): a parked draft takes the drop into the same
        // editor it always had. Attaching a file is not a reason to start the
        // agent again — the send is — and what is attached rides along when it
        // does, because that editor is the one the thread comes back into.
        if let Some(message_editor) = self.draft_message_editor(cx) {
            message_editor.update(cx, |editor, cx| {
                editor.insert_dragged_files(paths, added_worktrees, window, cx);
                editor.focus_handle(cx).focus(window, cx);
            });
        }
    }

    /// Inserts the selected text into the message editor or the message being
    /// edited, if any.
    pub(crate) fn insert_selection(
        &self,
        selection: AgentContextSelection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(active_thread) = self.active_thread() {
            active_thread.update(cx, |thread, cx| {
                thread.active_editor(cx).update(cx, |editor, cx| {
                    editor.insert_selections(selection, window, cx);
                })
            });
        } else if let Some(message_editor) = self.draft_message_editor(cx) {
            message_editor.update(cx, |editor, cx| {
                editor.insert_selections(selection, window, cx);
            });
        }
    }

    fn current_model_name(&self, cx: &App) -> SharedString {
        // For native agent (Zed Agent), use the specific model name (e.g., "Claude 3.5 Sonnet")
        // For ACP agents, use the agent name (e.g., "Claude Agent", "Gemini CLI")
        // This provides better clarity about what refused the request
        if self.as_native_connection(cx).is_some() {
            self.root_thread_view()
                .and_then(|active| active.read(cx).model_selector.clone())
                .and_then(|selector| selector.read(cx).active_model(cx))
                .map(|model| model.name.clone())
                .unwrap_or_else(|| SharedString::from("モデル"))
        } else {
            // ACP agent - use the agent name (e.g., "Claude Agent", "Gemini CLI")
            self.agent.agent_id().0
        }
    }

    /// ADR-F49 decision 19: whether this view's agent is the one started
    /// outside its sandbox, which is this view's alone.
    fn runs_outside_sandbox(&self, cx: &App) -> bool {
        self.outside_sandbox.is_some_and(|id| {
            self.connection_store
                .read(cx)
                .outside_sandbox_connection(id)
                .is_some()
        })
    }

    /// While that agent runs, the view says so: the confirmation was for this
    /// start only, and the same agent in another view is still in its sandbox.
    fn render_outside_sandbox_banner(&self, cx: &App) -> Option<impl IntoElement> {
        // ADR-F49 (new decision): a parked view has no agent running at all,
        // in or out of its sandbox, so the banner would be saying something
        // that stopped being true when the idle disconnect took the
        // connection. Asking for it back goes into the sandbox again, which
        // [`Self::unpark`] is what makes true.
        if self.is_parked() || !self.runs_outside_sandbox(cx) {
            return None;
        }
        Some(
            h_flex()
                .w_full()
                .px_2()
                .py_1()
                .gap_1p5()
                .bg(cx.theme().status().warning_background)
                .border_b_1()
                .border_color(cx.theme().colors().border)
                .child(
                    Icon::new(IconName::Warning)
                        .size(IconSize::XSmall)
                        .color(Color::Warning),
                )
                .child(
                    Label::new(OUTSIDE_SANDBOX_RUNNING_LABEL)
                        .size(LabelSize::Small)
                        .color(Color::Warning),
                ),
        )
    }

    /// ADR-F49 decision 19: asks, every time and with no "always", before
    /// the agent starts outside its sandbox, for this once.
    fn start_agent_outside_sandbox_once(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let on_remote_host = self.project.read(cx).is_via_remote_server();
        let program = self.agent.agent_id().to_string();
        let confirmed = crate::sandboxed_claude_code::confirm_running_outside_sandbox(
            &program,
            on_remote_host,
            None,
            window,
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if !confirmed.await {
                return;
            }
            this.update_in(cx, |this, window, cx| {
                let key = this.connection_key.clone();
                let agent = this.agent.clone();
                // Kept apart from the connections other views share: the user
                // confirmed this start here, for this view.
                let id = this.connection_store.update(cx, |store, cx| {
                    store.connect_outside_sandbox(key, agent, cx)
                });
                this.outside_sandbox = Some(id);
                this.reset(window, cx);
            })
            .log_err();
        })
        .detach();
    }

    /// ADR-F49 decision 13: the consent the sandboxed Claude Code CLI asks
    /// for, then the hosts go into the IDE-wide allowlist and the agent starts
    /// again once the allowlist has them.
    ///
    /// The wait for the allowlist's watcher to take in the write is the
    /// shared one (`agent_registry_ui::add_consented_hosts_and_await_pickup`,
    /// F1's `agent_registry_ui::confirm_install` shape) rather than a poll of
    /// our own: it reacts to the policy's own change instead of guessing how
    /// long the watcher takes, and it reports a pickup that never lands
    /// instead of retrying the connection anyway. `agent_hosts_consent_pending`
    /// guards a confirm already in flight the same way `confirm_install`'s
    /// `consent.waiting` does — a second one would write and wait twice.
    ///
    /// A wait that gave up leaves the card (and this button) in place with its
    /// reason on it, so this is also the way back in. By then the hosts are
    /// already in the file, so there is nothing left to consent to and the
    /// second attempt goes straight to waiting for the policy.
    fn ask_to_allow_agent_hosts(
        &mut self,
        hosts: Vec<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.agent_hosts_consent_pending {
            return;
        }
        self.agent_hosts_consent_error = None;
        let missing = crate::agent_registry_ui::missing_hosts(&hosts, cx);
        if missing.is_empty() {
            // Everything this agent needs is allowed already (the watcher
            // caught up after a wait gave up): asking again would prompt for
            // an addition there is nothing to add for.
            self.reset(window, cx);
            return;
        }
        let detail = agent_hosts_detail(&self.agent.agent_id(), &hosts, &missing);
        let answer = window.prompt(
            PromptLevel::Warning,
            AGENT_HOSTS_MESSAGE,
            Some(&detail),
            &[
                PromptButton::ok(ADD_AGENT_HOSTS_LABEL),
                PromptButton::cancel(DECLINE_AGENT_HOSTS_LABEL),
            ],
            cx,
        );
        self.agent_hosts_consent_pending = true;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            if !answer.await.is_ok_and(|index| index == 0) {
                this.update(cx, |this, cx| {
                    this.agent_hosts_consent_pending = false;
                    cx.notify();
                })
                .log_err();
                return;
            }
            let added = missing.iter().map(ToString::to_string).collect::<Vec<_>>();
            let write_and_wait = match cx.update(|_, cx| {
                crate::agent_registry_ui::add_consented_hosts_and_await_pickup(added, cx)
            }) {
                Ok(write_and_wait) => write_and_wait,
                Err(error) => {
                    log::warn!("agent_ui: the consented hosts were not written: {error:#}");
                    this.update(cx, |this, cx| {
                        this.agent_hosts_consent_pending = false;
                        cx.notify();
                    })
                    .log_err();
                    return;
                }
            };
            let result = write_and_wait.await;
            this.update_in(cx, |this, window, cx| {
                this.agent_hosts_consent_pending = false;
                match result {
                    Ok(()) => this.reset(window, cx),
                    Err(failure) => {
                        // The card that asked stays up with the reason on it,
                        // so the user can try again; a `LoadError::Other`
                        // here would replace it with a dead end.
                        this.agent_hosts_consent_error = Some(failure.message());
                    }
                }
                cx.notify();
            })
            .log_err();
        })
        .detach();
    }

    /// ADR-F49 (new decision): the row a parked draft shows where its thread
    /// would put the model and attachment controls.
    ///
    /// Those controls need a connection and cannot be drawn without one, and a
    /// row of buttons that quietly disappears after five minutes reads as a
    /// bug. Saying what happened — and offering the way back that does not
    /// involve sending anything — costs one line and leaves nothing to guess.
    fn render_parked_notice(&self, cx: &mut Context<Self>) -> Callout {
        Callout::new()
            .severity(Severity::Info)
            .icon(IconName::Info)
            .title("使われていないのでエージェントを止めています。送信すると再接続します")
            .border_position(ui::CalloutBorderPosition::Top)
            .actions_slot(
                Button::new("reconnect-parked-agent", "今すぐ再接続")
                    .label_size(LabelSize::Small)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.unpark(Unpark::Only, window, cx);
                    })),
            )
    }

    fn create_copy_button(&self, message: impl Into<String>) -> impl IntoElement {
        let message = message.into();

        CopyButton::new("copy-error-message", message).tooltip_label("エラーメッセージをコピー")
    }

    pub(crate) fn reauthenticate(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.cancel_request_elicitations(cx);
        if let Some(active) = self.root_thread_view() {
            active.update(cx, |active, cx| active.clear_thread_error(cx));
        }
        let this = cx.weak_entity();
        let Some(connection) = self.as_connected().map(|c| c.connection.clone()) else {
            debug_panic!("This should not be possible");
            return;
        };
        window.defer(cx, |window, cx| {
            Self::handle_auth_required(this, AuthRequired::new(), connection, window, cx);
        })
    }

    pub(crate) fn logout(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.supports_logout() {
            return;
        }

        if let Some(active) = self.root_thread_view() {
            active.update(cx, |active, cx| active.clear_thread_error(cx));
        }
        let Some(connection) = self
            .as_connected()
            .map(|connected| connected.connection.clone())
        else {
            return;
        };
        let logout = connection.logout(cx);
        self.auth_task = Some(cx.spawn_in(window, {
            async move |this, cx| {
                let result = logout.await;
                this.update_in(cx, |this, window, cx| {
                    if let Err(err) = result {
                        if let Some(active) = this.root_thread_view() {
                            active.update(cx, |active, cx| active.handle_thread_error(err, cx));
                        }
                    } else {
                        this.cancel_request_elicitations(cx);
                        if let Some(connected) = this.as_connected_mut() {
                            connected.auth_state = AuthState::Unauthenticated {
                                description: None,
                                pending_auth_method: None,
                            };
                            cx.emit(StateChange);
                            if let Some(view) = connected.active_view()
                                && view
                                    .read(cx)
                                    .message_editor
                                    .focus_handle(cx)
                                    .is_focused(window)
                            {
                                this.focus_handle.focus(window, cx)
                            }
                            cx.notify();
                        }
                    }
                    drop(this.auth_task.take());
                })
                .ok();
            }
        }));
    }
}

fn loading_contents_spinner(size: IconSize) -> AnyElement {
    Icon::new(IconName::LoadCircle)
        .size(size)
        .color(Color::Accent)
        .with_rotate_animation(3)
        .into_any_element()
}

fn native_available_skills(
    native_connection: &agent::NativeAgentConnection,
    session_id: &acp::SessionId,
    cx: &App,
) -> Vec<AvailableSkill> {
    native_connection
        .available_skills(session_id, cx)
        .into_iter()
        .map(|skill| AvailableSkill {
            name: skill.name.into(),
            description: skill.description.into(),
            source: skill.source,
            skill_file_path: skill.skill_file_path,
            warning: skill.warning,
        })
        .collect()
}

fn placeholder_text(agent_name: &str, has_commands: bool) -> String {
    if agent_name == agent::ZED_AGENT_ID.as_ref() {
        format!(
            "{} にメッセージを送信、@ でコンテキストを追加、/ でコマンド",
            agent_name
        )
    } else if has_commands {
        format!(
            "{} にメッセージを送信 — @ でコンテキストを追加、/ でコマンド",
            agent_name
        )
    } else {
        format!("{} にメッセージを送信 — @ でコンテキストを追加", agent_name)
    }
}

impl Focusable for ConversationView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        if let ServerState::Parked(parked) = &self.server_state {
            return parked.message_editor.focus_handle(cx);
        }
        match self.active_thread() {
            Some(thread) => thread.read(cx).focus_handle(cx),
            None => self.focus_handle.clone(),
        }
    }
}

impl ConversationView {
    pub(crate) fn activation_focus_handle(&self, cx: &App) -> FocusHandle {
        if let ServerState::Parked(parked) = &self.server_state {
            return parked.message_editor.focus_handle(cx);
        }
        self.active_thread()
            .map(|thread| thread.read(cx).activation_focus_handle(cx))
            .unwrap_or_else(|| self.focus_handle.clone())
    }

    /// ADR-F36: このスレッドを載せたタブが別のウィンドウへ移されたときに、
    /// 持ち主のワークスペースを張り替える。旧ウィンドウのワークスペースを
    /// 参照したままだと、ファイルを開く導線もモーダルも消えた窓を見に行く。
    pub(crate) fn set_workspace(&mut self, workspace: WeakEntity<Workspace>, cx: &mut App) {
        self.workspace = workspace.clone();
        let Some(connected) = self.as_connected() else {
            return;
        };
        for thread_view in connected.threads.values().cloned().collect::<Vec<_>>() {
            thread_view.update(cx, |thread_view, cx| {
                thread_view.set_workspace(workspace.clone(), cx);
            });
        }
    }

    /// フォーカスリスナーは登録先のウィンドウに紐づくので、別ウィンドウへ
    /// 移したタブは張り直さないと効かなくなる。
    ///
    /// エージェントの接続待ちのあいだに移されたときはまだ [`ThreadView`] が
    /// 無いので張れない。その場合は予約だけ取っておき、接続できた時点で
    /// [`Self::apply_pending_window_rebind`] がやり直す。
    pub(crate) fn rebind_window_focus_listeners(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.rebind_notification_subscriptions(window, cx);

        if let ServerState::Parked(parked) = &self.server_state {
            parked.message_editor.clone().update(cx, |editor, cx| {
                editor.rebind_window_focus_listeners(window, cx)
            });
            return;
        }

        let Some(connected) = self.as_connected() else {
            self.pending_window_rebind = Some(window.window_handle());
            return;
        };
        for thread_view in connected.threads.values().cloned().collect::<Vec<_>>() {
            thread_view.update(cx, |thread_view, cx| {
                thread_view.rebind_window_focus_listeners(window, cx);
            });
        }
    }

    /// 接続が確立して [`ThreadView`] が生えた時点で、接続待ちのあいだに来た
    /// 「別ウィンドウへ移った」の張り直しを消化する (ADR-F36)。
    ///
    /// ここは読み込みタスクの `update_in` の中 = 自分がリース中なので、
    /// アプリ側の `defer` に逃がしてから対象ウィンドウを開く。
    fn apply_pending_window_rebind(&mut self, cx: &mut Context<Self>) {
        let Some(window_handle) = self.pending_window_rebind.take() else {
            return;
        };
        let workspace = self.workspace.clone();
        let this = cx.weak_entity();
        cx.defer(move |cx: &mut App| {
            window_handle
                .update(cx, |_, window, cx| {
                    this.update(cx, |this, cx| {
                        this.set_workspace(workspace, cx);
                        this.rebind_window_focus_listeners(window, cx);
                    })
                    .log_err();
                })
                .log_err();
        });
    }
}

#[cfg(any(test, feature = "test-support"))]
impl ConversationView {
    /// Builds this view's thread again from scratch, the way the sign-in, the
    /// allowlist and the failure cards all do once the user acts on them.
    // `pub(crate)`、かつ呼ぶのはこのクレートのテストだけなので、
    // `test-support` だけを有効にしたビルドでは dead_code になる。
    #[cfg(test)]
    pub(crate) fn test_reset(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.reset(window, cx);
    }

    /// Lets go of this view's connection the way the idle disconnect does,
    /// without waiting for the sweep that would otherwise ask for it.
    #[cfg(test)]
    pub(crate) fn test_park(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.park(window, cx)
    }

    /// Expands a tool call so its content is visible.
    /// This is primarily useful for visual testing.
    pub fn expand_tool_call(&mut self, tool_call_id: acp::ToolCallId, cx: &mut Context<Self>) {
        if let Some(active) = self.active_thread() {
            active.update(cx, |active, cx| {
                active.entry_view_state.update(cx, |state, _cx| {
                    state.expand_tool_call(tool_call_id);
                });
            });
            cx.notify();
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn set_updated_at(&mut self, updated_at: Instant, cx: &mut Context<Self>) {
        let Some(connected) = self.as_connected_mut() else {
            return;
        };

        connected.conversation.update(cx, |conversation, _cx| {
            conversation.updated_at = Some(updated_at);
        });
    }
}

impl Render for ConversationView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_request_elicitation_states(window, cx);
        let request_elicitation_connection = self.request_elicitation_connection();
        let active_thread_renders_request_elicitations =
            self.active_thread_renders_request_elicitations();
        let content = match &self.server_state {
            ServerState::Loading { .. } => {
                let label_text = self
                    .loading_status
                    .clone()
                    .unwrap_or_else(|| "読み込み中…".into());
                v_flex()
                    .flex_1()
                    .size_full()
                    .items_center()
                    .justify_center()
                    .child(
                        Label::new(label_text).color(Color::Muted).with_animation(
                            "loading-agent-label",
                            Animation::new(Duration::from_secs(2))
                                .repeat()
                                .with_easing(pulsating_between(0.3, 0.7)),
                            |label, delta| label.alpha(delta),
                        ),
                    )
                    .into_any()
            }
            ServerState::LoadError { error: e, .. } => v_flex()
                .flex_1()
                .size_full()
                .items_center()
                .justify_end()
                .child(self.render_load_error(e, window, cx))
                .into_any(),
            ServerState::Connected(ConnectedServerState {
                connection,
                auth_state:
                    AuthState::Unauthenticated {
                        description,
                        pending_auth_method,
                    },
                ..
            }) => v_flex()
                .flex_1()
                .size_full()
                .justify_end()
                .child(self.render_auth_required_state(
                    connection,
                    description.as_ref(),
                    pending_auth_method.as_ref(),
                    window,
                    cx,
                ))
                .into_any_element(),
            ServerState::Connected(connected) => {
                if let Some(view) = connected.active_view() {
                    view.clone().into_any_element()
                } else {
                    debug_panic!("This state should never be reached");
                    div().into_any_element()
                }
            }
            // ADR-F49 (new decision): the draft box, and nothing else. There
            // is no spinner and no "reconnecting" card because nothing is
            // being waited for: the agent starts again when the user sends,
            // and until then this is the same empty page a new thread shows.
            ServerState::Parked(parked) => {
                // Laid out like `ThreadView::render_message_editor` draws a
                // thread with no messages yet, so that letting the agent go
                // does not move the box the user is writing in.
                let max_content_width = AgentSettings::get_global(cx).max_content_width;
                h_flex()
                    .flex_1()
                    .size_full()
                    .py_2()
                    .bg(cx.theme().colors().editor_background)
                    .justify_center()
                    .on_action(cx.listener(|this, _: &ReconnectParkedAgent, window, cx| {
                        this.unpark(Unpark::Only, window, cx);
                    }))
                    .child(
                        v_flex()
                            .when_some(max_content_width, |this, max_w| this.flex_basis(max_w))
                            .when(max_content_width.is_none(), |this| this.w_full())
                            .min_w_0()
                            .h_full()
                            .px_2()
                            .flex_shrink_1()
                            .flex_grow_0()
                            .gap_2()
                            .child(
                                v_flex()
                                    .relative()
                                    .w_full()
                                    .min_h_0()
                                    .flex_1()
                                    .pt_1()
                                    .pr_2p5()
                                    .child(parked.message_editor.clone()),
                            )
                            .child(self.render_parked_notice(cx)),
                    )
                    .into_any_element()
            }
        };

        v_flex()
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .children(self.render_outside_sandbox_banner(cx))
            .child(v_flex().flex_1().min_h_0().child(content))
            .when(!active_thread_renders_request_elicitations, |this| {
                this.children(request_elicitation_connection.as_ref().map_or_else(
                    Vec::new,
                    |connection| {
                        self.render_request_elicitations(connection, cx.entity().downgrade(), cx)
                    },
                ))
            })
    }
}

fn render_agent_markdown(
    markdown: Entity<Markdown>,
    style: MarkdownStyle,
    workspace: &WeakEntity<Workspace>,
    code_span_resolver: &AgentCodeSpanResolver,
    cx: &App,
) -> MarkdownElement {
    let workspace = workspace.clone();
    let worktree_roots = code_span_resolver.worktree_roots(cx);
    let resolver = code_span_resolver.clone();
    MarkdownElement::new(markdown, style)
        .code_block_renderer(markdown::CodeBlockRenderer::Default {
            copy_button_visibility: markdown::CopyButtonVisibility::VisibleOnHover,
            wrap_button_visibility: markdown::WrapButtonVisibility::VisibleOnHover,
            border: false,
        })
        .image_resolver(move |dest_url, _cx| resolve_agent_image(dest_url, &worktree_roots))
        .on_url_click(move |text, window, cx| {
            thread_view::open_link(text, &workspace, window, cx);
        })
        .on_code_span_link(move |text, cx| resolver.try_resolve(text, cx))
}

/// Shared, cloneable handle for resolving inline markdown code spans like
/// `` `src/main.rs:42` `` to clickable workspace file links.
#[derive(Clone)]
pub(crate) struct AgentCodeSpanResolver {
    inner: Arc<AgentCodeSpanResolverInner>,
}

/// Maximum number of memoized code-span resolutions kept in the cache.
const CODE_SPAN_CACHE_CAPACITY: NonZeroUsize = match NonZeroUsize::new(2048) {
    Some(n) => n,
    None => unreachable!(),
};

struct AgentCodeSpanResolverInner {
    project: WeakEntity<Project>,
    cache: Mutex<LruCache<Arc<str>, Option<SharedString>>>,
}

impl AgentCodeSpanResolver {
    pub(crate) fn new(project: &WeakEntity<Project>, _cx: &App) -> Self {
        Self {
            inner: Arc::new(AgentCodeSpanResolverInner {
                project: project.clone(),
                cache: Mutex::new(LruCache::new(CODE_SPAN_CACHE_CAPACITY)),
            }),
        }
    }

    pub(crate) fn clear_cache(&self) {
        self.inner.cache.lock().clear();
    }

    /// Absolute paths of every current worktree.
    /// Used by the markdown image resolver, which needs the same set of roots.
    fn worktree_roots(&self, cx: &App) -> Vec<PathBuf> {
        self.inner
            .project
            .upgrade()
            .map(|project| {
                project
                    .read(cx)
                    .visible_worktrees(cx)
                    .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn try_resolve(&self, text: &str, cx: &App) -> Option<SharedString> {
        let trimmed = sanitize_path_text(text.trim());
        if !Self::is_path_like(trimmed) {
            return None;
        }

        if let Some(cached) = self.inner.cache.lock().get(trimmed).cloned() {
            return cached;
        }

        let resolved = self.resolve_uncached(trimmed, cx);
        self.inner
            .cache
            .lock()
            .push(Arc::from(trimmed), resolved.clone());
        resolved
    }

    fn resolve_uncached(&self, trimmed: &str, cx: &App) -> Option<SharedString> {
        let path_with_position = PathWithPosition::parse_str(trimmed);
        let candidate_path = &path_with_position.path;
        if candidate_path.as_os_str().is_empty() {
            return None;
        }

        let project = self.inner.project.upgrade()?;
        let project = project.read(cx);
        for worktree in project.visible_worktrees(cx) {
            let worktree = worktree.read(cx);
            for relative_path in Self::candidate_relative_paths(
                candidate_path,
                &worktree.abs_path(),
                worktree.path_style(),
            ) {
                let project_path = ProjectPath {
                    worktree_id: worktree.id(),
                    path: relative_path.clone(),
                };
                let Some(entry) = project.entry_for_path(&project_path, cx) else {
                    continue;
                };
                if !entry.is_file() {
                    continue;
                }

                let abs_path = worktree.absolutize(&relative_path);
                let mention = match path_with_position.row.and_then(|row| row.checked_sub(1)) {
                    Some(line) => MentionUri::Selection {
                        abs_path: Some(abs_path),
                        line_range: line..=line,
                        column: path_with_position
                            .column
                            .map(|column| column.saturating_sub(1)),
                    },
                    None => MentionUri::File { abs_path },
                };

                return Some(mention.to_uri().to_string().into());
            }
        }

        None
    }

    fn candidate_relative_paths(
        path: &Path,
        worktree_abs_path: &Path,
        path_style: PathStyle,
    ) -> Vec<Arc<RelPath>> {
        let path_text = path.to_string_lossy();
        let relative_path: Option<Arc<RelPath>> =
            if util::paths::is_absolute(path_text.as_ref(), path_style) {
                path_style
                    .strip_prefix(path, worktree_abs_path)
                    .map(std::borrow::Cow::into_owned)
                    .map(Into::into)
            } else {
                RelPath::new(path, path_style)
                    .ok()
                    .map(std::borrow::Cow::into_owned)
                    .map(Into::into)
            };

        let Some(relative_path) = relative_path else {
            return Vec::new();
        };

        let mut paths = vec![relative_path.clone()];
        if let Some(root_name) = worktree_abs_path.file_name().and_then(|name| name.to_str())
            && let Ok(root_name) = RelPath::new(Path::new(root_name), path_style)
            && let Ok(stripped) = relative_path.strip_prefix(root_name.as_ref())
            && !stripped.is_empty()
        {
            paths.push(Arc::from(stripped));
        }
        paths
    }

    fn is_path_like(text: &str) -> bool {
        if text.len() < 3
            || text.contains("://")
            || text.contains('|')
            || text.chars().any(char::is_control)
            || text.chars().all(|character| character.is_ascii_digit())
        {
            return false;
        }

        let path = PathWithPosition::parse_str(text).path;
        let path_text = path.to_string_lossy();
        if path_text.contains('/') || path_text.contains('\\') {
            return true;
        }

        path.extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| !extension.is_empty())
    }
}

fn plan_label_markdown_style(
    status: &acp::PlanEntryStatus,
    window: &Window,
    cx: &App,
) -> MarkdownStyle {
    let default_md_style = MarkdownStyle::themed(MarkdownFont::Agent, window, cx);

    MarkdownStyle {
        base_text_style: TextStyle {
            color: cx.theme().colors().text_muted,
            strikethrough: if matches!(status, acp::PlanEntryStatus::Completed) {
                Some(gpui::StrikethroughStyle {
                    thickness: px(1.),
                    color: Some(cx.theme().colors().text_muted.opacity(0.8)),
                })
            } else {
                None
            },
            ..default_md_style.base_text_style
        },
        ..default_md_style
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use acp_thread::StubAgentConnection;
    use action_log::ActionLog;
    use agent::{AgentTool, EditFileTool, FetchTool, TerminalTool, ToolPermissionContext};
    use agent_servers::FakeAcpAgentServer;
    use editor::MultiBufferOffset;
    use editor::actions::Paste;
    use feature_flags::{AcpBetaFeatureFlag, FeatureFlag as _, FeatureFlagAppExt as _};
    use fs::FakeFs;
    use gpui::{ClipboardItem, EventEmitter, TestAppContext, VisualTestContext, point, size};
    use http_client::{EgressPolicy, EgressPolicyHandle, RequestHost};
    use parking_lot::Mutex;
    use project::Project;
    use serde_json::json;
    use settings::SettingsStore;
    use std::any::Any;
    use std::path::{Path, PathBuf};
    use std::rc::Rc;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use workspace::{Item, MultiWorkspace};

    use crate::agent_panel;
    use crate::completion_provider::AgentContextSource;
    use crate::test_support::register_test_sidebar;
    use crate::thread_metadata_store::ThreadMetadataStore;

    use super::*;

    #[test]
    fn test_data_retention_error_maps_from_provider_error() {
        // The agent wraps the provider error in a fresh `anyhow::Error`, so
        // the mapping must downcast to `LanguageModelCompletionError` rather
        // than matching on the anyhow error directly.
        let provider_error = LanguageModelCompletionError::DataRetentionConsentRequired {
            model_name: "Claude Fable 5".to_string(),
        };
        let error = ThreadError::from(anyhow!(provider_error));
        assert!(
            matches!(error, ThreadError::DataRetentionConsentRequired),
            "expected ThreadError::DataRetentionConsentRequired, got: {error:?}"
        );
    }

    #[gpui::test]
    async fn test_drop(cx: &mut TestAppContext) {
        init_test(cx);

        let (conversation_view, _cx) =
            setup_conversation_view(StubAgentServer::default_response(), cx).await;
        let weak_view = conversation_view.downgrade();
        drop(conversation_view);
        assert!(!weak_view.is_upgradable());
    }

    #[gpui::test]
    async fn test_drop_preserves_shared_pending_request_elicitations(cx: &mut TestAppContext) {
        init_test(cx);
        cx.update(|cx| {
            cx.update_flags(true, vec![AcpBetaFeatureFlag::NAME.to_string()]);
        });

        let response = Arc::new(Mutex::new(None));
        let server = ReleaseRequestElicitationServer {
            response: response.clone(),
        };
        let (conversation_view, cx) = setup_conversation_view(server, cx).await;
        let _connection = conversation_view
            .read_with(cx, |view, _cx| view.request_elicitation_connection())
            .expect("conversation should have an active connection");
        let store = _connection
            .request_elicitations()
            .expect("connection should expose request elicitations");
        store.read_with(cx, |store, _cx| {
            assert_eq!(
                store.elicitations().len(),
                1,
                "test should start with one pending request elicitation"
            );
        });

        assert_eq!(*response.lock(), None);
        let weak_view = conversation_view.downgrade();
        drop(conversation_view);
        cx.update(|_, _| {});
        cx.run_until_parked();

        assert!(!weak_view.is_upgradable());
        store.read_with(cx, |store, _cx| {
            assert_eq!(
                store.elicitations().len(),
                1,
                "view release should not clear connection-wide request elicitations"
            );
        });
        assert_eq!(*response.lock(), None);

        store.update(cx, |store, cx| store.clear(cx));
        cx.run_until_parked();
        assert!(matches!(
            response.lock().as_ref(),
            Some(acp::ElicitationAction::Cancel)
        ));
    }

    #[gpui::test]
    async fn test_state_transition_preserves_shared_pending_request_elicitations(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        cx.update(|cx| {
            cx.update_flags(true, vec![AcpBetaFeatureFlag::NAME.to_string()]);
        });

        let response = Arc::new(Mutex::new(None));
        let server = ReleaseRequestElicitationServer {
            response: response.clone(),
        };
        let (conversation_view, cx) = setup_conversation_view(server, cx).await;
        let connection = conversation_view
            .read_with(cx, |view, _cx| view.request_elicitation_connection())
            .expect("conversation should have an active connection");
        let store = connection
            .request_elicitations()
            .expect("connection should expose request elicitations");
        store.read_with(cx, |store, _cx| {
            assert_eq!(
                store.elicitations().len(),
                1,
                "test should start with one pending request elicitation"
            );
        });

        conversation_view.update(cx, |view, cx| {
            view.set_server_state(
                ServerState::LoadError {
                    error: LoadError::Other("load failed".into()),
                    work_dirs: None,
                },
                cx,
            );
        });
        cx.run_until_parked();

        store.read_with(cx, |store, _cx| {
            assert_eq!(
                store.elicitations().len(),
                1,
                "leaving a connection should not clear connection-wide request elicitations"
            );
        });
        assert_eq!(*response.lock(), None);

        store.update(cx, |store, cx| store.clear(cx));
        cx.run_until_parked();
        assert!(matches!(
            response.lock().as_ref(),
            Some(acp::ElicitationAction::Cancel)
        ));
    }

    #[gpui::test]
    async fn test_successful_session_creation_clears_resolved_request_elicitations(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        cx.update(|cx| {
            cx.update_flags(true, vec![AcpBetaFeatureFlag::NAME.to_string()]);
        });

        let store = cx.update(|cx| cx.new(|_| ElicitationStore::default()));
        let response = Arc::new(Mutex::new(None));
        let server = SessionCreationRequestElicitationServer {
            store: store.clone(),
            response: response.clone(),
        };
        let (conversation_view, cx) = setup_conversation_view(server, cx).await;
        let first_request_id = acp::RequestId::Number(1);
        let second_request_id = acp::RequestId::Number(2);
        let first_elicitation_id = store.read_with(cx, |store, _cx| {
            assert_eq!(
                store.elicitations().len(),
                2,
                "session creation should be waiting on one prompt with another prompt still pending"
            );
            store
                .elicitations()
                .iter()
                .find_map(|elicitation| {
                    let acp::ElicitationScope::Request(scope) = elicitation.request.scope() else {
                        return None;
                    };
                    (&scope.request_id == &first_request_id).then(|| elicitation.id.clone())
                })
                .expect("first request-scoped elicitation should exist")
        });

        store.update(cx, |store, cx| {
            store.respond_to_elicitation(
                &first_elicitation_id,
                acp::CreateElicitationResponse::new(acp::ElicitationAction::Accept(
                    acp::ElicitationAcceptAction::new(),
                )),
                cx,
            );
        });
        cx.run_until_parked();

        assert!(matches!(
            response.lock().as_ref(),
            Some(acp::ElicitationAction::Accept(_))
        ));
        conversation_view.read_with(cx, |view, _cx| {
            let connected = view
                .as_connected()
                .expect("session creation should complete successfully");
            assert!(
                connected.active_id.is_some(),
                "successful session creation should install an active thread"
            );
        });
        store.read_with(cx, |store, _cx| {
            let [remaining] = store.elicitations() else {
                panic!(
                    "expected only the pending request elicitation to remain, got {:?}",
                    store.elicitations()
                );
            };
            let acp::ElicitationScope::Request(scope) = remaining.request.scope() else {
                panic!("expected request-scoped elicitation");
            };
            assert_eq!(scope.request_id, second_request_id);
            assert!(matches!(
                remaining.status,
                ElicitationStatus::Pending { .. }
            ));
        });

        store.update(cx, |store, cx| store.clear(cx));
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn test_external_source_prompt_requires_manual_send(cx: &mut TestAppContext) {
        init_test(cx);

        let Some(prompt) = crate::ExternalSourcePrompt::new("Write me a script") else {
            panic!("expected prompt from external source to sanitize successfully");
        };
        let initial_content = AgentInitialContent::FromExternalSource(prompt);

        let (conversation_view, cx) = setup_conversation_view_with_initial_content(
            StubAgentServer::default_response(),
            initial_content,
            cx,
        )
        .await;

        active_thread(&conversation_view, cx).read_with(cx, |view, cx| {
            assert!(view.show_external_source_prompt_warning);
            assert_eq!(view.thread.read(cx).entries().len(), 0);
            assert_eq!(view.message_editor.read(cx).text(cx), "Write me a script");
        });
    }

    #[gpui::test]
    async fn test_external_source_prompt_warning_clears_after_send(cx: &mut TestAppContext) {
        init_test(cx);

        let Some(prompt) = crate::ExternalSourcePrompt::new("Write me a script") else {
            panic!("expected prompt from external source to sanitize successfully");
        };
        let initial_content = AgentInitialContent::FromExternalSource(prompt);

        let (conversation_view, cx) = setup_conversation_view_with_initial_content(
            StubAgentServer::default_response(),
            initial_content,
            cx,
        )
        .await;

        active_thread(&conversation_view, cx)
            .update_in(cx, |view, window, cx| view.send(window, cx));
        cx.run_until_parked();

        active_thread(&conversation_view, cx).read_with(cx, |view, cx| {
            assert!(!view.show_external_source_prompt_warning);
            assert_eq!(view.message_editor.read(cx).text(cx), "");
            assert_eq!(view.thread.read(cx).entries().len(), 2);
        });
    }

    #[gpui::test]
    async fn test_agent_code_span_resolver_resolves_worktree_paths(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            util::path!("/project"),
            json!({
                "src": {
                    "main.rs": ""
                },
                "README.md": ""
            }),
        )
        .await;

        let project = Project::test(fs, [Path::new(util::path!("/project"))], cx).await;
        let resolver = cx.update(|cx| AgentCodeSpanResolver::new(&project.downgrade(), cx));

        let uri = cx
            .update(|cx| resolver.try_resolve("src/main.rs:10", cx))
            .expect("expected worktree-relative file path to resolve");
        assert_eq!(
            MentionUri::parse(&uri, PathStyle::local()).unwrap(),
            MentionUri::Selection {
                abs_path: Some(PathBuf::from(util::path!("/project/src/main.rs"))),
                line_range: 9..=9,
                column: None,
            }
        );

        let uri = cx
            .update(|cx| resolver.try_resolve("src/main.rs:10:5", cx))
            .expect("expected worktree-relative file path with row and column to resolve");
        assert_eq!(
            MentionUri::parse(&uri, PathStyle::local()).unwrap(),
            MentionUri::Selection {
                abs_path: Some(PathBuf::from(util::path!("/project/src/main.rs"))),
                line_range: 9..=9,
                column: Some(4),
            }
        );

        let uri = cx
            .update(|cx| resolver.try_resolve("src/main.rs:0", cx))
            .expect("`:0` should fall back to a file mention instead of returning None");
        assert_eq!(
            MentionUri::parse(&uri, PathStyle::local()).unwrap(),
            MentionUri::File {
                abs_path: PathBuf::from(util::path!("/project/src/main.rs")),
            }
        );

        assert!(cx.update(|cx| resolver.try_resolve("String", cx)).is_none());
        assert!(
            cx.update(|cx| resolver.try_resolve("does/not/exist.rs", cx))
                .is_none()
        );
        assert!(
            cx.update(|cx| resolver.try_resolve("src/main.rs.", cx))
                .is_some()
        );

        let uri = cx
            .update(|cx| resolver.try_resolve("project/src/main.rs:10", cx))
            .expect("expected root-prefixed worktree path to resolve");
        assert_eq!(
            MentionUri::parse(&uri, PathStyle::local()).unwrap(),
            MentionUri::Selection {
                abs_path: Some(PathBuf::from(util::path!("/project/src/main.rs"))),
                line_range: 9..=9,
                column: None,
            }
        );
    }

    #[gpui::test]
    async fn test_notification_for_stop_event(cx: &mut TestAppContext) {
        init_test(cx);

        let (conversation_view, cx) =
            setup_conversation_view(StubAgentServer::default_response(), cx).await;

        let message_editor = message_editor(&conversation_view, cx);
        message_editor.update_in(cx, |editor, window, cx| {
            editor.set_text("Hello", window, cx);
        });

        cx.deactivate_window();

        active_thread(&conversation_view, cx)
            .update_in(cx, |view, window, cx| view.send(window, cx));

        cx.run_until_parked();

        assert!(
            cx.windows()
                .iter()
                .any(|window| window.downcast::<AgentNotification>().is_some())
        );
    }

    #[gpui::test]
    async fn test_no_notification_when_queued_message_will_be_auto_sent(cx: &mut TestAppContext) {
        init_test(cx);

        let connection = StubAgentConnection::new();
        let (conversation_view, cx) =
            setup_conversation_view(StubAgentServer::new(connection.clone()), cx).await;
        add_to_workspace(conversation_view.clone(), cx);

        let message_editor = message_editor(&conversation_view, cx);
        message_editor.update_in(cx, |editor, window, cx| {
            editor.set_text("first", window, cx);
        });

        active_thread(&conversation_view, cx)
            .update_in(cx, |view, window, cx| view.send(window, cx));

        cx.run_until_parked();

        let session_id = conversation_view.read_with(cx, |view, cx| {
            view.active_thread()
                .unwrap()
                .read(cx)
                .thread
                .read(cx)
                .session_id()
                .clone()
        });

        active_thread(&conversation_view, cx).update_in(cx, |thread, window, cx| {
            thread.add_to_queue(
                vec![acp::ContentBlock::Text(acp::TextContent::new(
                    "queued".to_string(),
                ))],
                vec![],
                window,
                cx,
            );
        });

        cx.deactivate_window();
        cx.run_until_parked();

        cx.update(|_, cx| {
            connection.send_update(
                session_id.clone(),
                acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                    "first response".into(),
                )),
                cx,
            );
            connection.end_turn(session_id, acp::StopReason::EndTurn);
        });

        cx.run_until_parked();

        assert_eq!(
            cx.windows()
                .iter()
                .filter(|window| window.downcast::<AgentNotification>().is_some())
                .count(),
            0,
            "No notification should fire when a queued message will be auto-sent on Stopped"
        );
    }

    #[gpui::test]
    async fn test_queued_message_steer_defaults_off_and_toggles(cx: &mut TestAppContext) {
        init_test(cx);

        let (conversation_view, cx) =
            setup_conversation_view(StubAgentServer::default_response(), cx).await;
        add_to_workspace(conversation_view.clone(), cx);

        let id = active_thread(&conversation_view, cx).update_in(cx, |thread, window, cx| {
            thread.add_to_queue(
                vec![acp::ContentBlock::Text(acp::TextContent::new(
                    "queued".to_string(),
                ))],
                vec![],
                window,
                cx,
            );
            thread.message_queue.first_id().unwrap()
        });
        cx.run_until_parked();

        // Default: steering is off, so the message waits for end-of-generation
        // rather than interrupting the agent at the next boundary.
        active_thread(&conversation_view, cx).read_with(cx, |thread, _cx| {
            assert!(
                !thread.message_queue.front_wants_steer(),
                "steering should default off"
            );
        });

        active_thread(&conversation_view, cx).update(cx, |thread, _cx| {
            thread.message_queue.toggle_steer(id);
        });
        active_thread(&conversation_view, cx).read_with(cx, |thread, _cx| {
            assert!(
                thread.message_queue.front_wants_steer(),
                "steering should be on after toggling"
            );
        });
    }

    #[gpui::test]
    async fn test_queue_resumes_after_stop_and_new_message(cx: &mut TestAppContext) {
        init_test(cx);

        let connection = StubAgentConnection::new();
        let (conversation_view, cx) =
            setup_conversation_view(StubAgentServer::new(connection.clone()), cx).await;
        add_to_workspace(conversation_view.clone(), cx);

        let message_editor = message_editor(&conversation_view, cx);
        message_editor.update_in(cx, |editor, window, cx| {
            editor.set_text("first", window, cx);
        });
        active_thread(&conversation_view, cx)
            .update_in(cx, |view, window, cx| view.send(window, cx));
        cx.run_until_parked();

        // Queue a follow-up while the agent is generating.
        active_thread(&conversation_view, cx).update_in(cx, |thread, window, cx| {
            thread.add_to_queue(
                vec![acp::ContentBlock::Text(acp::TextContent::new(
                    "queued".to_string(),
                ))],
                vec![],
                window,
                cx,
            );
        });

        // User stops generation: the queued message must NOT be sent.
        active_thread(&conversation_view, cx)
            .update_in(cx, |thread, _window, cx| thread.cancel_generation(cx));
        cx.run_until_parked();

        let queue_len = active_thread(&conversation_view, cx)
            .read_with(cx, |thread, _cx| thread.message_queue.len());
        assert_eq!(queue_len, 1, "stopping must not send the queued message");

        // User sends a new message, which should resume queue auto-processing.
        message_editor.update_in(cx, |editor, window, cx| {
            editor.set_text("second", window, cx);
        });
        active_thread(&conversation_view, cx)
            .update_in(cx, |view, window, cx| view.send(window, cx));
        cx.run_until_parked();

        let session_id = conversation_view.read_with(cx, |view, cx| {
            view.active_thread()
                .unwrap()
                .read(cx)
                .thread
                .read(cx)
                .session_id()
                .clone()
        });

        // When this generation completes, the queued message should be picked
        // up automatically (regression test for the "frozen queue" bug).
        connection.end_turn(session_id, acp::StopReason::EndTurn);
        cx.run_until_parked();

        let queue_len = active_thread(&conversation_view, cx)
            .read_with(cx, |thread, _cx| thread.message_queue.len());
        assert_eq!(
            queue_len, 0,
            "queued message should be auto-sent after the user re-engages"
        );
    }

    #[gpui::test]
    async fn test_notification_for_error(cx: &mut TestAppContext) {
        init_test(cx);

        let server = FakeAcpAgentServer::new();
        let (conversation_view, cx) = setup_conversation_view(server.clone(), cx).await;

        let message_editor = message_editor(&conversation_view, cx);
        message_editor.update_in(cx, |editor, window, cx| {
            editor.set_text("Hello", window, cx);
        });

        cx.deactivate_window();
        server.fail_next_prompt();

        active_thread(&conversation_view, cx)
            .update_in(cx, |view, window, cx| view.send(window, cx));

        cx.run_until_parked();

        assert!(
            cx.windows()
                .iter()
                .any(|window| window.downcast::<AgentNotification>().is_some())
        );
    }

    #[gpui::test]
    async fn test_acp_server_exit_transitions_conversation_to_load_error_without_panic(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);

        let server = FakeAcpAgentServer::new();
        let close_session_count = server.close_session_count();
        let (conversation_view, cx) = setup_conversation_view(server.clone(), cx).await;

        cx.run_until_parked();

        server.simulate_server_exit();
        cx.run_until_parked();

        conversation_view.read_with(cx, |view, _cx| {
            assert!(
                matches!(view.server_state, ServerState::LoadError { .. }),
                "Conversation should transition to LoadError when an ACP thread exits"
            );
        });
        assert_eq!(
            close_session_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "ConversationView should close the ACP session after a thread exit"
        );
    }

    #[gpui::test]
    async fn test_thread_view_seeds_existing_elicitation_form_state(cx: &mut TestAppContext) {
        init_test(cx);
        cx.update(|cx| {
            cx.update_flags(true, vec![AcpBetaFeatureFlag::NAME.to_string()]);
        });

        let connection = PreloadedElicitationConnection::default();
        let elicitation_id = connection.elicitation_id.clone();
        let (conversation_view, cx) =
            setup_conversation_view(StubAgentServer::new(connection), cx).await;

        let elicitation_id = elicitation_id
            .lock()
            .clone()
            .expect("connection should preload an elicitation");
        let active_thread = active_thread(&conversation_view, cx);
        active_thread.read_with(cx, |thread, _cx| {
            assert!(
                thread.has_elicitation_form_state(&elicitation_id),
                "pending form elicitations that predate ThreadView construction should be usable"
            );
        });
    }

    #[gpui::test]
    async fn test_resume_without_history_adds_notice(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        let thread_store = cx.update(|_window, cx| cx.new(|cx| ThreadStore::new(cx)));
        let connection_store =
            cx.update(|_window, cx| cx.new(|cx| AgentConnectionStore::new(project.clone(), cx)));

        let conversation_view = cx.update(|window, cx| {
            cx.new(|cx| {
                ConversationView::new(
                    Rc::new(StubAgentServer::new(ResumeOnlyAgentConnection)),
                    connection_store,
                    Agent::Custom { id: "Test".into() },
                    Some(acp::SessionId::new("resume-session")),
                    None,
                    None,
                    None,
                    None,
                    workspace.downgrade(),
                    project,
                    Some(thread_store),
                    AgentThreadSource::AgentPanel,
                    window,
                    cx,
                )
            })
        });

        cx.run_until_parked();

        conversation_view.read_with(cx, |view, cx| {
            let state = view.active_thread().unwrap();
            assert!(state.read(cx).resumed_without_history);
            assert_eq!(state.read(cx).list_state.item_count(), 0);
        });
    }

    #[derive(Clone)]
    struct RestoredAvailableCommandsConnection;

    impl AgentConnection for RestoredAvailableCommandsConnection {
        fn agent_id(&self) -> AgentId {
            AgentId::new("restored-available-commands")
        }

        fn telemetry_id(&self) -> SharedString {
            "restored-available-commands".into()
        }

        fn new_session(
            self: Rc<Self>,
            project: Entity<Project>,
            _work_dirs: PathList,
            cx: &mut App,
        ) -> Task<gpui::Result<Entity<AcpThread>>> {
            let thread = build_test_thread(
                self,
                project,
                "RestoredAvailableCommandsConnection",
                acp::SessionId::new("new-session"),
                cx,
            );
            Task::ready(Ok(thread))
        }

        fn supports_load_session(&self) -> bool {
            true
        }

        fn load_session(
            self: Rc<Self>,
            session_id: acp::SessionId,
            project: Entity<Project>,
            _work_dirs: PathList,
            _title: Option<SharedString>,
            cx: &mut App,
        ) -> Task<gpui::Result<Entity<AcpThread>>> {
            let thread = build_test_thread(
                self,
                project,
                "RestoredAvailableCommandsConnection",
                session_id,
                cx,
            );

            thread
                .update(cx, |thread, cx| {
                    thread.handle_session_update(
                        acp::SessionUpdate::AvailableCommandsUpdate(
                            acp::AvailableCommandsUpdate::new(vec![acp::AvailableCommand::new(
                                "help", "Get help",
                            )]),
                        ),
                        cx,
                    )
                })
                .expect("available commands update should succeed");

            Task::ready(Ok(thread))
        }

        fn auth_methods(&self) -> &[acp::AuthMethod] {
            &[]
        }

        fn authenticate(
            &self,
            _method_id: acp::AuthMethodId,
            _cx: &mut App,
        ) -> Task<gpui::Result<()>> {
            Task::ready(Ok(()))
        }

        fn prompt(
            &self,
            _params: acp::PromptRequest,
            _cx: &mut App,
        ) -> Task<gpui::Result<acp::PromptResponse>> {
            Task::ready(Ok(acp::PromptResponse::new(acp::StopReason::EndTurn)))
        }

        fn cancel(&self, _session_id: &acp::SessionId, _cx: &mut App) {}

        fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
            self
        }
    }

    #[gpui::test]
    async fn test_restored_threads_keep_available_commands(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        let thread_store = cx.update(|_window, cx| cx.new(|cx| ThreadStore::new(cx)));
        let connection_store =
            cx.update(|_window, cx| cx.new(|cx| AgentConnectionStore::new(project.clone(), cx)));

        let conversation_view = cx.update(|window, cx| {
            cx.new(|cx| {
                ConversationView::new(
                    Rc::new(StubAgentServer::new(RestoredAvailableCommandsConnection)),
                    connection_store,
                    Agent::Custom { id: "Test".into() },
                    Some(acp::SessionId::new("restored-session")),
                    None,
                    None,
                    None,
                    None,
                    workspace.downgrade(),
                    project,
                    Some(thread_store),
                    AgentThreadSource::AgentPanel,
                    window,
                    cx,
                )
            })
        });

        cx.run_until_parked();

        let message_editor = message_editor(&conversation_view, cx);
        let editor =
            message_editor.update(cx, |message_editor, _cx| message_editor.editor().clone());
        let placeholder = editor.update(cx, |editor, cx| editor.placeholder_text(cx));

        active_thread(&conversation_view, cx).read_with(cx, |view, _cx| {
            let available_commands = view
                .session_capabilities
                .read()
                .available_commands()
                .to_vec();
            assert_eq!(available_commands.len(), 1);
            assert_eq!(available_commands[0].name.as_str(), "help");
            assert_eq!(available_commands[0].description.as_str(), "Get help");
        });

        assert_eq!(
            placeholder,
            Some("Test にメッセージを送信 — @ でコンテキストを追加、/ でコマンド".to_string())
        );

        message_editor.update_in(cx, |editor, window, cx| {
            editor.set_text("/help", window, cx);
        });

        let contents_result = message_editor
            .update(cx, |editor, cx| editor.contents(false, cx))
            .await;

        assert!(contents_result.is_ok());
    }

    #[gpui::test]
    async fn test_resume_thread_uses_session_cwd_when_inside_project(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/project",
            json!({
                "subdir": {
                    "file.txt": "hello"
                }
            }),
        )
        .await;
        let project = Project::test(fs, [Path::new("/project")], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        let connection = CwdCapturingConnection::new();
        let captured_cwd = connection.captured_work_dirs.clone();

        let thread_store = cx.update(|_window, cx| cx.new(|cx| ThreadStore::new(cx)));
        let connection_store =
            cx.update(|_window, cx| cx.new(|cx| AgentConnectionStore::new(project.clone(), cx)));

        let _conversation_view = cx.update(|window, cx| {
            cx.new(|cx| {
                ConversationView::new(
                    Rc::new(StubAgentServer::new(connection)),
                    connection_store,
                    Agent::Custom { id: "Test".into() },
                    Some(acp::SessionId::new("session-1")),
                    None,
                    Some(PathList::new(&[PathBuf::from("/project/subdir")])),
                    None,
                    None,
                    workspace.downgrade(),
                    project,
                    Some(thread_store),
                    AgentThreadSource::AgentPanel,
                    window,
                    cx,
                )
            })
        });

        cx.run_until_parked();

        assert_eq!(
            captured_cwd.lock().as_ref().unwrap(),
            &PathList::new(&[Path::new("/project/subdir")]),
            "Should use session cwd when it's inside the project"
        );
    }

    #[gpui::test]
    async fn test_refusal_handling(cx: &mut TestAppContext) {
        init_test(cx);

        let (conversation_view, cx) =
            setup_conversation_view(StubAgentServer::new(RefusalAgentConnection), cx).await;

        let message_editor = message_editor(&conversation_view, cx);
        message_editor.update_in(cx, |editor, window, cx| {
            editor.set_text("Do something harmful", window, cx);
        });

        active_thread(&conversation_view, cx)
            .update_in(cx, |view, window, cx| view.send(window, cx));

        cx.run_until_parked();

        // Check that the refusal error is set
        conversation_view.read_with(cx, |thread_view, cx| {
            let state = thread_view.active_thread().unwrap();
            assert!(
                matches!(state.read(cx).thread_error, Some(ThreadError::Refusal)),
                "Expected refusal error to be set"
            );
        });
    }

    #[gpui::test]
    async fn test_connect_failure_transitions_to_load_error(cx: &mut TestAppContext) {
        init_test(cx);

        let (conversation_view, cx) = setup_conversation_view(FailingAgentServer, cx).await;

        conversation_view.read_with(cx, |view, cx| {
            let title = view.title(cx);
            assert_eq!(
                title.as_ref(),
                "Codex CLI の読み込みエラー",
                "Tab title should show the agent name with an error prefix"
            );
            match &view.server_state {
                ServerState::LoadError {
                    error: LoadError::Other(msg),
                    ..
                } => {
                    assert!(
                        msg.contains("Invalid gzip header"),
                        "Error callout should contain the underlying extraction error, got: {msg}"
                    );
                }
                other => panic!(
                    "Expected LoadError::Other, got: {}",
                    match other {
                        ServerState::Loading { .. } => "Loading (stuck!)",
                        ServerState::LoadError { .. } => "LoadError (wrong variant)",
                        ServerState::Connected(_) => "Connected",
                        ServerState::Parked(_) => "Parked",
                    }
                ),
            }
        });
    }

    #[gpui::test]
    async fn test_reset_preserves_session_id_after_load_error(cx: &mut TestAppContext) {
        use crate::thread_metadata_store::{ThreadId, ThreadMetadata};
        use chrono::Utc;
        use project::{AgentId as ProjectAgentId, WorktreePaths};
        use std::sync::atomic::Ordering;

        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        let thread_store = cx.update(|_window, cx| cx.new(|cx| ThreadStore::new(cx)));
        let connection_store =
            cx.update(|_window, cx| cx.new(|cx| AgentConnectionStore::new(project.clone(), cx)));

        // Simulate a previous run that persisted metadata for this session.
        let resume_session_id = acp::SessionId::new("persistent-session");
        let stored_title: SharedString = "Persistent chat".into();
        cx.update(|_window, cx| {
            ThreadMetadataStore::global(cx).update(cx, |store, cx| {
                store.save(
                    ThreadMetadata {
                        thread_id: ThreadId::new(),
                        session_id: Some(resume_session_id.clone()),
                        agent_id: ProjectAgentId::new("Flaky"),
                        title: Some(stored_title.clone()),
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

        let connection = StubAgentConnection::new().with_supports_load_session(true);
        let (server, fail) = FlakyAgentServer::new(connection);

        let conversation_view = cx.update(|window, cx| {
            cx.new(|cx| {
                ConversationView::new(
                    Rc::new(server),
                    connection_store,
                    Agent::Custom { id: "Flaky".into() },
                    Some(resume_session_id.clone()),
                    None,
                    None,
                    None,
                    None,
                    workspace.downgrade(),
                    project.clone(),
                    Some(thread_store),
                    AgentThreadSource::AgentPanel,
                    window,
                    cx,
                )
            })
        });
        cx.run_until_parked();

        // The first connect() fails, so we land in LoadError.
        conversation_view.read_with(cx, |view, _cx| {
            assert!(
                matches!(view.server_state, ServerState::LoadError { .. }),
                "expected LoadError after failed initial connect"
            );
            assert_eq!(
                view.root_session_id.as_ref(),
                Some(&resume_session_id),
                "root_session_id should still hold the original id while in LoadError"
            );
        });

        // Now let the agent come online and emit AgentServersUpdated. This is
        // the moment the bug would have stomped on root_session_id.
        fail.store(false, Ordering::SeqCst);
        project.update(cx, |project, cx| {
            project
                .agent_server_store()
                .update(cx, |_store, cx| cx.emit(project::AgentServersUpdated));
        });
        cx.run_until_parked();

        // The retry should have resumed the ORIGINAL session, not created a
        // brand-new one.
        conversation_view.read_with(cx, |view, cx| {
            let connected = view
                .as_connected()
                .expect("should be Connected after flaky server comes online");
            let active_id = connected
                .active_id
                .as_ref()
                .expect("Connected state should have an active_id");
            assert_eq!(
                active_id, &resume_session_id,
                "reset() must resume the original session id, not call new_session()"
            );
            let active_thread = view
                .active_thread()
                .expect("should have an active thread view");
            let thread_session = active_thread.read(cx).thread.read(cx).session_id().clone();
            assert_eq!(
                thread_session, resume_session_id,
                "the live AcpThread should hold the resumed session id"
            );
        });
    }

    #[gpui::test]
    async fn test_auth_required_on_initial_connect(cx: &mut TestAppContext) {
        init_test(cx);

        let connection = AuthGatedAgentConnection::new();
        let (conversation_view, cx) =
            setup_conversation_view(StubAgentServer::new(connection), cx).await;

        // When new_session returns AuthRequired, the server should transition
        // to Connected + Unauthenticated rather than getting stuck in Loading.
        conversation_view.read_with(cx, |view, _cx| {
            let connected = view
                .as_connected()
                .expect("Should be in Connected state even though auth is required");
            assert!(
                !connected.auth_state.is_ok(),
                "Auth state should be Unauthenticated"
            );
            assert!(
                !view.supports_logout(),
                "Logout should be hidden while unauthenticated"
            );
            assert!(
                connected.active_id.is_none(),
                "There should be no active thread since no session was created"
            );
            assert!(
                !view.active_thread_renders_request_elicitations(),
                "request elicitations should render outside ThreadView when no thread exists"
            );
            assert!(
                connected.threads.is_empty(),
                "There should be no threads since no session was created"
            );
        });

        conversation_view.read_with(cx, |view, _cx| {
            assert!(
                view.active_thread().is_none(),
                "active_thread() should be None when unauthenticated without a session"
            );
        });

        // Authenticate using the real authenticate flow on ConnectionView.
        // This calls connection.authenticate(), which flips the internal flag,
        // then on success triggers reset() -> new_session() which now succeeds.
        conversation_view.update_in(cx, |view, window, cx| {
            view.authenticate(
                acp::AuthMethodId::new(AuthGatedAgentConnection::AUTH_METHOD_ID),
                window,
                cx,
            );
        });
        cx.run_until_parked();

        // After auth, the server should have an active thread in the Ok state.
        conversation_view.read_with(cx, |view, cx| {
            let connected = view
                .as_connected()
                .expect("Should still be in Connected state after auth");
            assert!(connected.auth_state.is_ok(), "Auth state should be Ok");
            assert!(
                view.supports_logout(),
                "Logout should be available after authentication"
            );
            assert!(
                connected.active_id.is_some(),
                "There should be an active thread after successful auth"
            );
            assert!(
                view.active_thread_renders_request_elicitations(),
                "request elicitations should render inside ThreadView while authenticated"
            );
            assert_eq!(
                connected.threads.len(),
                1,
                "There should be exactly one thread"
            );

            let active = view
                .active_thread()
                .expect("active_thread() should return the new thread");
            assert!(
                active.read(cx).thread_error.is_none(),
                "The new thread should have no errors"
            );
        });

        conversation_view.update_in(cx, |view, window, cx| view.logout(window, cx));
        cx.run_until_parked();

        conversation_view.read_with(cx, |view, _cx| {
            let connected = view
                .as_connected()
                .expect("Should still be in Connected state after logout");
            assert!(
                !connected.auth_state.is_ok(),
                "Auth state should be Unauthenticated after logout"
            );
            assert!(
                !view.supports_logout(),
                "Logout should be hidden after logout"
            );
            assert!(
                view.active_thread().is_some(),
                "The existing thread should still exist after logout"
            );
            assert!(
                !view.active_thread_renders_request_elicitations(),
                "Unauthenticated auth UI should render request elicitations outside ThreadView"
            );
        });
    }

    #[gpui::test]
    async fn test_notification_for_tool_authorization(cx: &mut TestAppContext) {
        init_test(cx);

        let tool_call_id = acp::ToolCallId::new("1");
        let tool_call = acp::ToolCall::new(tool_call_id.clone(), "Label")
            .kind(acp::ToolKind::Edit)
            .content(vec!["hi".into()]);
        let connection =
            StubAgentConnection::new().with_permission_requests(HashMap::from_iter([(
                tool_call_id,
                PermissionOptions::Flat(vec![acp::PermissionOption::new(
                    "1",
                    "Allow",
                    acp::PermissionOptionKind::AllowOnce,
                )]),
            )]));

        connection.set_next_prompt_updates(vec![acp::SessionUpdate::ToolCall(tool_call)]);

        let (conversation_view, cx) =
            setup_conversation_view(StubAgentServer::new(connection), cx).await;

        let message_editor = message_editor(&conversation_view, cx);
        message_editor.update_in(cx, |editor, window, cx| {
            editor.set_text("Hello", window, cx);
        });

        cx.deactivate_window();

        active_thread(&conversation_view, cx)
            .update_in(cx, |view, window, cx| view.send(window, cx));

        cx.run_until_parked();

        assert!(
            cx.windows()
                .iter()
                .any(|window| window.downcast::<AgentNotification>().is_some())
        );
    }

    #[gpui::test]
    async fn test_notification_when_panel_hidden(cx: &mut TestAppContext) {
        init_test(cx);

        let (conversation_view, cx) =
            setup_conversation_view(StubAgentServer::default_response(), cx).await;

        add_to_workspace(conversation_view.clone(), cx);

        let message_editor = message_editor(&conversation_view, cx);

        message_editor.update_in(cx, |editor, window, cx| {
            editor.set_text("Hello", window, cx);
        });

        // Window is active (don't deactivate), but panel will be hidden
        // Note: In the test environment, the panel is not actually added to the dock,
        // so is_agent_panel_hidden will return true

        active_thread(&conversation_view, cx)
            .update_in(cx, |view, window, cx| view.send(window, cx));

        cx.run_until_parked();

        // Should show notification because window is active but panel is hidden
        assert!(
            cx.windows()
                .iter()
                .any(|window| window.downcast::<AgentNotification>().is_some()),
            "Expected notification when panel is hidden"
        );
    }

    #[gpui::test]
    async fn test_notification_still_works_when_window_inactive(cx: &mut TestAppContext) {
        init_test(cx);

        let (conversation_view, cx) =
            setup_conversation_view(StubAgentServer::default_response(), cx).await;

        let message_editor = message_editor(&conversation_view, cx);
        message_editor.update_in(cx, |editor, window, cx| {
            editor.set_text("Hello", window, cx);
        });

        // Deactivate window - should show notification regardless of setting
        cx.deactivate_window();

        active_thread(&conversation_view, cx)
            .update_in(cx, |view, window, cx| view.send(window, cx));

        cx.run_until_parked();

        // Should still show notification when window is inactive (existing behavior)
        assert!(
            cx.windows()
                .iter()
                .any(|window| window.downcast::<AgentNotification>().is_some()),
            "Expected notification when window is inactive"
        );
    }

    #[gpui::test]
    async fn test_notification_when_different_conversation_is_active_in_visible_panel(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());

        cx.update(|cx| {
            cx.update_flags(true, vec!["agent-v2".to_string()]);
            agent::ThreadStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);
            <dyn Fs>::set_global(fs.clone(), cx);
        });

        let project = Project::test(fs, [], cx).await;
        let multi_workspace_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));

        let workspace = multi_workspace_handle
            .read_with(cx, |mw, _cx| mw.workspace().clone())
            .unwrap();

        let cx = &mut VisualTestContext::from_window(multi_workspace_handle.into(), cx);

        let panel = workspace.update_in(cx, |workspace, window, cx| {
            let panel = cx.new(|cx| crate::AgentPanel::new(workspace, window, cx));
            workspace.add_panel(panel.clone(), window, cx);
            workspace.focus_panel::<crate::AgentPanel>(window, cx);
            panel
        });

        cx.run_until_parked();

        panel.update_in(cx, |panel, window, cx| {
            panel.open_external_thread_with_server(
                Rc::new(StubAgentServer::default_response()),
                window,
                cx,
            );
        });

        cx.run_until_parked();

        panel.read_with(cx, |panel, cx| {
            assert!(crate::AgentPanel::is_visible(&workspace, cx));
            assert!(panel.active_conversation_view().is_some());
        });

        let thread_store = cx.update(|_window, cx| cx.new(|cx| ThreadStore::new(cx)));
        let connection_store =
            cx.update(|_window, cx| cx.new(|cx| AgentConnectionStore::new(project.clone(), cx)));

        let conversation_view = cx.update(|window, cx| {
            cx.new(|cx| {
                ConversationView::new(
                    Rc::new(StubAgentServer::default_response()),
                    connection_store,
                    Agent::Custom { id: "Test".into() },
                    None,
                    None,
                    None,
                    None,
                    None,
                    workspace.downgrade(),
                    project.clone(),
                    Some(thread_store),
                    AgentThreadSource::AgentPanel,
                    window,
                    cx,
                )
            })
        });

        cx.run_until_parked();

        panel.read_with(cx, |panel, _cx| {
            assert_ne!(
                panel
                    .active_conversation_view()
                    .map(|view| view.entity_id()),
                Some(conversation_view.entity_id()),
                "The visible panel should still be showing a different conversation"
            );
        });

        let message_editor = message_editor(&conversation_view, cx);
        message_editor.update_in(cx, |editor, window, cx| {
            editor.set_text("Hello", window, cx);
        });

        active_thread(&conversation_view, cx)
            .update_in(cx, |view, window, cx| view.send(window, cx));

        cx.run_until_parked();

        assert!(
            cx.windows()
                .iter()
                .any(|window| window.downcast::<AgentNotification>().is_some()),
            "Expected notification when a different conversation is active in the visible panel"
        );
    }

    #[gpui::test]
    async fn test_no_notification_when_sidebar_open_but_different_thread_focused(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());

        cx.update(|cx| {
            cx.update_flags(true, vec!["agent-v2".to_string()]);
            agent::ThreadStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);
            <dyn Fs>::set_global(fs.clone(), cx);
        });

        let project = Project::test(fs, [], cx).await;
        let multi_workspace_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));

        let workspace = multi_workspace_handle
            .read_with(cx, |mw, _cx| mw.workspace().clone())
            .unwrap();

        let cx = &mut VisualTestContext::from_window(multi_workspace_handle.into(), cx);
        register_test_sidebar(true, cx);

        // Open the sidebar so that sidebar_open() returns true.
        multi_workspace_handle
            .update(cx, |mw, _window, cx| {
                mw.open_sidebar(cx);
            })
            .unwrap();

        cx.run_until_parked();

        assert!(
            multi_workspace_handle
                .read_with(cx, |mw, _cx| mw.sidebar_open())
                .unwrap(),
            "Sidebar should be open"
        );

        // Create a conversation view that is NOT the active one in the panel.
        let thread_store = cx.update(|_window, cx| cx.new(|cx| ThreadStore::new(cx)));
        let connection_store =
            cx.update(|_window, cx| cx.new(|cx| AgentConnectionStore::new(project.clone(), cx)));

        let conversation_view = cx.update(|window, cx| {
            cx.new(|cx| {
                ConversationView::new(
                    Rc::new(StubAgentServer::default_response()),
                    connection_store,
                    Agent::Custom { id: "Test".into() },
                    None,
                    None,
                    None,
                    None,
                    None,
                    workspace.downgrade(),
                    project.clone(),
                    Some(thread_store),
                    AgentThreadSource::AgentPanel,
                    window,
                    cx,
                )
            })
        });

        cx.run_until_parked();

        let message_editor = message_editor(&conversation_view, cx);
        message_editor.update_in(cx, |editor, window, cx| {
            editor.set_text("Hello", window, cx);
        });

        active_thread(&conversation_view, cx)
            .update_in(cx, |view, window, cx| view.send(window, cx));

        cx.run_until_parked();

        assert!(
            !cx.windows()
                .iter()
                .any(|window| window.downcast::<AgentNotification>().is_some()),
            "Expected no notification when the sidebar is open, even if focused on another thread"
        );
    }

    #[gpui::test]
    async fn test_notification_when_sidebar_open_but_thread_list_hidden(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());

        cx.update(|cx| {
            cx.update_flags(true, vec!["agent-v2".to_string()]);
            agent::ThreadStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);
            <dyn Fs>::set_global(fs.clone(), cx);
        });

        let project = Project::test(fs, [], cx).await;
        let multi_workspace_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));

        let workspace = multi_workspace_handle
            .read_with(cx, |mw, _cx| mw.workspace().clone())
            .unwrap();

        let cx = &mut VisualTestContext::from_window(multi_workspace_handle.into(), cx);
        register_test_sidebar(false, cx);
        multi_workspace_handle
            .update(cx, |mw, _window, cx| {
                mw.open_sidebar(cx);
            })
            .unwrap();
        cx.run_until_parked();

        let thread_store = cx.update(|_window, cx| cx.new(|cx| ThreadStore::new(cx)));
        let connection_store =
            cx.update(|_window, cx| cx.new(|cx| AgentConnectionStore::new(project.clone(), cx)));

        let conversation_view = cx.update(|window, cx| {
            cx.new(|cx| {
                ConversationView::new(
                    Rc::new(StubAgentServer::default_response()),
                    connection_store,
                    Agent::Custom { id: "Test".into() },
                    None,
                    None,
                    None,
                    None,
                    None,
                    workspace.downgrade(),
                    project.clone(),
                    Some(thread_store),
                    AgentThreadSource::AgentPanel,
                    window,
                    cx,
                )
            })
        });
        cx.run_until_parked();

        let message_editor = message_editor(&conversation_view, cx);
        message_editor.update_in(cx, |editor, window, cx| {
            editor.set_text("Hello", window, cx);
        });

        active_thread(&conversation_view, cx)
            .update_in(cx, |view, window, cx| view.send(window, cx));
        cx.run_until_parked();

        assert!(
            cx.windows()
                .iter()
                .any(|window| window.downcast::<AgentNotification>().is_some()),
            "Expected notification when the sidebar is open but the thread list is hidden"
        );
    }

    #[gpui::test]
    async fn test_notification_dismissed_when_sidebar_opens(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());

        cx.update(|cx| {
            cx.update_flags(true, vec!["agent-v2".to_string()]);
            agent::ThreadStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);
            <dyn Fs>::set_global(fs.clone(), cx);
        });

        let project = Project::test(fs, [], cx).await;
        let multi_workspace_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));

        let workspace = multi_workspace_handle
            .read_with(cx, |mw, _cx| mw.workspace().clone())
            .unwrap();

        let cx = &mut VisualTestContext::from_window(multi_workspace_handle.into(), cx);
        register_test_sidebar(true, cx);

        let thread_store = cx.update(|_window, cx| cx.new(|cx| ThreadStore::new(cx)));
        let connection_store =
            cx.update(|_window, cx| cx.new(|cx| AgentConnectionStore::new(project.clone(), cx)));

        let conversation_view = cx.update(|window, cx| {
            cx.new(|cx| {
                ConversationView::new(
                    Rc::new(StubAgentServer::default_response()),
                    connection_store,
                    Agent::Custom { id: "Test".into() },
                    None,
                    None,
                    None,
                    None,
                    None,
                    workspace.downgrade(),
                    project.clone(),
                    Some(thread_store),
                    AgentThreadSource::AgentPanel,
                    window,
                    cx,
                )
            })
        });

        cx.run_until_parked();

        let message_editor = message_editor(&conversation_view, cx);
        message_editor.update_in(cx, |editor, window, cx| {
            editor.set_text("Hello", window, cx);
        });

        active_thread(&conversation_view, cx)
            .update_in(cx, |view, window, cx| view.send(window, cx));

        cx.run_until_parked();

        assert_eq!(
            cx.windows()
                .iter()
                .filter(|window| window.downcast::<AgentNotification>().is_some())
                .count(),
            1,
            "Expected a notification while the thread is not visible"
        );

        multi_workspace_handle
            .update(cx, |mw, _window, cx| {
                mw.open_sidebar(cx);
            })
            .unwrap();

        cx.run_until_parked();

        assert_eq!(
            cx.windows()
                .iter()
                .filter(|window| window.downcast::<AgentNotification>().is_some())
                .count(),
            0,
            "Notification should auto-dismiss when the sidebar opens and makes the thread visible"
        );
    }

    #[gpui::test]
    async fn test_notification_when_workspace_is_background_in_multi_workspace(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);

        // Enable multi-workspace feature flag and init globals needed by AgentPanel
        let fs = FakeFs::new(cx.executor());

        cx.update(|cx| {
            agent::ThreadStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);
            <dyn Fs>::set_global(fs.clone(), cx);
        });

        let project1 = Project::test(fs.clone(), [], cx).await;

        // Create a MultiWorkspace window with one workspace
        let multi_workspace_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project1.clone(), window, cx));

        // Get workspace 1 (the initial workspace)
        let workspace1 = multi_workspace_handle
            .read_with(cx, |mw, _cx| mw.workspace().clone())
            .unwrap();

        let cx = &mut VisualTestContext::from_window(multi_workspace_handle.into(), cx);

        let panel = workspace1.update_in(cx, |workspace, window, cx| {
            let panel = cx.new(|cx| crate::AgentPanel::new(workspace, window, cx));
            workspace.add_panel(panel.clone(), window, cx);

            // Open the dock and activate the agent panel so it's visible
            workspace.focus_panel::<crate::AgentPanel>(window, cx);
            panel
        });

        cx.run_until_parked();

        panel.update_in(cx, |panel, window, cx| {
            panel.open_external_thread_with_server(
                Rc::new(StubAgentServer::new(RestoredAvailableCommandsConnection)),
                window,
                cx,
            );
        });

        cx.run_until_parked();

        cx.read(|cx| {
            assert!(
                crate::AgentPanel::is_visible(&workspace1, cx),
                "AgentPanel should be visible in workspace1's dock"
            );
        });

        // Set up thread view in workspace 1
        let thread_store = cx.update(|_window, cx| cx.new(|cx| ThreadStore::new(cx)));
        let connection_store =
            cx.update(|_window, cx| cx.new(|cx| AgentConnectionStore::new(project1.clone(), cx)));

        let conversation_view = cx.update(|window, cx| {
            cx.new(|cx| {
                ConversationView::new(
                    Rc::new(StubAgentServer::new(RestoredAvailableCommandsConnection)),
                    connection_store,
                    Agent::Custom { id: "Test".into() },
                    None,
                    None,
                    None,
                    None,
                    None,
                    workspace1.downgrade(),
                    project1.clone(),
                    Some(thread_store),
                    AgentThreadSource::AgentPanel,
                    window,
                    cx,
                )
            })
        });
        cx.run_until_parked();

        let root_session_id = conversation_view
            .read_with(cx, |view, cx| {
                view.root_thread_view()
                    .map(|thread| thread.read(cx).thread.read(cx).session_id().clone())
            })
            .expect("Conversation view should have a root thread");

        let message_editor = message_editor(&conversation_view, cx);
        message_editor.update_in(cx, |editor, window, cx| {
            editor.set_text("Hello", window, cx);
        });

        // Create a second workspace and switch to it.
        // This makes workspace1 the "background" workspace.
        let project2 = Project::test(fs, [], cx).await;
        multi_workspace_handle
            .update(cx, |mw, window, cx| {
                mw.test_add_workspace(project2, window, cx);
            })
            .unwrap();

        cx.run_until_parked();

        // Verify workspace1 is no longer the active workspace
        multi_workspace_handle
            .read_with(cx, |mw, _cx| {
                assert_ne!(mw.workspace(), &workspace1);
            })
            .unwrap();

        // Window is active, agent panel is visible in workspace1, but workspace1
        // is in the background. The notification should show because the user
        // can't actually see the agent panel.
        active_thread(&conversation_view, cx)
            .update_in(cx, |view, window, cx| view.send(window, cx));

        cx.run_until_parked();

        assert!(
            cx.windows()
                .iter()
                .any(|window| window.downcast::<AgentNotification>().is_some()),
            "Expected notification when workspace is in background within MultiWorkspace"
        );

        // Also verify: clicking "View Panel" should switch to workspace1.
        cx.windows()
            .iter()
            .find_map(|window| window.downcast::<AgentNotification>())
            .unwrap()
            .update(cx, |window, _, cx| window.accept(cx))
            .unwrap();

        cx.run_until_parked();

        multi_workspace_handle
            .read_with(cx, |mw, _cx| {
                assert_eq!(
                    mw.workspace(),
                    &workspace1,
                    "Expected workspace1 to become the active workspace after accepting notification"
                );
            })
            .unwrap();

        panel.read_with(cx, |panel, cx| {
            let active_session_id = panel
                .active_agent_thread(cx)
                .map(|thread| thread.read(cx).session_id().clone());
            assert_eq!(
                active_session_id,
                Some(root_session_id),
                "Expected accepting the notification to load the notified thread in AgentPanel"
            );
        });
    }

    #[gpui::test]
    async fn test_notification_respects_never_setting(cx: &mut TestAppContext) {
        init_test(cx);

        // Set notify_when_agent_waiting to Never
        cx.update(|cx| {
            AgentSettings::override_global(
                AgentSettings {
                    notify_when_agent_waiting: NotifyWhenAgentWaiting::Never,
                    ..AgentSettings::get_global(cx).clone()
                },
                cx,
            );
        });

        let (conversation_view, cx) =
            setup_conversation_view(StubAgentServer::default_response(), cx).await;

        let message_editor = message_editor(&conversation_view, cx);
        message_editor.update_in(cx, |editor, window, cx| {
            editor.set_text("Hello", window, cx);
        });

        // Window is active

        active_thread(&conversation_view, cx)
            .update_in(cx, |view, window, cx| view.send(window, cx));

        cx.run_until_parked();

        // Should NOT show notification because notify_when_agent_waiting is Never
        assert!(
            !cx.windows()
                .iter()
                .any(|window| window.downcast::<AgentNotification>().is_some()),
            "Expected no notification when notify_when_agent_waiting is Never"
        );
    }

    #[gpui::test]
    async fn test_notification_closed_when_thread_view_dropped(cx: &mut TestAppContext) {
        init_test(cx);

        let (conversation_view, cx) =
            setup_conversation_view(StubAgentServer::default_response(), cx).await;

        let weak_view = conversation_view.downgrade();

        let message_editor = message_editor(&conversation_view, cx);
        message_editor.update_in(cx, |editor, window, cx| {
            editor.set_text("Hello", window, cx);
        });

        cx.deactivate_window();

        active_thread(&conversation_view, cx)
            .update_in(cx, |view, window, cx| view.send(window, cx));

        cx.run_until_parked();

        // Verify notification is shown
        assert!(
            cx.windows()
                .iter()
                .any(|window| window.downcast::<AgentNotification>().is_some()),
            "Expected notification to be shown"
        );

        // Drop the thread view (simulating navigation to a new thread)
        drop(conversation_view);
        drop(message_editor);
        // Trigger an update to flush effects, which will call release_dropped_entities
        cx.update(|_window, _cx| {});
        cx.run_until_parked();

        // Verify the entity was actually released
        assert!(
            !weak_view.is_upgradable(),
            "Thread view entity should be released after dropping"
        );

        // The notification should be automatically closed via on_release
        assert!(
            !cx.windows()
                .iter()
                .any(|window| window.downcast::<AgentNotification>().is_some()),
            "Notification should be closed when thread view is dropped"
        );
    }

    async fn setup_conversation_view(
        agent: impl AgentServer + 'static,
        cx: &mut TestAppContext,
    ) -> (Entity<ConversationView>, &mut VisualTestContext) {
        setup_conversation_view_with_initial_content_opt(agent, None, cx).await
    }

    #[gpui::test]
    async fn test_completed_plan_snapshot_keeps_list_state_in_sync(cx: &mut TestAppContext) {
        init_test(cx);

        let connection = StubAgentConnection::new();
        let (conversation_view, cx) =
            setup_conversation_view(StubAgentServer::new(connection.clone()), cx).await;

        message_editor(&conversation_view, cx).update_in(cx, |editor, window, cx| {
            editor.set_text("Hello", window, cx);
        });
        active_thread(&conversation_view, cx).update_in(cx, |view, window, cx| {
            view.send(window, cx);
        });
        cx.run_until_parked();

        let session_id = active_thread(&conversation_view, cx).read_with(cx, |view, cx| {
            assert_thread_list_item_count_matches_entries(view, cx);
            view.thread.read(cx).session_id().clone()
        });

        cx.update(|_, cx| {
            connection.send_update(
                session_id.clone(),
                acp::SessionUpdate::Plan(acp::Plan::new(vec![acp::PlanEntry::new(
                    "Do the thing",
                    acp::PlanEntryPriority::Medium,
                    acp::PlanEntryStatus::InProgress,
                )])),
                cx,
            );
        });
        cx.run_until_parked();
        active_thread(&conversation_view, cx).read_with(cx, |view, cx| {
            assert_thread_list_item_count_matches_entries(view, cx);
        });

        cx.update(|_, cx| {
            connection.send_update(
                session_id.clone(),
                acp::SessionUpdate::Plan(acp::Plan::new(vec![acp::PlanEntry::new(
                    "Do the thing",
                    acp::PlanEntryPriority::Medium,
                    acp::PlanEntryStatus::Completed,
                )])),
                cx,
            );
        });
        cx.run_until_parked();
        active_thread(&conversation_view, cx).read_with(cx, |view, cx| {
            assert_thread_list_item_count_matches_entries(view, cx);
        });

        connection.end_turn(session_id, acp::StopReason::EndTurn);
        cx.run_until_parked();
        active_thread(&conversation_view, cx).read_with(cx, |view, cx| {
            assert_thread_list_item_count_matches_entries(view, cx);
        });
    }

    async fn setup_conversation_view_with_initial_content(
        agent: impl AgentServer + 'static,
        initial_content: AgentInitialContent,
        cx: &mut TestAppContext,
    ) -> (Entity<ConversationView>, &mut VisualTestContext) {
        setup_conversation_view_with_initial_content_opt(agent, Some(initial_content), cx).await
    }

    async fn setup_conversation_view_with_initial_content_opt(
        agent: impl AgentServer + 'static,
        initial_content: Option<AgentInitialContent>,
        cx: &mut TestAppContext,
    ) -> (Entity<ConversationView>, &mut VisualTestContext) {
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        let thread_store = cx.update(|_window, cx| cx.new(|cx| ThreadStore::new(cx)));
        let connection_store =
            cx.update(|_window, cx| cx.new(|cx| AgentConnectionStore::new(project.clone(), cx)));

        let agent_key = Agent::Custom { id: "Test".into() };

        let conversation_view = cx.update(|window, cx| {
            cx.new(|cx| {
                ConversationView::new(
                    Rc::new(agent),
                    connection_store.clone(),
                    agent_key.clone(),
                    None,
                    None,
                    None,
                    None,
                    initial_content,
                    workspace.downgrade(),
                    project,
                    Some(thread_store),
                    AgentThreadSource::AgentPanel,
                    window,
                    cx,
                )
            })
        });
        cx.run_until_parked();

        (conversation_view, cx)
    }

    fn add_to_workspace(conversation_view: Entity<ConversationView>, cx: &mut VisualTestContext) {
        let workspace =
            conversation_view.read_with(cx, |thread_view, _cx| thread_view.workspace.clone());

        workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.add_item_to_active_pane(
                    Box::new(cx.new(|_| ThreadViewItem(conversation_view.clone()))),
                    None,
                    true,
                    window,
                    cx,
                );
            })
            .unwrap();
    }

    struct ThreadViewItem(Entity<ConversationView>);

    impl Item for ThreadViewItem {
        type Event = ();

        fn include_in_nav_history() -> bool {
            false
        }

        fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
            "Test".into()
        }
    }

    impl EventEmitter<()> for ThreadViewItem {}

    impl Focusable for ThreadViewItem {
        fn focus_handle(&self, cx: &App) -> FocusHandle {
            self.0.read(cx).focus_handle(cx)
        }
    }

    impl Render for ThreadViewItem {
        fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            // Render the title editor in the element tree too. In the real app
            // it is part of the agent panel
            let title_editor = self
                .0
                .read(cx)
                .active_thread()
                .map(|t| t.read(cx).title_editor.clone());

            v_flex().children(title_editor).child(self.0.clone())
        }
    }

    pub(crate) struct StubAgentServer<C> {
        connection: C,
    }

    impl<C> StubAgentServer<C> {
        pub(crate) fn new(connection: C) -> Self {
            Self { connection }
        }
    }

    impl StubAgentServer<StubAgentConnection> {
        pub(crate) fn default_response() -> Self {
            let conn = StubAgentConnection::new();
            conn.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
                acp::ContentChunk::new("Default response".into()),
            )]);
            Self::new(conn)
        }
    }

    impl<C> AgentServer for StubAgentServer<C>
    where
        C: 'static + AgentConnection + Send + Clone,
    {
        fn logo(&self) -> ui::IconName {
            ui::IconName::ZedAgent
        }

        fn agent_id(&self) -> AgentId {
            "Test".into()
        }

        fn connect(
            &self,
            _delegate: AgentServerDelegate,
            _project: Entity<Project>,
            _cx: &mut App,
        ) -> Task<gpui::Result<Rc<dyn AgentConnection>>> {
            Task::ready(Ok(Rc::new(self.connection.clone())))
        }

        fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
            self
        }
    }

    struct FailingAgentServer;

    impl AgentServer for FailingAgentServer {
        fn logo(&self) -> ui::IconName {
            ui::IconName::AiOpenAi
        }

        fn agent_id(&self) -> AgentId {
            AgentId::new("Codex CLI")
        }

        fn connect(
            &self,
            _delegate: AgentServerDelegate,
            _project: Entity<Project>,
            _cx: &mut App,
        ) -> Task<gpui::Result<Rc<dyn AgentConnection>>> {
            Task::ready(Err(anyhow!(
                "extracting downloaded asset for \
                 https://github.com/zed-industries/codex-acp/releases/download/v0.9.4/\
                 codex-acp-0.9.4-aarch64-pc-windows-msvc.zip: \
                 failed to iterate over archive: Invalid gzip header"
            )))
        }

        fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
            self
        }
    }

    /// Agent server whose `connect()` fails while `fail` is `true` and
    /// returns the wrapped connection otherwise. Used to simulate the
    /// race where an external agent isn't yet registered at startup.
    pub(crate) struct FlakyAgentServer {
        connection: StubAgentConnection,
        fail: Arc<std::sync::atomic::AtomicBool>,
    }

    impl FlakyAgentServer {
        pub(crate) fn new(
            connection: StubAgentConnection,
        ) -> (Self, Arc<std::sync::atomic::AtomicBool>) {
            let fail = Arc::new(std::sync::atomic::AtomicBool::new(true));
            (
                Self {
                    connection,
                    fail: fail.clone(),
                },
                fail,
            )
        }
    }

    impl AgentServer for FlakyAgentServer {
        fn logo(&self) -> ui::IconName {
            ui::IconName::ZedAgent
        }

        fn agent_id(&self) -> AgentId {
            "Flaky".into()
        }

        fn connect(
            &self,
            _delegate: AgentServerDelegate,
            _project: Entity<Project>,
            _cx: &mut App,
        ) -> Task<gpui::Result<Rc<dyn AgentConnection>>> {
            if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
                Task::ready(Err(anyhow!(
                    "Custom agent server `Flaky` is not registered"
                )))
            } else {
                Task::ready(Ok(Rc::new(self.connection.clone())))
            }
        }

        fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
            self
        }
    }

    /// Agent server whose `connect()` is refused for want of a sandbox unless
    /// the start was confirmed to run outside it.
    struct SandboxRefusingAgentServer {
        connection: StubAgentConnection,
        outside_starts: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl AgentServer for SandboxRefusingAgentServer {
        fn logo(&self) -> ui::IconName {
            ui::IconName::ZedAgent
        }

        fn agent_id(&self) -> AgentId {
            "Sandboxed".into()
        }

        fn connect(
            &self,
            delegate: AgentServerDelegate,
            _project: Entity<Project>,
            _cx: &mut App,
        ) -> Task<gpui::Result<Rc<dyn AgentConnection>>> {
            if delegate.runs_outside_sandbox() {
                self.outside_starts
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Task::ready(Ok(Rc::new(self.connection.clone())))
            } else {
                Task::ready(Err(anyhow!(LoadError::SandboxUnavailable(
                    "`bwrap` が見つかりません".into()
                ))))
            }
        }

        fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
            self
        }
    }

    /// ADR-F49 decision 19: an agent its sandbox cannot take is not started;
    /// the user may start it outside the sandbox, once, after confirming it.
    #[gpui::test]
    async fn test_an_agent_the_sandbox_cannot_take_starts_outside_it_only_when_confirmed(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let outside_starts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let server = SandboxRefusingAgentServer {
            connection: StubAgentConnection::new(),
            outside_starts: outside_starts.clone(),
        };
        let (conversation_view, cx) = setup_conversation_view(server, cx).await;
        cx.run_until_parked();
        let refused = |cx: &mut VisualTestContext| {
            conversation_view.read_with(cx, |view, _| {
                matches!(
                    view.server_state,
                    ServerState::LoadError {
                        error: LoadError::SandboxUnavailable(_),
                        ..
                    }
                )
            })
        };
        assert!(refused(cx), "the agent is not started without a sandbox");

        conversation_view.update_in(cx, |view, window, cx| {
            view.start_agent_outside_sandbox_once(window, cx)
        });
        cx.run_until_parked();
        cx.simulate_prompt_answer("キャンセル");
        cx.run_until_parked();
        assert_eq!(
            outside_starts.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "nothing starts unless the user confirms"
        );
        assert!(refused(cx));

        conversation_view.update_in(cx, |view, window, cx| {
            view.start_agent_outside_sandbox_once(window, cx)
        });
        cx.run_until_parked();
        cx.simulate_prompt_answer("起動 (今回のみ)");
        cx.run_until_parked();
        assert_eq!(outside_starts.load(std::sync::atomic::Ordering::SeqCst), 1);
        conversation_view.read_with(cx, |view, _| {
            assert!(
                matches!(view.server_state, ServerState::Connected(_)),
                "started outside the sandbox, this once"
            );
        });
    }

    /// ADR-F49 (new decision): the banner is there while that agent runs. A
    /// parked view has none running, in its sandbox or out of it, so the
    /// banner would go on saying something the idle disconnect made untrue —
    /// and asking for the agent back puts it in its sandbox again.
    #[gpui::test]
    async fn test_a_parked_view_stops_saying_its_agent_runs_outside_the_sandbox(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let outside_starts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let server = SandboxRefusingAgentServer {
            connection: StubAgentConnection::new(),
            outside_starts: outside_starts.clone(),
        };
        let (conversation_view, cx) = setup_conversation_view(server, cx).await;
        cx.run_until_parked();
        conversation_view.update_in(cx, |view, window, cx| {
            view.start_agent_outside_sandbox_once(window, cx)
        });
        cx.run_until_parked();
        cx.simulate_prompt_answer("起動 (今回のみ)");
        cx.run_until_parked();
        assert_eq!(outside_starts.load(std::sync::atomic::Ordering::SeqCst), 1);

        let banner_shows = |cx: &mut VisualTestContext| {
            conversation_view.read_with(cx, |view, cx| {
                view.render_outside_sandbox_banner(cx).is_some()
            })
        };
        assert!(
            banner_shows(cx),
            "the agent really is running outside its sandbox"
        );

        assert!(
            conversation_view.update_in(cx, |view, window, cx| view.test_park(window, cx)),
            "the view lets that connection go"
        );
        // The store still hands that connection back — nothing swept it — so
        // it is being parked that has to take the banner away.
        assert!(
            conversation_view.read_with(cx, |view, cx| view.runs_outside_sandbox(cx)),
            "the connection the user confirmed is still in the store"
        );
        assert!(
            !banner_shows(cx),
            "but nothing of that agent is running any more, so the view stops              saying it runs outside its sandbox"
        );
    }

    /// ADR-F49 decision 19 (HIGH-2): the start outside the sandbox belongs to
    /// the view that confirmed it. Another view — and a new thread — connects
    /// in the sandbox as usual, and is refused again where the sandbox cannot
    /// be built.
    #[gpui::test]
    async fn an_agent_started_outside_its_sandbox_is_not_reused_by_other_views(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let outside_starts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let server: Rc<dyn AgentServer> = Rc::new(SandboxRefusingAgentServer {
            connection: StubAgentConnection::new(),
            outside_starts: outside_starts.clone(),
        });

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let thread_store = cx.update(|_window, cx| cx.new(|cx| ThreadStore::new(cx)));
        let connection_store =
            cx.update(|_window, cx| cx.new(|cx| AgentConnectionStore::new(project.clone(), cx)));
        let agent_key = Agent::Custom { id: "Test".into() };

        let new_view = |cx: &mut VisualTestContext| {
            let server = server.clone();
            let connection_store = connection_store.clone();
            let agent_key = agent_key.clone();
            let workspace = workspace.downgrade();
            let project = project.clone();
            let thread_store = thread_store.clone();
            cx.update(|window, cx| {
                cx.new(|cx| {
                    ConversationView::new(
                        server,
                        connection_store,
                        agent_key,
                        None,
                        None,
                        None,
                        None,
                        None,
                        workspace,
                        project,
                        Some(thread_store),
                        AgentThreadSource::AgentPanel,
                        window,
                        cx,
                    )
                })
            })
        };
        let confirming_view = new_view(cx);
        let other_view = new_view(cx);
        cx.run_until_parked();

        let refused = |view: &Entity<ConversationView>, cx: &mut VisualTestContext| {
            view.read_with(cx, |view, _| {
                matches!(
                    view.server_state,
                    ServerState::LoadError {
                        error: LoadError::SandboxUnavailable(_),
                        ..
                    }
                )
            })
        };
        assert!(refused(&confirming_view, cx));
        assert!(refused(&other_view, cx));

        confirming_view.update_in(cx, |view, window, cx| {
            view.start_agent_outside_sandbox_once(window, cx)
        });
        cx.run_until_parked();
        cx.simulate_prompt_answer("起動 (今回のみ)");
        cx.run_until_parked();
        assert_eq!(outside_starts.load(std::sync::atomic::Ordering::SeqCst), 1);
        confirming_view.read_with(cx, |view, cx| {
            assert!(matches!(view.server_state, ServerState::Connected(_)));
            assert!(
                view.runs_outside_sandbox(cx),
                "the view that confirmed it says so"
            );
        });

        // The other view is untouched, and asking it to connect again keeps
        // it in the sandbox.
        assert!(refused(&other_view, cx));
        other_view.update_in(cx, |view, window, cx| view.reset(window, cx));
        cx.run_until_parked();
        assert!(refused(&other_view, cx), "still the sandbox's own start");
        other_view.read_with(cx, |view, cx| {
            assert!(!view.runs_outside_sandbox(cx));
        });
        assert_eq!(
            outside_starts.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "nothing else started outside the sandbox"
        );

        // A new thread in the confirming view goes back into the sandbox:
        // the connection it was given is for the start it confirmed.
        confirming_view.update_in(cx, |view, window, cx| {
            view.outside_sandbox = None;
            view.reset(window, cx);
        });
        cx.run_until_parked();
        assert!(refused(&confirming_view, cx));
        assert_eq!(outside_starts.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn the_agent_hosts_consent_says_they_apply_to_the_whole_ide() {
        let detail = agent_hosts_detail(
            &AgentId::new("codex-acp"),
            &["api.openai.com".into(), "chatgpt.com".into()],
            &["chatgpt.com".into()],
        );
        assert!(detail.contains("IDE 全体の許可リスト"), "{detail}");
        assert!(detail.contains("api.openai.com (登録済み)"), "{detail}");
        assert!(detail.contains("chatgpt.com (未登録)"), "{detail}");
    }

    /// Agent server whose `connect()` is refused with `SandboxHostsNotAllowed`
    /// until `host` is actually in the running egress policy — stands in for
    /// the sandboxed Claude Code CLI's own host check without depending on a
    /// real WSL sandbox.
    struct HostsGatedAgentServer {
        connection: StubAgentConnection,
        policy: EgressPolicyHandle,
        host: &'static str,
        connects: Arc<AtomicUsize>,
    }

    impl AgentServer for HostsGatedAgentServer {
        fn logo(&self) -> ui::IconName {
            ui::IconName::ZedAgent
        }

        fn agent_id(&self) -> AgentId {
            "HostsGated".into()
        }

        fn connect(
            &self,
            _delegate: AgentServerDelegate,
            _project: Entity<Project>,
            _cx: &mut App,
        ) -> Task<gpui::Result<Rc<dyn AgentConnection>>> {
            let allowed =
                RequestHost::parse(self.host).is_some_and(|host| self.policy.allows(&host));
            if allowed {
                self.connects.fetch_add(1, Ordering::SeqCst);
                Task::ready(Ok(Rc::new(self.connection.clone())))
            } else {
                Task::ready(Err(anyhow!(LoadError::SandboxHostsNotAllowed(vec![
                    self.host.into()
                ]))))
            }
        }

        fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
            self
        }
    }

    /// A conversation refused because `registry.npmjs.org` is not in the
    /// running egress policy yet, with that policy (deny-all, published as the
    /// IDE's) and a count of the agent's successful starts.
    async fn hosts_gated_conversation_view(
        cx: &mut TestAppContext,
    ) -> (
        Entity<ConversationView>,
        &mut VisualTestContext,
        EgressPolicyHandle,
        Arc<AtomicUsize>,
    ) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        let policy = EgressPolicyHandle::new(EgressPolicy::deny_all());
        cx.update(|cx| {
            <dyn Fs>::set_global(fs.clone(), cx);
            cx.set_global(egress_policy::EgressAllowlistGlobal(policy.clone()));
        });
        // The consent writes into this file, which has no directory yet in a
        // fake file system.
        let allowlist = paths::network_allowlist_file().clone();
        if let Some(directory) = allowlist.parent() {
            fs.create_dir(directory)
                .await
                .expect("the fake file system should take the allowlist's directory");
        }

        let connects = Arc::new(AtomicUsize::new(0));
        let server = HostsGatedAgentServer {
            connection: StubAgentConnection::new(),
            policy: policy.clone(),
            host: "registry.npmjs.org",
            connects: connects.clone(),
        };
        let (conversation_view, cx) = setup_conversation_view(server, cx).await;
        cx.run_until_parked();
        (conversation_view, cx, policy, connects)
    }

    /// ADR-F49 decision 13 (F1): the consent waits for the running egress
    /// policy to actually take in the hosts it just wrote
    /// (`agent_registry_ui`'s shared `await_allowlist_pickup`) before starting
    /// the agent again, rather than the former fixed 3-second guess
    /// (`ALLOWLIST_RELOAD_POLLS` / `ALLOWLIST_RELOAD_INTERVAL`) at how long the
    /// allowlist's file watcher takes.
    #[gpui::test]
    async fn agent_hosts_consent_connects_only_once_the_policy_actually_allows_the_hosts(
        cx: &mut TestAppContext,
    ) {
        let (conversation_view, cx, policy, connects) = hosts_gated_conversation_view(cx).await;
        let refused = |cx: &mut VisualTestContext| {
            conversation_view.read_with(cx, |view, _| {
                matches!(
                    &view.server_state,
                    ServerState::LoadError {
                        error: LoadError::SandboxHostsNotAllowed(hosts),
                        ..
                    } if hosts.iter().any(|host| host.as_ref() == "registry.npmjs.org")
                )
            })
        };
        assert!(refused(cx), "the host is not in the allowlist yet");

        conversation_view.update_in(cx, |view, window, cx| {
            view.ask_to_allow_agent_hosts(vec!["registry.npmjs.org".into()], window, cx)
        });
        cx.run_until_parked();
        cx.simulate_prompt_answer(ADD_AGENT_HOSTS_LABEL);
        cx.run_until_parked();
        assert_eq!(
            connects.load(Ordering::SeqCst),
            0,
            "the policy has not actually taken the write in yet"
        );

        // The watcher's update lands some time after the write - later than
        // the former fixed 3-second poll, but inside the shared wait's
        // 10-second one, so a wait tied to the real signal must still be
        // running here (and one on the old fixed poll would not be).
        cx.executor().advance_clock(Duration::from_secs(5));
        cx.run_until_parked();
        assert_eq!(
            connects.load(Ordering::SeqCst),
            0,
            "still waiting: the policy itself has not changed yet"
        );

        let (allowed, errors) = EgressPolicy::from_host_strings(&["registry.npmjs.org"]);
        assert!(errors.is_empty(), "{errors:?}");
        policy.set(allowed);
        cx.run_until_parked();

        assert_eq!(
            connects.load(Ordering::SeqCst),
            1,
            "the wait must wake on the policy's own change and connect"
        );
        conversation_view.read_with(cx, |view, _| {
            assert!(
                matches!(view.server_state, ServerState::Connected(_)),
                "expected the connection to succeed once the policy allows the host"
            );
        });
    }

    /// The guard that keeps a second click from writing and waiting twice must
    /// be given back on every way out, or the card's button stays disabled for
    /// good and the consent can never be asked for again.
    #[gpui::test]
    async fn agent_hosts_consent_is_askable_again_after_a_decline_or_a_watcher_that_never_picks_up(
        cx: &mut TestAppContext,
    ) {
        let (conversation_view, cx, _policy, connects) = hosts_gated_conversation_view(cx).await;
        let pending = |cx: &mut VisualTestContext| {
            conversation_view.read_with(cx, |view, _| view.agent_hosts_consent_pending)
        };

        conversation_view.update_in(cx, |view, window, cx| {
            view.ask_to_allow_agent_hosts(vec!["registry.npmjs.org".into()], window, cx)
        });
        cx.run_until_parked();
        assert!(pending(cx), "the button is disabled while the prompt is up");
        cx.simulate_prompt_answer(DECLINE_AGENT_HOSTS_LABEL);
        cx.run_until_parked();
        assert!(
            !pending(cx),
            "a declined consent must leave the button clickable"
        );
        assert_eq!(connects.load(Ordering::SeqCst), 0, "nothing was allowed");

        // Asking again works, and this time the policy never takes the write
        // in: the shared wait gives up after its timeout and says so, rather
        // than starting the agent anyway or waiting for good.
        conversation_view.update_in(cx, |view, window, cx| {
            view.ask_to_allow_agent_hosts(vec!["registry.npmjs.org".into()], window, cx)
        });
        cx.run_until_parked();
        cx.simulate_prompt_answer(ADD_AGENT_HOSTS_LABEL);
        cx.run_until_parked();
        assert!(pending(cx), "the write is still on its way to the policy");
        cx.executor().advance_clock(
            crate::agent_registry_ui::ALLOWLIST_PICKUP_TIMEOUT + Duration::from_secs(1),
        );
        cx.run_until_parked();
        assert!(
            !pending(cx),
            "a wait that timed out must leave the button clickable"
        );
        assert_eq!(
            connects.load(Ordering::SeqCst),
            0,
            "the agent must not start while the policy still refuses its host"
        );
        // The reason goes on the card that asked, which stays up: replacing it
        // with a plain error would take the only button that starts the
        // consent again away with it.
        conversation_view.read_with(cx, |view, _| {
            assert!(
                matches!(
                    &view.server_state,
                    ServerState::LoadError {
                        error: LoadError::SandboxHostsNotAllowed(hosts),
                        ..
                    } if hosts.iter().any(|host| host.as_ref() == "registry.npmjs.org")
                ),
                "the card that asks for the hosts must still be there to try again from"
            );
            assert_eq!(
                view.agent_hosts_consent_error.as_deref(),
                Some(crate::agent_registry_ui::ALLOWLIST_NOT_APPLIED)
            );
        });
    }

    /// The way back in after a wait that gave up: by then the hosts are in the
    /// file and only the running policy is behind, so pressing the card's
    /// button again must not ask for an addition there is nothing to add for —
    /// it waits for the policy and starts the agent once it catches up.
    #[gpui::test]
    async fn the_card_after_a_pickup_that_never_landed_starts_the_agent_when_the_policy_catches_up(
        cx: &mut TestAppContext,
    ) {
        let (conversation_view, cx, policy, connects) = hosts_gated_conversation_view(cx).await;

        conversation_view.update_in(cx, |view, window, cx| {
            view.ask_to_allow_agent_hosts(vec!["registry.npmjs.org".into()], window, cx)
        });
        cx.run_until_parked();
        cx.simulate_prompt_answer(ADD_AGENT_HOSTS_LABEL);
        cx.run_until_parked();
        cx.executor().advance_clock(
            crate::agent_registry_ui::ALLOWLIST_PICKUP_TIMEOUT + Duration::from_secs(1),
        );
        cx.run_until_parked();
        conversation_view.read_with(cx, |view, _| {
            assert_eq!(
                view.agent_hosts_consent_error.as_deref(),
                Some(crate::agent_registry_ui::ALLOWLIST_NOT_APPLIED)
            );
        });

        // The watcher catches up late, and the user presses the card's button.
        let (allowed, errors) = EgressPolicy::from_host_strings(&["registry.npmjs.org"]);
        assert!(errors.is_empty(), "{errors:?}");
        policy.set(allowed);
        cx.run_until_parked();
        conversation_view.update_in(cx, |view, window, cx| {
            view.ask_to_allow_agent_hosts(vec!["registry.npmjs.org".into()], window, cx)
        });
        cx.run_until_parked();

        assert_eq!(
            connects.load(Ordering::SeqCst),
            1,
            "the retry must start the agent without asking for the addition again"
        );
        conversation_view.read_with(cx, |view, _| {
            assert!(view.agent_hosts_consent_error.is_none());
            assert!(
                matches!(view.server_state, ServerState::Connected(_)),
                "expected the retry to connect once the policy allows the host"
            );
        });
    }

    fn build_test_thread(
        connection: Rc<dyn AgentConnection>,
        project: Entity<Project>,
        name: &'static str,
        session_id: acp::SessionId,
        cx: &mut App,
    ) -> Entity<AcpThread> {
        let action_log = cx.new(|_| ActionLog::new(project.clone()));
        cx.new(|cx| {
            AcpThread::new(
                None,
                Some(name.into()),
                None,
                connection,
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

    #[derive(Clone, Default)]
    struct PreloadedElicitationConnection {
        elicitation_id: Arc<Mutex<Option<ElicitationEntryId>>>,
    }

    impl AgentConnection for PreloadedElicitationConnection {
        fn agent_id(&self) -> AgentId {
            AgentId::new("preloaded-elicitation")
        }

        fn telemetry_id(&self) -> SharedString {
            "preloaded-elicitation".into()
        }

        fn new_session(
            self: Rc<Self>,
            project: Entity<Project>,
            _work_dirs: PathList,
            cx: &mut App,
        ) -> Task<gpui::Result<Entity<AcpThread>>> {
            let session_id = acp::SessionId::new("new-session");
            let thread = build_test_thread(
                self.clone(),
                project,
                "PreloadedElicitationConnection",
                session_id.clone(),
                cx,
            );
            thread.update(cx, |thread, cx| {
                thread
                    .request_elicitation(
                        acp::CreateElicitationRequest::new(
                            acp::ElicitationFormMode::new(
                                acp::ElicitationSessionScope::new(session_id),
                                acp::ElicitationSchema::new().string("name", true),
                            ),
                            "Provide a name",
                        ),
                        cx,
                    )
                    .expect("preloaded elicitation should be accepted")
                    .detach();
            });
            let elicitation_id = thread.read_with(cx, |thread, _cx| {
                thread.entries().iter().find_map(|entry| {
                    if let AgentThreadEntry::Elicitation(elicitation_id) = entry {
                        Some(elicitation_id.clone())
                    } else {
                        None
                    }
                })
            });
            *self.elicitation_id.lock() = elicitation_id;
            Task::ready(Ok(thread))
        }

        fn auth_methods(&self) -> &[acp::AuthMethod] {
            &[]
        }

        fn authenticate(
            &self,
            _method_id: acp::AuthMethodId,
            _cx: &mut App,
        ) -> Task<gpui::Result<()>> {
            Task::ready(Ok(()))
        }

        fn prompt(
            &self,
            _params: acp::PromptRequest,
            _cx: &mut App,
        ) -> Task<gpui::Result<acp::PromptResponse>> {
            Task::ready(Ok(acp::PromptResponse::new(acp::StopReason::EndTurn)))
        }

        fn cancel(&self, _session_id: &acp::SessionId, _cx: &mut App) {}

        fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
            self
        }
    }

    struct SessionCreationRequestElicitationServer {
        store: Entity<ElicitationStore>,
        response: Arc<Mutex<Option<acp::ElicitationAction>>>,
    }

    impl AgentServer for SessionCreationRequestElicitationServer {
        fn logo(&self) -> ui::IconName {
            ui::IconName::ZedAgent
        }

        fn agent_id(&self) -> AgentId {
            "SessionCreationRequestElicitation".into()
        }

        fn connect(
            &self,
            _delegate: AgentServerDelegate,
            _project: Entity<Project>,
            _cx: &mut App,
        ) -> Task<gpui::Result<Rc<dyn AgentConnection>>> {
            let connection = SessionCreationRequestElicitationConnection {
                store: self.store.clone(),
                response: self.response.clone(),
            };
            Task::ready(Ok(Rc::new(connection)))
        }

        fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
            self
        }
    }

    struct SessionCreationRequestElicitationConnection {
        store: Entity<ElicitationStore>,
        response: Arc<Mutex<Option<acp::ElicitationAction>>>,
    }

    impl AgentConnection for SessionCreationRequestElicitationConnection {
        fn agent_id(&self) -> AgentId {
            AgentId::new("session-creation-request-elicitation")
        }

        fn telemetry_id(&self) -> SharedString {
            "session-creation-request-elicitation".into()
        }

        fn new_session(
            self: Rc<Self>,
            project: Entity<Project>,
            _work_dirs: PathList,
            cx: &mut App,
        ) -> Task<gpui::Result<Entity<AcpThread>>> {
            let thread = build_test_thread(
                self.clone(),
                project,
                "SessionCreationRequestElicitationConnection",
                acp::SessionId::new("session-creation-request-elicitation-session"),
                cx,
            );
            let first_response_task = self.store.update(cx, |store, cx| {
                store
                    .request_elicitation(
                        acp::CreateElicitationRequest::new(
                            acp::ElicitationFormMode::new(
                                acp::ElicitationRequestScope::new(acp::RequestId::Number(1)),
                                acp::ElicitationSchema::new().string("name", true),
                            ),
                            "Provide a name",
                        ),
                        cx,
                    )
                    .expect("first request-scoped elicitation should be accepted")
            });
            self.store
                .update(cx, |store, cx| {
                    store
                        .request_elicitation(
                            acp::CreateElicitationRequest::new(
                                acp::ElicitationFormMode::new(
                                    acp::ElicitationRequestScope::new(acp::RequestId::Number(2)),
                                    acp::ElicitationSchema::new().string("account", true),
                                ),
                                "Provide an account",
                            ),
                            cx,
                        )
                        .expect("second request-scoped elicitation should be accepted")
                })
                .detach();

            let response = self.response.clone();
            cx.spawn(async move |_cx| {
                let elicitation_response = first_response_task.await;
                *response.lock() = Some(elicitation_response.action);
                Ok(thread)
            })
        }

        fn request_elicitations(&self) -> Option<Entity<ElicitationStore>> {
            Some(self.store.clone())
        }

        fn auth_methods(&self) -> &[acp::AuthMethod] {
            &[]
        }

        fn authenticate(
            &self,
            _method_id: acp::AuthMethodId,
            _cx: &mut App,
        ) -> Task<gpui::Result<()>> {
            Task::ready(Ok(()))
        }

        fn prompt(
            &self,
            _params: acp::PromptRequest,
            _cx: &mut App,
        ) -> Task<gpui::Result<acp::PromptResponse>> {
            Task::ready(Ok(acp::PromptResponse::new(acp::StopReason::EndTurn)))
        }

        fn cancel(&self, _session_id: &acp::SessionId, _cx: &mut App) {}

        fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
            self
        }
    }

    struct ReleaseRequestElicitationServer {
        response: Arc<Mutex<Option<acp::ElicitationAction>>>,
    }

    impl AgentServer for ReleaseRequestElicitationServer {
        fn logo(&self) -> ui::IconName {
            ui::IconName::ZedAgent
        }

        fn agent_id(&self) -> AgentId {
            "ReleaseRequestElicitation".into()
        }

        fn connect(
            &self,
            _delegate: AgentServerDelegate,
            _project: Entity<Project>,
            cx: &mut App,
        ) -> Task<gpui::Result<Rc<dyn AgentConnection>>> {
            let connection = ReleaseRequestElicitationConnection {
                store: cx.new(|_| ElicitationStore::default()),
                response: self.response.clone(),
            };
            Task::ready(Ok(Rc::new(connection)))
        }

        fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
            self
        }
    }

    struct ReleaseRequestElicitationConnection {
        store: Entity<ElicitationStore>,
        response: Arc<Mutex<Option<acp::ElicitationAction>>>,
    }

    impl AgentConnection for ReleaseRequestElicitationConnection {
        fn agent_id(&self) -> AgentId {
            AgentId::new("release-request-elicitation")
        }

        fn telemetry_id(&self) -> SharedString {
            "release-request-elicitation".into()
        }

        fn new_session(
            self: Rc<Self>,
            project: Entity<Project>,
            _work_dirs: PathList,
            cx: &mut App,
        ) -> Task<gpui::Result<Entity<AcpThread>>> {
            let thread = build_test_thread(
                self.clone(),
                project,
                "ReleaseRequestElicitationConnection",
                acp::SessionId::new("release-request-elicitation-session"),
                cx,
            );
            let response_task = self.store.update(cx, |store, cx| {
                store
                    .request_elicitation(
                        acp::CreateElicitationRequest::new(
                            acp::ElicitationFormMode::new(
                                acp::ElicitationRequestScope::new(acp::RequestId::Number(1)),
                                acp::ElicitationSchema::new().string("name", true),
                            ),
                            "Provide a name",
                        ),
                        cx,
                    )
                    .expect("request-scoped elicitation should be accepted")
            });
            let response = self.response.clone();
            cx.spawn(async move |_cx| {
                let elicitation_response = response_task.await;
                *response.lock() = Some(elicitation_response.action);
            })
            .detach();
            Task::ready(Ok(thread))
        }

        fn request_elicitations(&self) -> Option<Entity<ElicitationStore>> {
            Some(self.store.clone())
        }

        fn auth_methods(&self) -> &[acp::AuthMethod] {
            &[]
        }

        fn authenticate(
            &self,
            _method_id: acp::AuthMethodId,
            _cx: &mut App,
        ) -> Task<gpui::Result<()>> {
            Task::ready(Ok(()))
        }

        fn prompt(
            &self,
            _params: acp::PromptRequest,
            _cx: &mut App,
        ) -> Task<gpui::Result<acp::PromptResponse>> {
            Task::ready(Ok(acp::PromptResponse::new(acp::StopReason::EndTurn)))
        }

        fn cancel(&self, _session_id: &acp::SessionId, _cx: &mut App) {}

        fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
            self
        }
    }

    #[derive(Clone)]
    struct ResumeOnlyAgentConnection;

    impl AgentConnection for ResumeOnlyAgentConnection {
        fn agent_id(&self) -> AgentId {
            AgentId::new("resume-only")
        }

        fn telemetry_id(&self) -> SharedString {
            "resume-only".into()
        }

        fn new_session(
            self: Rc<Self>,
            project: Entity<Project>,
            _work_dirs: PathList,
            cx: &mut gpui::App,
        ) -> Task<gpui::Result<Entity<AcpThread>>> {
            let thread = build_test_thread(
                self,
                project,
                "ResumeOnlyAgentConnection",
                acp::SessionId::new("new-session"),
                cx,
            );
            Task::ready(Ok(thread))
        }

        fn supports_resume_session(&self) -> bool {
            true
        }

        fn resume_session(
            self: Rc<Self>,
            session_id: acp::SessionId,
            project: Entity<Project>,
            _work_dirs: PathList,
            _title: Option<SharedString>,
            cx: &mut App,
        ) -> Task<gpui::Result<Entity<AcpThread>>> {
            let thread =
                build_test_thread(self, project, "ResumeOnlyAgentConnection", session_id, cx);
            Task::ready(Ok(thread))
        }

        fn auth_methods(&self) -> &[acp::AuthMethod] {
            &[]
        }

        fn authenticate(
            &self,
            _method_id: acp::AuthMethodId,
            _cx: &mut App,
        ) -> Task<gpui::Result<()>> {
            Task::ready(Ok(()))
        }

        fn prompt(
            &self,
            _params: acp::PromptRequest,
            _cx: &mut App,
        ) -> Task<gpui::Result<acp::PromptResponse>> {
            Task::ready(Ok(acp::PromptResponse::new(acp::StopReason::EndTurn)))
        }

        fn cancel(&self, _session_id: &acp::SessionId, _cx: &mut App) {}

        fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
            self
        }
    }

    /// Simulates an agent that requires authentication before a session can be
    /// created. `new_session` returns `AuthRequired` until `authenticate` is
    /// called with the correct method, after which sessions are created normally.
    #[derive(Clone)]
    struct AuthGatedAgentConnection {
        authenticated: Arc<Mutex<bool>>,
        auth_methods: Vec<acp::AuthMethod>,
    }

    impl AuthGatedAgentConnection {
        const AUTH_METHOD_ID: &str = "test-login";

        fn new() -> Self {
            Self::with_methods(&[Self::AUTH_METHOD_ID])
        }

        fn with_methods(method_ids: &[&'static str]) -> Self {
            Self {
                authenticated: Arc::new(Mutex::new(false)),
                auth_methods: method_ids
                    .iter()
                    .map(|id| {
                        acp::AuthMethod::Agent(acp::AuthMethodAgent::new(
                            *id,
                            format!("Sign in with {id}"),
                        ))
                    })
                    .collect(),
            }
        }

        fn is_authenticated(&self) -> bool {
            *self.authenticated.lock()
        }
    }

    impl AgentConnection for AuthGatedAgentConnection {
        fn agent_id(&self) -> AgentId {
            AgentId::new("auth-gated")
        }

        fn telemetry_id(&self) -> SharedString {
            "auth-gated".into()
        }

        fn new_session(
            self: Rc<Self>,
            project: Entity<Project>,
            work_dirs: PathList,
            cx: &mut gpui::App,
        ) -> Task<gpui::Result<Entity<AcpThread>>> {
            if !*self.authenticated.lock() {
                return Task::ready(Err(acp_thread::AuthRequired::new()
                    .with_description("Sign in to continue".to_string())
                    .into()));
            }

            let session_id = acp::SessionId::new("auth-gated-session");
            let action_log = cx.new(|_| ActionLog::new(project.clone()));
            Task::ready(Ok(cx.new(|cx| {
                AcpThread::new(
                    None,
                    None,
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
            })))
        }

        fn auth_methods(&self) -> &[acp::AuthMethod] {
            &self.auth_methods
        }

        fn authenticate(
            &self,
            method_id: acp::AuthMethodId,
            _cx: &mut App,
        ) -> Task<gpui::Result<()>> {
            if self
                .auth_methods
                .iter()
                .any(|method| method.id() == &method_id)
            {
                *self.authenticated.lock() = true;
                Task::ready(Ok(()))
            } else {
                Task::ready(Err(anyhow::anyhow!("Unknown auth method")))
            }
        }

        fn supports_logout(&self) -> bool {
            true
        }

        fn logout(&self, _cx: &mut App) -> Task<gpui::Result<()>> {
            *self.authenticated.lock() = false;
            Task::ready(Ok(()))
        }

        fn prompt(
            &self,
            _params: acp::PromptRequest,
            _cx: &mut App,
        ) -> Task<gpui::Result<acp::PromptResponse>> {
            unimplemented!()
        }

        fn cancel(&self, _session_id: &acp::SessionId, _cx: &mut App) {
            unimplemented!()
        }

        fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
            self
        }
    }

    /// ADR-F53: stands in for the ACP Claude entry — the id the fork's policy
    /// is keyed on — offering the vendor's subscription sign-in next to an
    /// API-key method.
    struct ClaudeAcpAgentServer {
        connection: AuthGatedAgentConnection,
    }

    impl AgentServer for ClaudeAcpAgentServer {
        fn logo(&self) -> ui::IconName {
            ui::IconName::AiClaude
        }

        fn agent_id(&self) -> AgentId {
            AgentId::new(agent_servers::CLAUDE_AGENT_ID)
        }

        fn connect(
            &self,
            _delegate: AgentServerDelegate,
            _project: Entity<Project>,
            _cx: &mut App,
        ) -> Task<gpui::Result<Rc<dyn AgentConnection>>> {
            Task::ready(Ok(Rc::new(self.connection.clone())))
        }

        fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
            self
        }
    }

    const CLAUDE_ACP_API_KEY_METHOD: &str = "anthropic-api-key";
    /// `debug_bounds` takes a `&'static str`, so the two selectors this test
    /// looks for are spelled out and tied to [`auth_method_selector`] below.
    const CLAUDE_SUBSCRIPTION_METHOD_SELECTOR: &str = "AUTH-METHOD-claude-login";
    const CLAUDE_ACP_API_KEY_METHOD_SELECTOR: &str = "AUTH-METHOD-anthropic-api-key";

    /// `agent_servers.claude-acp.allow_subscription_sign_in` を書き換えて、
    /// 設定が届くまで待つ。ビューが見るのと同じグローバルを通るので、
    /// 設定からカードまでの経路ごと確かめられる。
    async fn set_subscription_sign_in(
        conversation_view: &Entity<ConversationView>,
        allowed: bool,
        cx: &mut VisualTestContext,
    ) {
        let fs = conversation_view.read_with(cx, |view, cx| view.project.read(cx).fs().clone());
        let written = cx.update(|_window, cx| {
            agent_servers::set_subscription_sign_in_allowed(
                agent_servers::CLAUDE_AGENT_ID,
                allowed,
                fs,
                cx,
            )
        });
        written.await.expect("設定を書ける");
        cx.run_until_parked();
        cx.update(|window, _cx| window.refresh());
        cx.run_until_parked();
    }

    /// ADR-F55: 未改変の `claude` が Anthropic のフローで自分でサインインする
    /// 経路なので、サブスクリプションのログインは既定でカードに出る。
    /// 設定で切るとカードから消え、カードを飛ばして要求しても断られる。
    #[gpui::test]
    async fn the_acp_claudes_subscription_sign_in_follows_the_setting(cx: &mut TestAppContext) {
        init_test(cx);

        assert_eq!(
            auth_method_selector(agent_servers::CLAUDE_SUBSCRIPTION_AUTH_METHOD),
            CLAUDE_SUBSCRIPTION_METHOD_SELECTOR
        );
        assert_eq!(
            auth_method_selector(CLAUDE_ACP_API_KEY_METHOD),
            CLAUDE_ACP_API_KEY_METHOD_SELECTOR
        );

        let connection = AuthGatedAgentConnection::with_methods(&[
            agent_servers::CLAUDE_SUBSCRIPTION_AUTH_METHOD,
            CLAUDE_ACP_API_KEY_METHOD,
        ]);
        let (conversation_view, cx) = setup_conversation_view(
            ClaudeAcpAgentServer {
                connection: connection.clone(),
            },
            cx,
        )
        .await;
        add_to_workspace(conversation_view.clone(), cx);
        let fs = conversation_view.read_with(cx, |view, cx| view.project.read(cx).fs().clone());
        fs.create_dir(::paths::settings_file().parent().expect("設定ファイルの親"))
            .await
            .expect("設定ファイルの置き場を作れる");
        // A frame that reuses a cached pane records no `debug_selector`.
        cx.update(|window, _cx| window.refresh());
        cx.run_until_parked();

        assert!(
            cx.debug_bounds(FORK_AUTH_NOTICE_SELECTOR).is_some(),
            "the card carries this fork's note and its key field"
        );
        assert!(
            cx.debug_bounds(CLAUDE_SUBSCRIPTION_METHOD_SELECTOR)
                .is_some(),
            "the subscription sign-in is offered by default"
        );
        assert!(
            cx.debug_bounds(CLAUDE_ACP_API_KEY_METHOD_SELECTOR)
                .is_some(),
            "an API-key method stays beside it"
        );
        assert!(
            cx.debug_bounds(SubscriptionSignIn::Offered.notice_selector())
                .is_some(),
            "the note says the sign-in is offered"
        );
        assert!(
            cx.debug_bounds(LAUNCH_CLAUDE_CODE_SELECTOR).is_none(),
            "nothing sends the user to the CLI tab when the agent signs in here"
        );

        // 設定で切るとカードから消え、要求しても通らない。
        // サインインが通るとカードそのものが消えるので、ボタンの有無を
        // 見る確かめは全部ここで済ませてから最後にサインインする。
        set_subscription_sign_in(&conversation_view, false, cx).await;

        assert!(
            cx.debug_bounds(CLAUDE_SUBSCRIPTION_METHOD_SELECTOR)
                .is_none(),
            "the sign-in the setting turns off is left off the card"
        );
        assert!(
            cx.debug_bounds(CLAUDE_ACP_API_KEY_METHOD_SELECTOR)
                .is_some(),
            "an API-key method is unaffected by the setting"
        );
        assert!(
            cx.debug_bounds(FORK_AUTH_NOTICE_SELECTOR).is_some(),
            "the note and the key field stay"
        );
        assert!(
            cx.debug_bounds(SubscriptionSignIn::TurnedOff.notice_selector())
                .is_some(),
            "turned off reads differently from never advertised: what the user \
             does next is not the same"
        );
        assert!(
            cx.debug_bounds(SubscriptionSignIn::Offered.notice_selector())
                .is_none()
        );

        conversation_view.update_in(cx, |view, window, cx| {
            view.authenticate(
                acp::AuthMethodId::new(agent_servers::CLAUDE_SUBSCRIPTION_AUTH_METHOD),
                window,
                cx,
            );
        });
        cx.run_until_parked();
        assert!(
            !connection.is_authenticated(),
            "the sign-in the card left off is refused when asked for anyway"
        );

        // 戻せばまた出る。
        set_subscription_sign_in(&conversation_view, true, cx).await;
        assert!(
            cx.debug_bounds(CLAUDE_SUBSCRIPTION_METHOD_SELECTOR)
                .is_some(),
            "turning it back on brings the sign-in back"
        );

        conversation_view.update_in(cx, |view, window, cx| {
            view.authenticate(
                acp::AuthMethodId::new(agent_servers::CLAUDE_SUBSCRIPTION_AUTH_METHOD),
                window,
                cx,
            );
        });
        cx.run_until_parked();
        assert!(
            connection.is_authenticated(),
            "the sign-in the card offers works"
        );
    }

    /// ADR-F55: ベンダーがメソッドを改名しても (`Login`、`claude_login`)、
    /// 設定で切ってあれば同じように消えて断られる。取りこぼすと、利用者が
    /// 切ったはずの設定が黙って効かなくなる。
    #[gpui::test]
    async fn a_renamed_sign_in_is_turned_off_too(cx: &mut TestAppContext) {
        init_test(cx);
        const RENAMED: &str = "Login";

        let connection =
            AuthGatedAgentConnection::with_methods(&[RENAMED, CLAUDE_ACP_API_KEY_METHOD]);
        let (conversation_view, cx) = setup_conversation_view(
            ClaudeAcpAgentServer {
                connection: connection.clone(),
            },
            cx,
        )
        .await;
        add_to_workspace(conversation_view.clone(), cx);
        let fs = conversation_view.read_with(cx, |view, cx| view.project.read(cx).fs().clone());
        fs.create_dir(::paths::settings_file().parent().expect("設定ファイルの親"))
            .await
            .expect("設定ファイルの置き場を作れる");
        cx.update(|window, _cx| window.refresh());
        cx.run_until_parked();

        const RENAMED_SELECTOR: &str = "AUTH-METHOD-Login";
        assert_eq!(auth_method_selector(RENAMED), RENAMED_SELECTOR);
        assert!(cx.debug_bounds(RENAMED_SELECTOR).is_some(), "既定では出る");
        assert!(
            cx.debug_bounds(SubscriptionSignIn::Offered.notice_selector())
                .is_some(),
            "改名されていてもサインインとして数える"
        );

        set_subscription_sign_in(&conversation_view, false, cx).await;
        assert!(
            cx.debug_bounds(RENAMED_SELECTOR).is_none(),
            "改名されたサインインもカードから消える"
        );
        assert!(
            cx.debug_bounds(CLAUDE_ACP_API_KEY_METHOD_SELECTOR)
                .is_some(),
            "鍵のメソッドは残る"
        );

        conversation_view.update_in(cx, |view, window, cx| {
            view.authenticate(acp::AuthMethodId::new(RENAMED), window, cx);
        });
        cx.run_until_parked();
        assert!(
            !connection.is_authenticated(),
            "カードを飛ばして要求しても断る"
        );
    }

    /// ADR-F55: 鍵とサブスクリプションが両方そろっているカードでは、どちらが
    /// 使われるかを 1 行足す。押せば署名はできるので、押した利用者が課金先を
    /// 取り違えるのはここでしか止められない。キーを消せばその行も消える。
    #[gpui::test]
    async fn the_card_says_which_one_wins_when_both_are_set(cx: &mut TestAppContext) {
        init_test(cx);
        const KEY: &str = "sk-ant-api03-both";

        let connection = AuthGatedAgentConnection::with_methods(&[
            agent_servers::CLAUDE_SUBSCRIPTION_AUTH_METHOD,
            CLAUDE_ACP_API_KEY_METHOD,
        ]);
        let (conversation_view, cx) =
            setup_conversation_view(ClaudeAcpAgentServer { connection }, cx).await;
        add_to_workspace(conversation_view.clone(), cx);
        let fs = conversation_view.read_with(cx, |view, cx| view.project.read(cx).fs().clone());
        fs.create_dir(::paths::settings_file().parent().expect("設定ファイルの親"))
            .await
            .expect("設定ファイルの置き場を作れる");
        cx.update(|window, _cx| window.refresh());
        cx.run_until_parked();

        assert!(
            cx.debug_bounds(SubscriptionSignIn::Offered.notice_selector())
                .is_some(),
            "サインインは出ている"
        );
        assert!(
            cx.debug_bounds(API_KEY_WINS_SELECTOR).is_none(),
            "キーが無いうちは優先順位の話をしない"
        );

        save_api_key(&conversation_view, KEY, cx).await;
        assert_eq!(saved_api_key(cx).as_deref(), Some(KEY));
        assert!(
            cx.debug_bounds(API_KEY_WINS_SELECTOR).is_some(),
            "両方そろったらどちらが使われるかを言う"
        );

        let remove = cx
            .debug_bounds(API_KEY_REMOVE_SELECTOR)
            .expect("設定済みのカードに「キーを削除」がある");
        cx.simulate_click(remove.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        cx.update(|window, _cx| window.refresh());
        cx.run_until_parked();

        assert_eq!(saved_api_key(cx), None);
        assert!(
            cx.debug_bounds(API_KEY_WINS_SELECTOR).is_none(),
            "キーを消せばその行も消える"
        );
        assert!(
            cx.debug_bounds(SubscriptionSignIn::Offered.notice_selector())
                .is_some(),
            "サインインの注記は残る"
        );
    }

    /// ADR-F55: 端末を開く直前の関門。カードを経ずに (キー割り当て・古い
    /// フレーム・`/login`) ここへ来ても、設定で切ってあれば端末そのものを
    /// 開かない。カードの認証メソッドはエージェント種別でこの経路を通らない
    /// ので、ここは別に確かめる。
    #[gpui::test]
    async fn a_turned_off_sign_in_never_reaches_a_terminal(cx: &mut TestAppContext) {
        init_test(cx);

        let connection = AuthGatedAgentConnection::with_methods(&[
            agent_servers::CLAUDE_SUBSCRIPTION_AUTH_METHOD,
            CLAUDE_ACP_API_KEY_METHOD,
        ]);
        let (conversation_view, cx) =
            setup_conversation_view(ClaudeAcpAgentServer { connection }, cx).await;
        add_to_workspace(conversation_view.clone(), cx);
        let fs = conversation_view.read_with(cx, |view, cx| view.project.read(cx).fs().clone());
        fs.create_dir(::paths::settings_file().parent().expect("設定ファイルの親"))
            .await
            .expect("設定ファイルの置き場を作れる");
        cx.run_until_parked();

        fn start_login(
            conversation_view: &Entity<ConversationView>,
            method: &str,
            cx: &mut VisualTestContext,
        ) -> Task<anyhow::Result<()>> {
            let (workspace, project) = conversation_view.read_with(cx, |view, _cx| {
                (
                    view.workspace.upgrade().expect("ワークスペース"),
                    view.project.clone(),
                )
            });
            let login = acp_thread::build_terminal_auth_task(
                "test-login".to_string(),
                "ログイン".to_string(),
                "claude".to_string(),
                vec!["/login".to_string()],
                HashMap::default(),
            );
            let method = acp::AuthMethodId::new(method.to_string());
            cx.update(|window, cx| {
                ConversationView::spawn_external_agent_login(
                    login,
                    workspace,
                    project,
                    AgentId::new(agent_servers::CLAUDE_AGENT_ID),
                    method,
                    false,
                    false,
                    None,
                    window,
                    cx,
                )
            })
        }

        // 既定では関門を素通りし、端末が無いことで初めて失敗する = 断り文句とは
        // 別の理由。ここが同じ文言で失敗するなら、この確かめは同義反復になる。
        let allowed = start_login(
            &conversation_view,
            agent_servers::CLAUDE_SUBSCRIPTION_AUTH_METHOD,
            cx,
        )
        .await
        .expect_err("このテスト環境には端末パネルが無い");
        assert_ne!(
            format!("{allowed}"),
            SUBSCRIPTION_SIGN_IN_REFUSED,
            "既定では関門で止めない"
        );

        set_subscription_sign_in(&conversation_view, false, cx).await;
        let refused = start_login(
            &conversation_view,
            agent_servers::CLAUDE_SUBSCRIPTION_AUTH_METHOD,
            cx,
        )
        .await
        .expect_err("切ってあれば断る");
        assert_eq!(
            format!("{refused}"),
            SUBSCRIPTION_SIGN_IN_REFUSED,
            "端末を開く前に断る"
        );

        let key_method = start_login(&conversation_view, CLAUDE_ACP_API_KEY_METHOD, cx)
            .await
            .expect_err("このテスト環境には端末パネルが無い");
        assert_ne!(
            format!("{key_method}"),
            SUBSCRIPTION_SIGN_IN_REFUSED,
            "鍵のメソッドはこの設定に関係なく通る"
        );
    }

    /// ADR-F55: エージェントがサインインを 1 つも出していないときは、「切った」
    /// のではなく「出ていない」と言い、公式の CLI を起動する導線を添える。
    #[gpui::test]
    async fn a_card_without_a_sign_in_sends_the_user_to_the_cli(cx: &mut TestAppContext) {
        init_test(cx);

        let connection = AuthGatedAgentConnection::with_methods(&[CLAUDE_ACP_API_KEY_METHOD]);
        let (conversation_view, cx) =
            setup_conversation_view(ClaudeAcpAgentServer { connection }, cx).await;
        add_to_workspace(conversation_view, cx);
        cx.update(|window, _cx| window.refresh());
        cx.run_until_parked();

        assert!(
            cx.debug_bounds(SubscriptionSignIn::NotAdvertised.notice_selector())
                .is_some(),
            "出ていないことを言う"
        );
        assert!(
            cx.debug_bounds(LAUNCH_CLAUDE_CODE_SELECTOR).is_some(),
            "公式の CLI を起動する導線がある"
        );
        assert!(
            cx.debug_bounds(SubscriptionSignIn::Offered.notice_selector())
                .is_none()
        );
    }

    /// `agent_servers.claude-acp.env.ANTHROPIC_API_KEY` にいま入っている値。
    /// `Registry { env }` の形まで固定して読む: 保存先が変われば落ちる。
    fn saved_api_key(cx: &mut VisualTestContext) -> Option<String> {
        saved_env_var(cx, agent_servers::CLAUDE_API_KEY_VARIABLE)
    }

    fn saved_env_var(cx: &mut VisualTestContext, name: &str) -> Option<String> {
        cx.update(|_window, cx| {
            cx.global::<SettingsStore>()
                .get::<project::agent_server_store::AllAgentServersSettings>(None)
                .get(agent_servers::CLAUDE_AGENT_ID)
                .and_then(|agent| match agent {
                    project::agent_server_store::CustomAgentServerSettings::Registry {
                        env,
                        ..
                    } => env.get(name).cloned(),
                    project::agent_server_store::CustomAgentServerSettings::Custom { .. } => None,
                })
        })
    }

    /// カードがいま画面に出しているうち、実装しだいで API キーが混ざりうる
    /// 文字列 (断り書き・入力欄の中身)。ラベルは固定文なので含めない。
    fn api_key_card_text(
        conversation_view: &Entity<ConversationView>,
        cx: &mut VisualTestContext,
    ) -> String {
        conversation_view.read_with(cx, |view, cx| {
            format!(
                "{}\n{}",
                view.api_key_error.clone().unwrap_or_default(),
                view.api_key_editor
                    .as_ref()
                    .map(|editor| editor.read(cx).text(cx))
                    .unwrap_or_default(),
            )
        })
    }

    /// ADR-F53 決定 4: ACP Claude の認証カードは API キーを直接受け取り、
    /// `agent_servers.claude-acp.env` へ書く。空欄は書かずに断り、保存した
    /// キーは画面にもエラー文にも残さない。
    #[gpui::test]
    async fn the_acp_claude_card_takes_an_api_key(cx: &mut TestAppContext) {
        init_test(cx);
        const FIRST_KEY: &str = "sk-ant-api03-first-key";
        const SECOND_KEY: &str = "sk-ant-api03-second-key";

        let connection = AuthGatedAgentConnection::with_methods(&[CLAUDE_ACP_API_KEY_METHOD]);
        let (conversation_view, cx) =
            setup_conversation_view(ClaudeAcpAgentServer { connection }, cx).await;
        add_to_workspace(conversation_view.clone(), cx);
        // A frame that reuses a cached pane records no `debug_selector`.
        cx.update(|window, _cx| window.refresh());
        cx.run_until_parked();

        let fs = conversation_view.read_with(cx, |view, cx| view.project.read(cx).fs().clone());
        fs.create_dir(::paths::settings_file().parent().expect("設定ファイルの親"))
            .await
            .expect("設定ファイルの置き場を作れる");

        assert!(
            cx.debug_bounds(API_KEY_INPUT_SELECTOR).is_some(),
            "キーを入れる欄がカードにある"
        );
        assert!(
            cx.debug_bounds(API_KEY_CONFIGURED_SELECTOR).is_none(),
            "まだ何も設定されていない"
        );

        let save = cx
            .debug_bounds(API_KEY_SAVE_SELECTOR)
            .expect("保存ボタンがカードにある");
        cx.simulate_click(save.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        assert_eq!(saved_api_key(cx), None, "空欄では何も書かない");
        conversation_view.read_with(cx, |view, _cx| {
            assert!(
                view.api_key_error.is_some(),
                "空欄を断った理由がカードに出る"
            );
        });

        let editor = conversation_view.read_with(cx, |view, _cx| {
            view.api_key_editor.clone().expect("入力欄を持っている")
        });
        editor.update_in(cx, |editor, window, cx| {
            editor.set_text(FIRST_KEY, window, cx);
        });
        cx.update(|window, _cx| window.refresh());
        cx.run_until_parked();

        let save = cx
            .debug_bounds(API_KEY_SAVE_SELECTOR)
            .expect("保存ボタンがカードにある");
        cx.simulate_click(save.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        assert_eq!(
            saved_api_key(cx).as_deref(),
            Some(FIRST_KEY),
            "settings.json の agent_servers.claude-acp.env に入る"
        );
        let shown = api_key_card_text(&conversation_view, cx);
        assert!(
            !shown.contains(FIRST_KEY),
            "保存したキーは画面に残さない: {shown:?}"
        );
        conversation_view.read_with(cx, |view, _cx| {
            assert_eq!(view.api_key_error, None, "保存できたので理由は消える");
        });

        cx.update(|window, _cx| window.refresh());
        cx.run_until_parked();
        assert!(
            cx.debug_bounds(API_KEY_CONFIGURED_SELECTOR).is_some(),
            "設定済みの表示に変わる"
        );
        assert!(
            cx.debug_bounds(API_KEY_INPUT_SELECTOR).is_none(),
            "設定済みのあいだは入力欄を出さない"
        );

        // 「変更」で入力欄が戻り、Enter でも同じ経路で保存できる。
        conversation_view.update(cx, |view, cx| {
            view.api_key_editing = true;
            cx.notify();
        });
        cx.update(|window, _cx| window.refresh());
        cx.run_until_parked();
        assert!(
            cx.debug_bounds(API_KEY_INPUT_SELECTOR).is_some(),
            "「変更」で入力欄が戻る"
        );

        editor.update_in(cx, |editor, window, cx| {
            editor.set_text(SECOND_KEY, window, cx);
            let focus_handle = editor.focus_handle(cx);
            window.focus(&focus_handle, cx);
        });
        cx.run_until_parked();
        cx.dispatch_action(editor::actions::Newline);
        cx.run_until_parked();

        assert_eq!(
            saved_api_key(cx).as_deref(),
            Some(SECOND_KEY),
            "Enter でも確定する"
        );
        let shown = api_key_card_text(&conversation_view, cx);
        assert!(
            !shown.contains(SECOND_KEY),
            "入れ替えたキーも画面に残さない: {shown:?}"
        );
    }

    /// 1 行の入力欄は貼り付けられた改行を畳んで見せるだけで中身には残すので、
    /// 端を落としただけの値には改行が残りうる。壊れた環境変数を黙って書かない
    /// こと、前後の空白は落として書くことを固定する。
    #[gpui::test]
    async fn an_api_key_that_is_not_one_line_is_refused(cx: &mut TestAppContext) {
        init_test(cx);
        const PASTED_KEY: &str = "sk-ant-api03-pasted\nfrom-a-document";
        const PADDED_KEY: &str = "sk-ant-api03-padded";

        let connection = AuthGatedAgentConnection::with_methods(&[CLAUDE_ACP_API_KEY_METHOD]);
        let (conversation_view, cx) =
            setup_conversation_view(ClaudeAcpAgentServer { connection }, cx).await;
        add_to_workspace(conversation_view.clone(), cx);
        cx.run_until_parked();

        let fs = conversation_view.read_with(cx, |view, cx| view.project.read(cx).fs().clone());
        fs.create_dir(::paths::settings_file().parent().expect("設定ファイルの親"))
            .await
            .expect("設定ファイルの置き場を作れる");
        let editor = conversation_view.read_with(cx, |view, _cx| {
            view.api_key_editor.clone().expect("入力欄を持っている")
        });

        editor.update_in(cx, |editor, window, cx| {
            editor.set_text(PASTED_KEY, window, cx);
        });
        conversation_view.update_in(cx, |view, window, cx| view.submit_api_key(window, cx));
        cx.run_until_parked();

        assert_eq!(saved_api_key(cx), None, "改行を含む値は書かない");
        conversation_view.read_with(cx, |view, _cx| {
            let error = view
                .api_key_error
                .clone()
                .expect("断った理由がカードに出る");
            // 断られた値は直せるよう入力欄に残す。載ってはいけないのは、
            // 入力欄と違ってマスクされない理由の文字列のほう。
            assert!(
                !error.contains("sk-ant-api03-pasted"),
                "断り書きにキーを載せない: {error}"
            );
        });

        editor.update_in(cx, |editor, window, cx| {
            editor.set_text(format!("  {PADDED_KEY}\t"), window, cx);
        });
        conversation_view.update_in(cx, |view, window, cx| view.submit_api_key(window, cx));
        cx.run_until_parked();

        assert_eq!(
            saved_api_key(cx).as_deref(),
            Some(PADDED_KEY),
            "前後の空白は落として書く"
        );
        assert_eq!(
            editor.read_with(cx, |editor, cx| editor.text(cx)),
            "",
            "保存できたキーは入力欄にも残さない"
        );
    }

    /// `agent_servers.claude-acp` が `custom` で、しかも env が既にあるとき。
    /// キーを足すだけで、同じエージェントの他の env も、他のエージェントも、
    /// 無関係な設定も潰さない。
    #[gpui::test]
    async fn saving_an_api_key_keeps_the_other_settings(cx: &mut TestAppContext) {
        init_test(cx);
        const KEY: &str = "sk-ant-api03-kept";
        const EXISTING_SETTINGS: &str = r#"{
  "buffer_font_size": 15,
  "agent_servers": {
    "claude-acp": {
      "type": "custom",
      "command": "/usr/bin/claude-code-acp",
      "env": { "CLAUDE_CONFIG_DIR": "/tmp/keep-me" }
    },
    "gemini": {
      "type": "registry",
      "env": { "SURFACE": "keep-me-too" }
    }
  }
}"#;

        let connection = AuthGatedAgentConnection::with_methods(&[CLAUDE_ACP_API_KEY_METHOD]);
        let (conversation_view, cx) =
            setup_conversation_view(ClaudeAcpAgentServer { connection }, cx).await;
        add_to_workspace(conversation_view.clone(), cx);
        cx.run_until_parked();

        let fs = conversation_view.read_with(cx, |view, cx| view.project.read(cx).fs().clone());
        fs.create_dir(::paths::settings_file().parent().expect("設定ファイルの親"))
            .await
            .expect("設定ファイルの置き場を作れる");
        fs.atomic_write(
            ::paths::settings_file().to_path_buf(),
            EXISTING_SETTINGS.to_string(),
        )
        .await
        .expect("設定ファイルを置ける");

        let editor = conversation_view.read_with(cx, |view, _cx| {
            view.api_key_editor.clone().expect("入力欄を持っている")
        });
        editor.update_in(cx, |editor, window, cx| {
            editor.set_text(KEY, window, cx);
        });
        conversation_view.update_in(cx, |view, window, cx| view.submit_api_key(window, cx));
        cx.run_until_parked();

        let written = fs
            .load(::paths::settings_file())
            .await
            .expect("設定ファイルを読める");
        conversation_view.read_with(cx, |view, _cx| {
            assert_eq!(
                view.api_key_error, None,
                "書けたので理由は出ない: {written}"
            );
        });
        for kept in [
            "\"buffer_font_size\": 15",
            "/tmp/keep-me",
            "keep-me-too",
            "/usr/bin/claude-code-acp",
        ] {
            assert!(written.contains(kept), "{kept} が消えた:\n{written}");
        }
        assert!(written.contains(KEY), "キーが入っていない:\n{written}");
        assert!(
            cx.update(|_window, cx| agent_servers::has_env_var(
                agent_servers::CLAUDE_AGENT_ID,
                agent_servers::CLAUDE_API_KEY_VARIABLE,
                cx
            )),
            "custom 変種でも設定済みとして読める:\n{written}"
        );
    }

    /// 設定ファイルが壊れていて書けないとき。黙って設定済みの顔をせず、
    /// 理由をカードに出す。
    #[gpui::test]
    async fn a_settings_file_that_cannot_be_written_stops_the_save(cx: &mut TestAppContext) {
        init_test(cx);
        const KEY: &str = "sk-ant-api03-unwritable";
        const BROKEN_SETTINGS: &str = "{ \"agent_servers\": ";

        let connection = AuthGatedAgentConnection::with_methods(&[CLAUDE_ACP_API_KEY_METHOD]);
        let (conversation_view, cx) =
            setup_conversation_view(ClaudeAcpAgentServer { connection }, cx).await;
        add_to_workspace(conversation_view.clone(), cx);
        cx.run_until_parked();

        let fs = conversation_view.read_with(cx, |view, cx| view.project.read(cx).fs().clone());
        fs.create_dir(::paths::settings_file().parent().expect("設定ファイルの親"))
            .await
            .expect("設定ファイルの置き場を作れる");
        fs.atomic_write(
            ::paths::settings_file().to_path_buf(),
            BROKEN_SETTINGS.to_string(),
        )
        .await
        .expect("設定ファイルを置ける");

        let editor = conversation_view.read_with(cx, |view, _cx| {
            view.api_key_editor.clone().expect("入力欄を持っている")
        });
        editor.update_in(cx, |editor, window, cx| {
            editor.set_text(KEY, window, cx);
        });
        conversation_view.update_in(cx, |view, window, cx| view.submit_api_key(window, cx));
        cx.run_until_parked();

        assert_eq!(
            fs.load(::paths::settings_file())
                .await
                .expect("設定ファイルを読める"),
            BROKEN_SETTINGS,
            "壊れたファイルを書き換えない"
        );
        conversation_view.read_with(cx, |view, _cx| {
            let error = view
                .api_key_error
                .clone()
                .expect("書けなかった理由がカードに出る");
            assert!(
                error.starts_with("API キーを保存できませんでした"),
                "理由が日本語で始まる: {error}"
            );
            assert!(!error.contains(KEY), "理由にキーを載せない: {error}");
            assert!(
                view.api_key_save_task.is_none(),
                "失敗しても次の保存を受け付ける"
            );
        });
    }

    /// キーを `env` から読まないエージェントの認証カードには、入力欄そのものを
    /// 出さない (空の Editor も持たない)。
    #[gpui::test]
    async fn only_an_agent_that_reads_the_key_gets_the_field(cx: &mut TestAppContext) {
        init_test(cx);

        let connection = AuthGatedAgentConnection::new();
        let (conversation_view, cx) =
            setup_conversation_view(StubAgentServer::new(connection), cx).await;
        add_to_workspace(conversation_view.clone(), cx);
        cx.update(|window, _cx| window.refresh());
        cx.run_until_parked();

        conversation_view.read_with(cx, |view, _cx| {
            assert!(
                view.api_key_editor.is_none(),
                "キーを読まないエージェントに入力欄は要らない"
            );
        });
        // ADR-F55 で目印が増えた分もここに並べる: カードのどの部品も、キーを
        // 読まないエージェントには出ない。
        for selector in [
            API_KEY_INPUT_SELECTOR,
            API_KEY_SAVE_SELECTOR,
            API_KEY_SAVING_SELECTOR,
            API_KEY_CONFIGURED_SELECTOR,
            API_KEY_REMOVE_SELECTOR,
            API_KEY_CANCEL_SELECTOR,
            API_KEY_WINS_SELECTOR,
            FORK_AUTH_NOTICE_SELECTOR,
            LAUNCH_CLAUDE_CODE_SELECTOR,
        ] {
            assert!(
                cx.debug_bounds(selector).is_none(),
                "{selector} は ACP Claude のカードだけのもの"
            );
        }
    }

    /// ADR-F55 「やらないこと」: サインインの端末は「終わったか」を見るために
    /// だけ見られ、照合先は固定の文字列だけ。出力から何かを取り出す
    /// 実装に変わればこの形は保てない。
    #[test]
    fn a_sign_in_is_watched_for_fixed_strings_only() {
        let patterns = login_success_patterns(agent_servers::CLAUDE_SUBSCRIPTION_AUTH_METHOD);
        assert_eq!(patterns, ["Login successful", "Type your message"]);
        for pattern in &patterns {
            let upper = pattern.to_uppercase();
            assert!(
                !upper.contains("TOKEN")
                    && !upper.contains("KEY")
                    && !upper.contains("OAUTH")
                    && !pattern.contains('{'),
                "the watcher matches a fixed line, it does not capture one: {pattern}"
            );
        }
        assert!(
            login_success_patterns(CLAUDE_ACP_API_KEY_METHOD).is_empty(),
            "a key needs no terminal to watch"
        );
    }

    /// カードにキーを保存させ、「設定済み」の表示まで進める。
    async fn save_api_key(
        conversation_view: &Entity<ConversationView>,
        key: &str,
        cx: &mut VisualTestContext,
    ) {
        let editor = conversation_view.read_with(cx, |view, _cx| {
            view.api_key_editor.clone().expect("入力欄を持っている")
        });
        editor.update_in(cx, |editor, window, cx| {
            editor.set_text(key, window, cx);
        });
        conversation_view.update_in(cx, |view, window, cx| view.submit_api_key(window, cx));
        cx.run_until_parked();
        cx.update(|window, _cx| window.refresh());
        cx.run_until_parked();
    }

    /// ADR-F55: 「変更」を押してから保存せずにやめられる。やめたら「設定済み」の
    /// 表示へ戻り、入力欄に書きかけた値も保存済みのキーも残らない・変わらない。
    #[gpui::test]
    async fn the_acp_claude_card_cancels_an_edit(cx: &mut TestAppContext) {
        init_test(cx);
        const KEY: &str = "sk-ant-api03-kept";
        const TYPED: &str = "sk-ant-api03-half-typed";

        let connection = AuthGatedAgentConnection::with_methods(&[CLAUDE_ACP_API_KEY_METHOD]);
        let (conversation_view, cx) =
            setup_conversation_view(ClaudeAcpAgentServer { connection }, cx).await;
        add_to_workspace(conversation_view.clone(), cx);
        cx.update(|window, _cx| window.refresh());
        cx.run_until_parked();

        let fs = conversation_view.read_with(cx, |view, cx| view.project.read(cx).fs().clone());
        fs.create_dir(::paths::settings_file().parent().expect("設定ファイルの親"))
            .await
            .expect("設定ファイルの置き場を作れる");

        save_api_key(&conversation_view, KEY, cx).await;
        assert!(
            cx.debug_bounds(API_KEY_WINS_SELECTOR).is_none(),
            "サインインが出ていないカードで「キーを削除すればサブスクで使える」とは言わない"
        );
        assert!(
            cx.debug_bounds(API_KEY_CANCEL_SELECTOR).is_none(),
            "設定済みの表示中は取り消すものが無い"
        );

        conversation_view.update(cx, |view, cx| {
            view.api_key_editing = true;
            cx.notify();
        });
        let editor = conversation_view.read_with(cx, |view, _cx| {
            view.api_key_editor.clone().expect("入力欄を持っている")
        });
        editor.update_in(cx, |editor, window, cx| {
            editor.set_text(TYPED, window, cx);
        });
        cx.update(|window, _cx| window.refresh());
        cx.run_until_parked();

        let cancel = cx
            .debug_bounds(API_KEY_CANCEL_SELECTOR)
            .expect("変更中は「取り消し」が出る");
        cx.simulate_click(cancel.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        conversation_view.read_with(cx, |view, cx| {
            assert!(!view.api_key_editing, "設定済みの表示へ戻る");
            assert_eq!(
                view.api_key_editor
                    .as_ref()
                    .map(|editor| editor.read(cx).text(cx))
                    .unwrap_or_default(),
                "",
                "書きかけのキーは残さない"
            );
        });
        assert_eq!(
            saved_api_key(cx).as_deref(),
            Some(KEY),
            "取り消しても保存済みのキーはそのまま"
        );

        cx.update(|window, _cx| window.refresh());
        cx.run_until_parked();
        assert!(
            cx.debug_bounds(API_KEY_CONFIGURED_SELECTOR).is_some(),
            "「設定済み」の表示に戻る"
        );
        assert!(
            cx.debug_bounds(API_KEY_INPUT_SELECTOR).is_none(),
            "入力欄は消える"
        );
    }

    /// ADR-F55: 保存が走っているあいだは「保存中…」を出して押せなくする。
    /// 以前は見た目が変わらず、2 度目の押下が黙って捨てられていた。
    #[gpui::test]
    async fn the_acp_claude_card_says_while_it_is_saving(cx: &mut TestAppContext) {
        init_test(cx);

        let connection = AuthGatedAgentConnection::with_methods(&[CLAUDE_ACP_API_KEY_METHOD]);
        let (conversation_view, cx) =
            setup_conversation_view(ClaudeAcpAgentServer { connection }, cx).await;
        add_to_workspace(conversation_view.clone(), cx);
        cx.update(|window, _cx| window.refresh());
        cx.run_until_parked();

        assert!(
            cx.debug_bounds(API_KEY_SAVE_SELECTOR).is_some(),
            "何も走っていなければ「保存」"
        );
        assert!(cx.debug_bounds(API_KEY_SAVING_SELECTOR).is_none());

        // 終わらない書き込みを置いて、保存中のフレームを見る。
        conversation_view.update(cx, |view, cx| {
            view.api_key_save_task = Some(cx.spawn(async move |_this, _cx| {
                futures::future::pending::<()>().await;
            }));
            cx.notify();
        });
        cx.update(|window, _cx| window.refresh());
        cx.run_until_parked();

        assert!(
            cx.debug_bounds(API_KEY_SAVING_SELECTOR).is_some(),
            "保存中はそう言う"
        );
        assert!(
            cx.debug_bounds(API_KEY_SAVE_SELECTOR).is_none(),
            "押せる「保存」は出さない"
        );

        // その間にもう一度送っても、前の書き込みを踏みつぶさない。
        let editor = conversation_view.read_with(cx, |view, _cx| {
            view.api_key_editor.clone().expect("入力欄を持っている")
        });
        editor.update_in(cx, |editor, window, cx| {
            editor.set_text("sk-ant-api03-second-press", window, cx);
        });
        conversation_view.update_in(cx, |view, window, cx| view.submit_api_key(window, cx));
        cx.run_until_parked();
        assert_eq!(saved_api_key(cx), None, "2 度目は何も書かない");

        conversation_view.update(cx, |view, cx| {
            view.api_key_save_task = None;
            cx.notify();
        });
        cx.update(|window, _cx| window.refresh());
        cx.run_until_parked();
        assert!(
            cx.debug_bounds(API_KEY_SAVE_SELECTOR).is_some(),
            "終われば押せる「保存」に戻る"
        );
    }

    /// ADR-F55: 設定済みのカードの「キーを削除」で `env` から変数が外れ、
    /// カードは認証待ち (入力欄) に戻る。ほかの `env` は残る。
    #[gpui::test]
    async fn the_acp_claude_card_removes_a_saved_api_key(cx: &mut TestAppContext) {
        init_test(cx);
        const KEY: &str = "sk-ant-api03-to-be-removed";

        let connection = AuthGatedAgentConnection::with_methods(&[CLAUDE_ACP_API_KEY_METHOD]);
        let (conversation_view, cx) =
            setup_conversation_view(ClaudeAcpAgentServer { connection }, cx).await;
        add_to_workspace(conversation_view.clone(), cx);
        cx.update(|window, _cx| window.refresh());
        cx.run_until_parked();

        let fs = conversation_view.read_with(cx, |view, cx| view.project.read(cx).fs().clone());
        fs.create_dir(::paths::settings_file().parent().expect("設定ファイルの親"))
            .await
            .expect("設定ファイルの置き場を作れる");

        // 同じ `env` に別の変数を先に置いておき、削除がそれを巻き込まない
        // ことを確かめる。
        let other = cx.update(|_window, cx| {
            agent_servers::set_env_var(
                agent_servers::CLAUDE_AGENT_ID,
                "KEEP_ME",
                "1",
                fs.clone(),
                cx,
            )
            .expect("値が空ではない")
        });
        other.await.expect("書ける");
        cx.run_until_parked();

        let editor = conversation_view.read_with(cx, |view, _cx| {
            view.api_key_editor.clone().expect("入力欄を持っている")
        });
        editor.update_in(cx, |editor, window, cx| {
            editor.set_text(KEY, window, cx);
        });
        conversation_view.update_in(cx, |view, window, cx| view.submit_api_key(window, cx));
        cx.run_until_parked();
        assert_eq!(saved_api_key(cx).as_deref(), Some(KEY));

        cx.update(|window, _cx| window.refresh());
        cx.run_until_parked();
        let remove = cx
            .debug_bounds(API_KEY_REMOVE_SELECTOR)
            .expect("設定済みのカードに「キーを削除」がある");
        cx.simulate_click(remove.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        assert_eq!(saved_api_key(cx), None, "env から外れる");
        assert_eq!(
            saved_env_var(cx, "KEEP_ME").as_deref(),
            Some("1"),
            "ほかの env は残る"
        );
        conversation_view.read_with(cx, |view, _cx| {
            assert_eq!(view.api_key_error, None, "削除できたので理由は出ない");
        });

        cx.update(|window, _cx| window.refresh());
        cx.run_until_parked();
        assert!(
            cx.debug_bounds(API_KEY_CONFIGURED_SELECTOR).is_none(),
            "「設定済み」の表示は消える"
        );
        assert!(
            cx.debug_bounds(API_KEY_INPUT_SELECTOR).is_some(),
            "認証待ち (入力欄) に戻る"
        );
    }

    /// Simulates a model which always returns a refusal response
    #[derive(Clone)]
    struct RefusalAgentConnection;

    impl AgentConnection for RefusalAgentConnection {
        fn agent_id(&self) -> AgentId {
            AgentId::new("refusal")
        }

        fn telemetry_id(&self) -> SharedString {
            "refusal".into()
        }

        fn new_session(
            self: Rc<Self>,
            project: Entity<Project>,
            work_dirs: PathList,
            cx: &mut gpui::App,
        ) -> Task<gpui::Result<Entity<AcpThread>>> {
            Task::ready(Ok(cx.new(|cx| {
                let action_log = cx.new(|_| ActionLog::new(project.clone()));
                AcpThread::new(
                    None,
                    None,
                    Some(work_dirs),
                    self,
                    project,
                    action_log,
                    acp::SessionId::new("test"),
                    watch::Receiver::constant(
                        acp::PromptCapabilities::new()
                            .image(true)
                            .audio(true)
                            .embedded_context(true),
                    ),
                    cx,
                )
            })))
        }

        fn auth_methods(&self) -> &[acp::AuthMethod] {
            &[]
        }

        fn authenticate(
            &self,
            _method_id: acp::AuthMethodId,
            _cx: &mut App,
        ) -> Task<gpui::Result<()>> {
            unimplemented!()
        }

        fn prompt(
            &self,
            _params: acp::PromptRequest,
            _cx: &mut App,
        ) -> Task<gpui::Result<acp::PromptResponse>> {
            Task::ready(Ok(acp::PromptResponse::new(acp::StopReason::Refusal)))
        }

        fn cancel(&self, _session_id: &acp::SessionId, _cx: &mut App) {
            unimplemented!()
        }

        fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
            self
        }
    }

    #[derive(Clone)]
    struct CwdCapturingConnection {
        captured_work_dirs: Arc<Mutex<Option<PathList>>>,
    }

    impl CwdCapturingConnection {
        fn new() -> Self {
            Self {
                captured_work_dirs: Arc::new(Mutex::new(None)),
            }
        }
    }

    impl AgentConnection for CwdCapturingConnection {
        fn agent_id(&self) -> AgentId {
            AgentId::new("cwd-capturing")
        }

        fn telemetry_id(&self) -> SharedString {
            "cwd-capturing".into()
        }

        fn new_session(
            self: Rc<Self>,
            project: Entity<Project>,
            work_dirs: PathList,
            cx: &mut gpui::App,
        ) -> Task<gpui::Result<Entity<AcpThread>>> {
            *self.captured_work_dirs.lock() = Some(work_dirs.clone());
            let action_log = cx.new(|_| ActionLog::new(project.clone()));
            let thread = cx.new(|cx| {
                AcpThread::new(
                    None,
                    None,
                    Some(work_dirs),
                    self.clone(),
                    project,
                    action_log,
                    acp::SessionId::new("new-session"),
                    watch::Receiver::constant(
                        acp::PromptCapabilities::new()
                            .image(true)
                            .audio(true)
                            .embedded_context(true),
                    ),
                    cx,
                )
            });
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
            _title: Option<SharedString>,
            cx: &mut App,
        ) -> Task<gpui::Result<Entity<AcpThread>>> {
            *self.captured_work_dirs.lock() = Some(work_dirs.clone());
            let action_log = cx.new(|_| ActionLog::new(project.clone()));
            let thread = cx.new(|cx| {
                AcpThread::new(
                    None,
                    None,
                    Some(work_dirs),
                    self.clone(),
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
            });
            Task::ready(Ok(thread))
        }

        fn auth_methods(&self) -> &[acp::AuthMethod] {
            &[]
        }

        fn authenticate(
            &self,
            _method_id: acp::AuthMethodId,
            _cx: &mut App,
        ) -> Task<gpui::Result<()>> {
            Task::ready(Ok(()))
        }

        fn prompt(
            &self,
            _params: acp::PromptRequest,
            _cx: &mut App,
        ) -> Task<gpui::Result<acp::PromptResponse>> {
            Task::ready(Ok(acp::PromptResponse::new(acp::StopReason::EndTurn)))
        }

        fn cancel(&self, _session_id: &acp::SessionId, _cx: &mut App) {}

        fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
            self
        }
    }

    pub(crate) fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            // Use an isolated DB so parallel tests can't overwrite each
            // other's global keys (e.g. the last-created entry kind).
            cx.set_global(db::AppDatabase::test_new());
            ThreadMetadataStore::init_global(cx);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            agent_panel::init(cx);
            release_channel::init(semver::Version::new(0, 0, 0), cx);
            prompt_store::init(cx)
        });
    }

    /// ADR-F49 決定 24: when the channel that carries `getDiagnostics` into
    /// the sandbox could not be brought up, the thread says so — before, only
    /// the log knew, and the agent answered as if the project had no
    /// diagnostics.
    #[gpui::test]
    async fn a_thread_says_when_its_agent_was_given_no_ide_diagnostics(cx: &mut TestAppContext) {
        init_test(cx);
        let reason = "WSL が 60 秒以内に応答しませんでした。";
        let (conversation_view, cx) = setup_conversation_view(
            StubAgentServer::new(StubAgentConnection::new().with_sandbox_ide_unavailable(reason)),
            cx,
        )
        .await;

        let thread = active_thread(&conversation_view, cx);
        thread.read_with(cx, |view, cx| {
            assert_eq!(view.sandbox_ide_notice_reason(cx).as_deref(), Some(reason));
        });

        // Dismissed, it stays dismissed for this thread: the reason belongs to
        // the connection and would otherwise come back on every render.
        thread.update(cx, |view, _| {
            view.sandbox_ide_notice_dismissed = true;
        });
        thread.read_with(cx, |view, cx| {
            assert!(view.sandbox_ide_notice_reason(cx).is_none());
        });
    }

    /// An agent that has the tool — or was never meant to have one — says
    /// nothing.
    #[gpui::test]
    async fn a_thread_with_nothing_lost_shows_no_ide_notice(cx: &mut TestAppContext) {
        init_test(cx);
        let (conversation_view, cx) =
            setup_conversation_view(StubAgentServer::new(StubAgentConnection::new()), cx).await;

        active_thread(&conversation_view, cx).read_with(cx, |view, cx| {
            assert!(view.sandbox_ide_notice_reason(cx).is_none());
        });
    }

    fn active_thread(
        conversation_view: &Entity<ConversationView>,
        cx: &TestAppContext,
    ) -> Entity<ThreadView> {
        cx.read(|cx| {
            conversation_view
                .read(cx)
                .active_thread()
                .expect("No active thread")
                .clone()
        })
    }

    fn assert_thread_list_item_count_matches_entries(view: &ThreadView, cx: &App) {
        assert_eq!(
            view.list_state.item_count(),
            view.thread.read(cx).entries().len() + usize::from(view.generating_indicator_in_list)
        );
    }

    fn message_editor(
        conversation_view: &Entity<ConversationView>,
        cx: &TestAppContext,
    ) -> Entity<MessageEditor> {
        let thread = active_thread(conversation_view, cx);
        cx.read(|cx| thread.read(cx).message_editor.clone())
    }

    #[gpui::test]
    async fn test_rewind_views(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/project",
            json!({
                "test1.txt": "old content 1",
                "test2.txt": "old content 2"
            }),
        )
        .await;
        let project = Project::test(fs, [Path::new("/project")], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        let thread_store = cx.update(|_window, cx| cx.new(|cx| ThreadStore::new(cx)));
        let connection_store =
            cx.update(|_window, cx| cx.new(|cx| AgentConnectionStore::new(project.clone(), cx)));

        let connection = Rc::new(StubAgentConnection::new());
        let conversation_view = cx.update(|window, cx| {
            cx.new(|cx| {
                ConversationView::new(
                    Rc::new(StubAgentServer::new(connection.as_ref().clone())),
                    connection_store,
                    Agent::Custom { id: "Test".into() },
                    None,
                    None,
                    None,
                    None,
                    None,
                    workspace.downgrade(),
                    project.clone(),
                    Some(thread_store.clone()),
                    AgentThreadSource::AgentPanel,
                    window,
                    cx,
                )
            })
        });

        cx.run_until_parked();

        let thread = conversation_view
            .read_with(cx, |view, cx| {
                view.active_thread().map(|r| r.read(cx).thread.clone())
            })
            .unwrap();

        // First user message
        connection.set_next_prompt_updates(vec![acp::SessionUpdate::ToolCall(
            acp::ToolCall::new("tool1", "Edit file 1")
                .kind(acp::ToolKind::Edit)
                .status(acp::ToolCallStatus::Completed)
                .content(vec![acp::ToolCallContent::Diff(
                    acp::Diff::new("/project/test1.txt", "new content 1").old_text("old content 1"),
                )]),
        )]);

        thread
            .update(cx, |thread, cx| thread.send_raw("Give me a diff", cx))
            .await
            .unwrap();
        cx.run_until_parked();

        thread.read_with(cx, |thread, _cx| {
            assert_eq!(thread.entries().len(), 2);
        });

        conversation_view.read_with(cx, |view, cx| {
            let entry_view_state = view
                .active_thread()
                .map(|active| active.read(cx).entry_view_state.clone())
                .unwrap();
            entry_view_state.read_with(cx, |entry_view_state, _| {
                assert!(
                    entry_view_state
                        .entry(0)
                        .unwrap()
                        .message_editor()
                        .is_some()
                );
                assert!(entry_view_state.entry(1).unwrap().has_content());
            });
        });

        // Second user message
        connection.set_next_prompt_updates(vec![acp::SessionUpdate::ToolCall(
            acp::ToolCall::new("tool2", "Edit file 2")
                .kind(acp::ToolKind::Edit)
                .status(acp::ToolCallStatus::Completed)
                .content(vec![acp::ToolCallContent::Diff(
                    acp::Diff::new("/project/test2.txt", "new content 2").old_text("old content 2"),
                )]),
        )]);

        thread
            .update(cx, |thread, cx| thread.send_raw("Another one", cx))
            .await
            .unwrap();
        cx.run_until_parked();

        let second_user_message_id = thread.read_with(cx, |thread, _| {
            assert_eq!(thread.entries().len(), 4);
            let AgentThreadEntry::UserMessage(user_message) = &thread.entries()[2] else {
                panic!();
            };
            user_message.client_id.clone().unwrap()
        });

        conversation_view.read_with(cx, |view, cx| {
            let entry_view_state = view
                .active_thread()
                .unwrap()
                .read(cx)
                .entry_view_state
                .clone();
            entry_view_state.read_with(cx, |entry_view_state, _| {
                assert!(
                    entry_view_state
                        .entry(0)
                        .unwrap()
                        .message_editor()
                        .is_some()
                );
                assert!(entry_view_state.entry(1).unwrap().has_content());
                assert!(
                    entry_view_state
                        .entry(2)
                        .unwrap()
                        .message_editor()
                        .is_some()
                );
                assert!(entry_view_state.entry(3).unwrap().has_content());
            });
        });

        // Rewind to first message
        thread
            .update(cx, |thread, cx| thread.rewind(second_user_message_id, cx))
            .await
            .unwrap();

        cx.run_until_parked();

        thread.read_with(cx, |thread, _| {
            assert_eq!(thread.entries().len(), 2);
        });

        conversation_view.read_with(cx, |view, cx| {
            let active = view.active_thread().unwrap();
            active
                .read(cx)
                .entry_view_state
                .read_with(cx, |entry_view_state, _| {
                    assert!(
                        entry_view_state
                            .entry(0)
                            .unwrap()
                            .message_editor()
                            .is_some()
                    );
                    assert!(entry_view_state.entry(1).unwrap().has_content());

                    // Old views should be dropped
                    assert!(entry_view_state.entry(2).is_none());
                    assert!(entry_view_state.entry(3).is_none());
                });
        });
    }

    #[gpui::test]
    async fn test_regenerate_keeps_pending_subagent_edits(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/project",
            json!({
                "file.txt": "original content"
            }),
        )
        .await;
        let project = Project::test(fs, [Path::new("/project")], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        let thread_store = cx.update(|_window, cx| cx.new(|cx| ThreadStore::new(cx)));
        let connection_store =
            cx.update(|_window, cx| cx.new(|cx| AgentConnectionStore::new(project.clone(), cx)));

        let connection = Rc::new(StubAgentConnection::new());
        let conversation_view = cx.update(|window, cx| {
            cx.new(|cx| {
                ConversationView::new(
                    Rc::new(StubAgentServer::new(connection.as_ref().clone())),
                    connection_store,
                    Agent::Custom { id: "Test".into() },
                    None,
                    None,
                    None,
                    None,
                    None,
                    workspace.downgrade(),
                    project.clone(),
                    Some(thread_store.clone()),
                    AgentThreadSource::AgentPanel,
                    window,
                    cx,
                )
            })
        });

        cx.run_until_parked();

        let thread = conversation_view
            .read_with(cx, |view, cx| {
                view.active_thread().map(|r| r.read(cx).thread.clone())
            })
            .unwrap();

        // First turn: a subagent tool call. Subagent edits never appear as
        // diffs in the parent thread's entries; they are only forwarded to the
        // parent's action log through the linked-log mechanism.
        connection.set_next_prompt_updates(vec![acp::SessionUpdate::ToolCall(
            acp::ToolCall::new("spawn1", "Subagent task")
                .kind(acp::ToolKind::Other)
                .status(acp::ToolCallStatus::Completed)
                .meta(acp_thread::meta_with_tool_name("spawn_agent")),
        )]);

        thread
            .update(cx, |thread, cx| thread.send_raw("Use a subagent", cx))
            .await
            .unwrap();
        cx.run_until_parked();

        // Simulate the subagent editing a file: edits performed through a
        // child action log are forwarded to the parent thread's action log,
        // just like `Thread::new_subagent` wires it up.
        let parent_action_log = thread.read_with(cx, |thread, _| thread.action_log().clone());
        let subagent_action_log = cx.update(|_, cx| {
            cx.new(|_| {
                ActionLog::new(project.clone()).with_linked_action_log(parent_action_log.clone())
            })
        });

        let buffer = project
            .update(cx, |project, cx| {
                let path = project.find_project_path("file.txt", cx).unwrap();
                project.open_buffer(path, cx)
            })
            .await
            .unwrap();
        cx.update(|_, cx| {
            subagent_action_log.update(cx, |log, cx| log.buffer_read(buffer.clone(), cx));
            buffer.update(cx, |buffer, cx| {
                buffer.set_text("edited by subagent", cx);
            });
            subagent_action_log.update(cx, |log, cx| log.buffer_edited(buffer.clone(), cx));
        });
        cx.run_until_parked();

        parent_action_log.read_with(cx, |log, cx| {
            assert_eq!(
                log.changed_buffers(cx).count(),
                1,
                "the subagent edit should be pending review in the parent's action log"
            );
        });

        // Second turn: a plain follow-up.
        connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
            acp::ContentChunk::new("Response".into()),
        )]);
        thread
            .update(cx, |thread, cx| thread.send_raw("Follow-up", cx))
            .await
            .unwrap();
        cx.run_until_parked();

        let follow_up_ix = thread.read_with(cx, |thread, cx| {
            thread
                .entries()
                .iter()
                .position(|entry| entry.to_markdown(cx) == "## User\n\nFollow-up\n\n")
                .unwrap()
        });

        // Edit and regenerate the follow-up message.
        let user_message_editor = conversation_view.read_with(cx, |view, cx| {
            view.active_thread()
                .unwrap()
                .read(cx)
                .entry_view_state
                .read(cx)
                .entry(follow_up_ix)
                .unwrap()
                .message_editor()
                .unwrap()
                .clone()
        });
        user_message_editor.update_in(cx, |editor, window, cx| {
            editor.set_text("Edited follow-up", window, cx);
        });

        connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
            acp::ContentChunk::new("New response".into()),
        )]);
        active_thread(&conversation_view, cx).update_in(cx, |view, window, cx| {
            view.regenerate(follow_up_ix, user_message_editor.clone(), window, cx);
        });
        cx.run_until_parked();

        // The thread should have been rewound and the edited message resent.
        thread.read_with(cx, |thread, cx| {
            let entries = thread.entries();
            assert_eq!(entries.len(), 4);
            assert_eq!(
                entries[2].to_markdown(cx),
                "## User\n\nEdited follow-up\n\n"
            );
        });

        // The subagent's edits predate the regenerated prompt, so they must be
        // auto-kept rather than rejected by the rewind.
        buffer.read_with(cx, |buffer, _| {
            assert_eq!(
                buffer.text(),
                "edited by subagent",
                "pending subagent edits should be kept when regenerating a later prompt"
            );
        });
        parent_action_log.read_with(cx, |log, cx| {
            assert_eq!(
                log.changed_buffers(cx).count(),
                0,
                "the subagent edit should have been auto-kept"
            );
        });
    }

    #[gpui::test]
    async fn test_scroll_to_most_recent_user_prompt(cx: &mut TestAppContext) {
        init_test(cx);

        let connection = StubAgentConnection::new();

        // Each user prompt will result in a user message entry plus an agent message entry.
        connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
            acp::ContentChunk::new("Response 1".into()),
        )]);

        let (conversation_view, cx) =
            setup_conversation_view(StubAgentServer::new(connection.clone()), cx).await;

        let thread = conversation_view
            .read_with(cx, |view, cx| {
                view.active_thread().map(|r| r.read(cx).thread.clone())
            })
            .unwrap();

        thread
            .update(cx, |thread, cx| thread.send_raw("Prompt 1", cx))
            .await
            .unwrap();
        cx.run_until_parked();

        connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
            acp::ContentChunk::new("Response 2".into()),
        )]);

        thread
            .update(cx, |thread, cx| thread.send_raw("Prompt 2", cx))
            .await
            .unwrap();
        cx.run_until_parked();

        // Move somewhere else first so we're not trivially already on the last user prompt.
        active_thread(&conversation_view, cx).update(cx, |view, cx| {
            view.scroll_to_top(cx);
        });
        cx.run_until_parked();

        active_thread(&conversation_view, cx).update(cx, |view, cx| {
            view.scroll_to_user_message_index(None, cx);
            let scroll_top = view.list_state.logical_scroll_top();
            // Entries layout is: [User1, Assistant1, User2, Assistant2]
            assert_eq!(scroll_top.item_ix, 2);

            view.scroll_to_top(cx);
            view.scroll_to_user_message_index(Some(0), cx);
            let scroll_top = view.list_state.logical_scroll_top();
            assert_eq!(scroll_top.item_ix, 0);

            view.scroll_to_top(cx);
            view.scroll_to_user_message_index(Some(2), cx);
            let scroll_top = view.list_state.logical_scroll_top();
            assert_eq!(scroll_top.item_ix, 2);
        });
    }

    #[gpui::test]
    async fn test_scroll_to_most_recent_user_prompt_falls_back_to_bottom_without_user_messages(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);

        let (conversation_view, cx) =
            setup_conversation_view(StubAgentServer::default_response(), cx).await;

        // With no entries, scrolling should be a no-op and must not panic.
        active_thread(&conversation_view, cx).update(cx, |view, cx| {
            view.scroll_to_user_message_index(None, cx);
            let scroll_top = view.list_state.logical_scroll_top();
            assert_eq!(scroll_top.item_ix, 0);
        });
    }

    #[gpui::test]
    async fn test_thread_search_finds_matches_across_entries(cx: &mut TestAppContext) {
        init_test(cx);

        let connection = StubAgentConnection::new();
        connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
            acp::ContentChunk::new(
                "Yes, you can substitute banana for plantain in this recipe.".into(),
            ),
        )]);

        let (conversation_view, cx) =
            setup_conversation_view(StubAgentServer::new(connection.clone()), cx).await;

        let thread =
            active_thread(&conversation_view, cx).read_with(cx, |view, _| view.thread.clone());

        thread
            .update(cx, |thread, cx| {
                thread.send_raw("Can I use banana here?", cx)
            })
            .await
            .unwrap();
        cx.run_until_parked();

        connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
            acp::ContentChunk::new(
                "Banana yogurt also works as a topping; whisk the banana smooth first.".into(),
            ),
        )]);

        thread
            .update(cx, |thread, cx| {
                thread.send_raw("What about as a topping?", cx)
            })
            .await
            .unwrap();
        cx.run_until_parked();

        let thread_view = active_thread(&conversation_view, cx);
        thread_view.update_in(cx, |view, window, cx| {
            view.toggle_search(&crate::ToggleSearch, window, cx);
        });
        cx.run_until_parked();

        let bar = thread_view
            .read_with(cx, |view, _| view.thread_search_bar.clone())
            .expect("thread_search_bar should be set after toggle_search");
        bar.update_in(cx, |bar, window, cx| {
            bar.query_editor.update(cx, |editor, cx| {
                editor.set_text("banana", window, cx);
            });
            bar.update_matches(window, cx);
        });
        cx.run_until_parked();

        let (match_count, active_text) =
            bar.read_with(cx, |bar, cx| (bar.match_count(), bar.active_match_text(cx)));
        assert_eq!(
            match_count, 4,
            "expected 4 matches for case-insensitive 'banana'"
        );
        assert_eq!(active_text.as_deref(), Some("1/4"));

        thread_view.read_with(cx, |view, _| {
            assert_eq!(view.list_state.logical_scroll_top().item_ix, 0);
        });

        bar.update_in(cx, |bar, window, cx| {
            bar.select_next_match(&super::thread_search_bar::SelectNextThreadMatch, window, cx);
        });
        cx.run_until_parked();
        let active_text_2 = bar.read_with(cx, |bar, cx| bar.active_match_text(cx));
        assert_eq!(active_text_2.as_deref(), Some("2/4"));

        bar.update_in(cx, |bar, window, cx| {
            bar.select_prev_match(
                &super::thread_search_bar::SelectPreviousThreadMatch,
                window,
                cx,
            );
        });
        let active_text_3 = bar.read_with(cx, |bar, cx| bar.active_match_text(cx));
        assert_eq!(active_text_3.as_deref(), Some("1/4"));

        bar.update_in(cx, |bar, window, cx| {
            bar.query_editor.update(cx, |editor, cx| {
                editor.set_text("apple", window, cx);
            });
            bar.update_matches(window, cx);
        });
        cx.run_until_parked();
        let (match_count_apple, active_text_apple) =
            bar.read_with(cx, |bar, cx| (bar.match_count(), bar.active_match_text(cx)));
        assert_eq!(match_count_apple, 0);
        assert_eq!(active_text_apple.as_deref(), Some("0/0"));
    }

    #[gpui::test]
    async fn test_thread_search_includes_expanded_thinking_blocks(cx: &mut TestAppContext) {
        init_test(cx);

        let connection = StubAgentConnection::new();
        connection.set_next_prompt_updates(vec![
            acp::SessionUpdate::AgentThoughtChunk(acp::ContentChunk::new(
                "Hidden papaya reasoning.".into(),
            )),
            acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                "Final answer without that fruit.".into(),
            )),
        ]);

        let (conversation_view, cx) =
            setup_conversation_view(StubAgentServer::new(connection.clone()), cx).await;

        let thread =
            active_thread(&conversation_view, cx).read_with(cx, |view, _| view.thread.clone());
        thread
            .update(cx, |thread, cx| thread.send_raw("Think this through", cx))
            .await
            .unwrap();
        cx.run_until_parked();

        let (assistant_entry_ix, thought_chunk_ix) = thread.read_with(cx, |thread, _| {
            thread
                .entries()
                .iter()
                .enumerate()
                .find_map(|(entry_ix, entry)| match entry {
                    AgentThreadEntry::AssistantMessage(message) => message
                        .chunks
                        .iter()
                        .position(|chunk| matches!(chunk, AssistantMessageChunk::Thought { .. }))
                        .map(|chunk_ix| (entry_ix, chunk_ix)),
                    _ => None,
                })
                .expect("assistant thought chunk should exist")
        });

        let thread_view = active_thread(&conversation_view, cx);
        thread_view.update_in(cx, |view, window, cx| {
            view.toggle_search(&crate::ToggleSearch, window, cx);
        });
        cx.run_until_parked();

        let bar = thread_view
            .read_with(cx, |view, _| view.thread_search_bar.clone())
            .expect("thread_search_bar should be set after toggle_search");
        bar.update_in(cx, |bar, window, cx| {
            bar.query_editor.update(cx, |editor, cx| {
                editor.set_text("papaya", window, cx);
            });
            bar.update_matches(window, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            bar.read_with(cx, |bar, _| bar.match_count()),
            0,
            "collapsed thinking content should not be searched",
        );

        thread_view.update(cx, |view, cx| {
            view.entry_view_state.update(cx, |state, cx| {
                state.toggle_thinking_block_expansion((assistant_entry_ix, thought_chunk_ix), cx);
            });
        });
        bar.update_in(cx, |bar, window, cx| {
            bar.update_matches(window, cx);
        });
        cx.run_until_parked();

        assert_eq!(
            bar.read_with(cx, |bar, _| bar.match_count()),
            1,
            "expanded thinking content should be searchable",
        );
    }

    #[gpui::test]
    async fn test_thread_search_includes_expanded_tool_call_content(cx: &mut TestAppContext) {
        init_test(cx);

        let tool_call_id = acp::ToolCallId::new("search-tool-content");
        let connection = StubAgentConnection::new();
        connection.set_next_prompt_updates(vec![acp::SessionUpdate::ToolCall(
            acp::ToolCall::new(tool_call_id.clone(), "Inspect output")
                .kind(acp::ToolKind::Other)
                .status(acp::ToolCallStatus::Completed)
                .content(vec!["Tool output mentions papaya once.".into()]),
        )]);

        let (conversation_view, cx) =
            setup_conversation_view(StubAgentServer::new(connection.clone()), cx).await;

        let thread =
            active_thread(&conversation_view, cx).read_with(cx, |view, _| view.thread.clone());
        thread
            .update(cx, |thread, cx| thread.send_raw("Run the tool", cx))
            .await
            .unwrap();
        cx.run_until_parked();

        let thread_view = active_thread(&conversation_view, cx);
        thread_view.update_in(cx, |view, window, cx| {
            view.toggle_search(&crate::ToggleSearch, window, cx);
        });
        cx.run_until_parked();

        let bar = thread_view
            .read_with(cx, |view, _| view.thread_search_bar.clone())
            .expect("thread_search_bar should be set after toggle_search");
        bar.update_in(cx, |bar, window, cx| {
            bar.query_editor.update(cx, |editor, cx| {
                editor.set_text("papaya", window, cx);
            });
            bar.update_matches(window, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            bar.read_with(cx, |bar, _| bar.match_count()),
            0,
            "collapsed tool-call content should not be searched",
        );

        thread_view.update(cx, |view, cx| {
            view.entry_view_state.update(cx, |state, _cx| {
                state.expand_tool_call(tool_call_id);
            });
        });
        bar.update_in(cx, |bar, window, cx| {
            bar.update_matches(window, cx);
        });
        cx.run_until_parked();

        assert_eq!(
            bar.read_with(cx, |bar, _| bar.match_count()),
            1,
            "expanded tool-call content should be searchable",
        );
    }

    #[gpui::test]
    async fn test_thread_search_scrolls_to_later_user_message_match(cx: &mut TestAppContext) {
        init_test(cx);

        let connection = StubAgentConnection::new();
        connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
            acp::ContentChunk::new("First reply, no fruit here.".into()),
        )]);

        let (conversation_view, cx) =
            setup_conversation_view(StubAgentServer::new(connection.clone()), cx).await;
        add_to_workspace(conversation_view.clone(), cx);

        let thread =
            active_thread(&conversation_view, cx).read_with(cx, |view, _| view.thread.clone());
        thread
            .update(cx, |thread, cx| thread.send_raw("First question", cx))
            .await
            .unwrap();
        cx.run_until_parked();
        connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
            acp::ContentChunk::new("Second reply, still no fruit.".into()),
        )]);
        thread
            .update(cx, |thread, cx| thread.send_raw("Where is the papaya?", cx))
            .await
            .unwrap();
        cx.run_until_parked();

        let thread_view = active_thread(&conversation_view, cx);
        let papaya_entry_ix = thread.read_with(cx, |thread, _| {
            thread
                .entries()
                .iter()
                .rposition(|entry| matches!(entry, AgentThreadEntry::UserMessage(_)))
                .expect("a user message entry should exist")
        });

        thread_view.update_in(cx, |view, window, cx| {
            view.toggle_search(&crate::ToggleSearch, window, cx);
        });
        cx.run_until_parked();
        let bar = thread_view
            .read_with(cx, |view, _| view.thread_search_bar.clone())
            .expect("thread_search_bar should be set after toggle_search");
        bar.update_in(cx, |bar, window, cx| {
            bar.query_editor.update(cx, |editor, cx| {
                editor.set_text("papaya", window, cx);
            });
            bar.update_matches(window, cx);
        });
        cx.run_until_parked();

        bar.read_with(cx, |bar, _| {
            assert_eq!(
                bar.match_count(),
                1,
                "only the second user message matches 'papaya'"
            );
        });

        thread_view.read_with(cx, |view, _| {
            assert_eq!(
                view.list_state.logical_scroll_top().item_ix,
                papaya_entry_ix,
                "list should scroll to the user-message entry that owns the match",
            );
        });
    }

    /// Passive rescans (streaming updates, unrelated expansion toggles, query
    /// refinement) must not yank the list back to the active match.
    #[gpui::test]
    async fn test_thread_search_passive_rescan_preserves_scroll(cx: &mut TestAppContext) {
        init_test(cx);

        let connection = StubAgentConnection::new();
        connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
            acp::ContentChunk::new("First reply, no fruit here.".into()),
        )]);

        let (conversation_view, cx) =
            setup_conversation_view(StubAgentServer::new(connection.clone()), cx).await;
        add_to_workspace(conversation_view.clone(), cx);

        let thread =
            active_thread(&conversation_view, cx).read_with(cx, |view, _| view.thread.clone());
        thread
            .update(cx, |thread, cx| thread.send_raw("First question", cx))
            .await
            .unwrap();
        cx.run_until_parked();
        connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
            acp::ContentChunk::new("Second reply, still no fruit.".into()),
        )]);
        thread
            .update(cx, |thread, cx| thread.send_raw("Where is the papaya?", cx))
            .await
            .unwrap();
        cx.run_until_parked();

        let thread_view = active_thread(&conversation_view, cx);
        let papaya_entry_ix = thread.read_with(cx, |thread, _| {
            thread
                .entries()
                .iter()
                .rposition(|entry| matches!(entry, AgentThreadEntry::UserMessage(_)))
                .expect("a user message entry should exist")
        });

        thread_view.update_in(cx, |view, window, cx| {
            view.toggle_search(&crate::ToggleSearch, window, cx);
        });
        cx.run_until_parked();
        let bar = thread_view
            .read_with(cx, |view, _| view.thread_search_bar.clone())
            .expect("thread_search_bar should be set after toggle_search");
        bar.update_in(cx, |bar, window, cx| {
            bar.query_editor.update(cx, |editor, cx| {
                editor.set_text("papaya", window, cx);
            });
            bar.update_matches(window, cx);
        });
        cx.run_until_parked();

        // The initial query activates (and scrolls to) the only match.
        thread_view.read_with(cx, |view, _| {
            assert_eq!(
                view.list_state.logical_scroll_top().item_ix,
                papaya_entry_ix,
            );
        });

        // Simulate the user scrolling elsewhere, then a passive rescan re-running
        // the matcher while the same hit stays active.
        thread_view.update(cx, |view, _| {
            view.list_state.scroll_to(gpui::ListOffset {
                item_ix: 0,
                offset_in_item: gpui::px(0.),
            });
        });
        bar.update_in(cx, |bar, window, cx| {
            bar.update_matches(window, cx);
        });
        cx.run_until_parked();

        thread_view.read_with(cx, |view, _| {
            assert_eq!(
                view.list_state.logical_scroll_top().item_ix,
                0,
                "a passive rescan must not scroll back to the active match",
            );
        });
    }

    #[gpui::test]
    async fn test_thread_search_dismiss_clears_highlights(cx: &mut TestAppContext) {
        init_test(cx);

        let connection = StubAgentConnection::new();
        connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
            acp::ContentChunk::new("Mango is a tropical fruit.".into()),
        )]);

        let (conversation_view, cx) =
            setup_conversation_view(StubAgentServer::new(connection.clone()), cx).await;

        let thread =
            active_thread(&conversation_view, cx).read_with(cx, |view, _| view.thread.clone());

        thread
            .update(cx, |thread, cx| thread.send_raw("Tell me about mango", cx))
            .await
            .unwrap();
        cx.run_until_parked();

        let thread_view = active_thread(&conversation_view, cx);
        thread_view.update_in(cx, |view, window, cx| {
            view.toggle_search(&crate::ToggleSearch, window, cx);
        });
        cx.run_until_parked();

        let bar = thread_view
            .read_with(cx, |view, _| view.thread_search_bar.clone())
            .unwrap();
        bar.update_in(cx, |bar, window, cx| {
            bar.query_editor.update(cx, |editor, cx| {
                editor.set_text("mango", window, cx);
            });
            bar.update_matches(window, cx);
        });
        cx.run_until_parked();

        let entries = thread.read_with(cx, |thread, _| thread.entries().len());
        assert!(entries > 0);

        bar.update(cx, |bar, cx| bar.clear_highlights(cx));
        cx.run_until_parked();

        bar.read_with(cx, |bar, _| {
            assert_eq!(bar.match_count(), 0);
            assert!(bar.active_match_index().is_none());
        });
    }

    #[gpui::test]
    async fn test_thread_search_release_clears_markdown_highlights(cx: &mut TestAppContext) {
        init_test(cx);

        let connection = StubAgentConnection::new();
        connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
            acp::ContentChunk::new("Mango is a tropical fruit.".into()),
        )]);

        let (conversation_view, cx) =
            setup_conversation_view(StubAgentServer::new(connection.clone()), cx).await;

        let thread =
            active_thread(&conversation_view, cx).read_with(cx, |view, _| view.thread.clone());
        thread
            .update(cx, |thread, cx| thread.send_raw("Tell me about mango", cx))
            .await
            .unwrap();
        cx.run_until_parked();

        let assistant_markdown = thread.read_with(cx, |thread, _| {
            thread
                .entries()
                .iter()
                .find_map(|entry| match entry {
                    AgentThreadEntry::AssistantMessage(message) => {
                        message.chunks.iter().find_map(|chunk| match chunk {
                            AssistantMessageChunk::Message { block, .. } => {
                                block.markdown().cloned()
                            }
                            AssistantMessageChunk::Thought { .. } => None,
                        })
                    }
                    _ => None,
                })
                .expect("assistant message should have markdown")
        });

        let entry_view_state = active_thread(&conversation_view, cx)
            .read_with(cx, |view, _| view.entry_view_state.clone());
        let on_activate_match: Arc<dyn Fn(usize, &mut Window, &mut App)> = Arc::new(|_, _, _| {});
        let bar = cx.update(|window, cx| {
            cx.new(|cx| {
                super::thread_search_bar::ThreadSearchBar::new(
                    thread.clone(),
                    entry_view_state,
                    on_activate_match,
                    window,
                    cx,
                )
            })
        });

        bar.update_in(cx, |bar, window, cx| {
            bar.query_editor.update(cx, |editor, cx| {
                editor.set_text("mango", window, cx);
            });
            bar.update_matches(window, cx);
        });
        cx.run_until_parked();

        assert!(
            assistant_markdown
                .read_with(cx, |markdown, _| !markdown.search_highlights().is_empty()),
            "search should have highlighted the assistant markdown before release",
        );

        drop(bar);
        cx.update(|_, _| {});
        cx.run_until_parked();

        assert!(
            assistant_markdown.read_with(cx, |markdown, _| markdown.search_highlights().is_empty()),
            "releasing the search bar should clear retained markdown highlights",
        );
    }

    #[gpui::test]
    async fn test_thread_search_refreshes_on_new_thread_entry(cx: &mut TestAppContext) {
        init_test(cx);

        let connection = StubAgentConnection::new();
        connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
            acp::ContentChunk::new("First reply mentions banana once.".into()),
        )]);

        let (conversation_view, cx) =
            setup_conversation_view(StubAgentServer::new(connection.clone()), cx).await;

        let thread =
            active_thread(&conversation_view, cx).read_with(cx, |view, _| view.thread.clone());
        thread
            .update(cx, |thread, cx| thread.send_raw("Tell me about banana", cx))
            .await
            .unwrap();
        cx.run_until_parked();

        let thread_view = active_thread(&conversation_view, cx);
        thread_view.update_in(cx, |view, window, cx| {
            view.toggle_search(&crate::ToggleSearch, window, cx);
        });
        cx.run_until_parked();

        let bar = thread_view
            .read_with(cx, |view, _| view.thread_search_bar.clone())
            .expect("thread_search_bar should be set after toggle_search");
        bar.update_in(cx, |bar, window, cx| {
            bar.query_editor.update(cx, |editor, cx| {
                editor.set_text("banana", window, cx);
            });
            bar.update_matches(window, cx);
        });
        cx.run_until_parked();

        let count_before = bar.read_with(cx, |bar, _| bar.match_count());
        assert!(
            count_before >= 2,
            "expected at least two initial matches, got {count_before}",
        );

        bar.update_in(cx, |bar, window, cx| {
            bar.select_next_match(&super::thread_search_bar::SelectNextThreadMatch, window, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            bar.read_with(cx, |bar, _| bar.active_match_index()),
            Some(1),
            "setup precondition: second match should be active before the refresh",
        );

        // Advance past the debounced thread-update rescan.
        connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
            acp::ContentChunk::new("Banana banana: two more banana hits here.".into()),
        )]);
        thread
            .update(cx, |thread, cx| thread.send_raw("More banana please", cx))
            .await
            .unwrap();
        cx.run_until_parked();
        cx.executor()
            .advance_clock(super::thread_search_bar::SEARCH_UPDATE_DEBOUNCE * 2);
        cx.run_until_parked();

        let (count_after, active_after) =
            bar.read_with(cx, |bar, _| (bar.match_count(), bar.active_match_index()));
        assert!(
            count_after > count_before,
            "thread subscription should refresh matches after new content \
             streamed in: before={count_before}, after={count_after}",
        );
        assert_eq!(
            active_after,
            Some(1),
            "refreshing matches should preserve the active result when it still exists",
        );
    }

    /// Regression test for re-entering `ThreadView` during search navigation.
    #[gpui::test]
    async fn test_thread_search_select_next_from_thread_view_update_does_not_panic(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);

        let connection = StubAgentConnection::new();
        connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
            acp::ContentChunk::new(
                "Banana banana banana, the banana fits the banana bread.".into(),
            ),
        )]);

        let (conversation_view, cx) =
            setup_conversation_view(StubAgentServer::new(connection.clone()), cx).await;

        let thread =
            active_thread(&conversation_view, cx).read_with(cx, |view, _| view.thread.clone());
        thread
            .update(cx, |thread, cx| thread.send_raw("Need banana help", cx))
            .await
            .unwrap();
        cx.run_until_parked();

        let thread_view = active_thread(&conversation_view, cx);
        thread_view.update_in(cx, |view, window, cx| {
            view.toggle_search(&crate::ToggleSearch, window, cx);
        });
        cx.run_until_parked();

        let bar = thread_view
            .read_with(cx, |view, _| view.thread_search_bar.clone())
            .expect("thread_search_bar should be set after toggle_search");
        bar.update_in(cx, |bar, window, cx| {
            bar.query_editor.update(cx, |editor, cx| {
                editor.set_text("banana", window, cx);
            });
            bar.update_matches(window, cx);
        });
        cx.run_until_parked();

        let initial_match_count = bar.read_with(cx, |bar, _| bar.match_count());
        assert!(
            initial_match_count >= 2,
            "setup precondition: expected at least 2 matches, got {}",
            initial_match_count,
        );

        thread_view.update_in(cx, |view, window, cx| {
            let bar = view
                .thread_search_bar
                .clone()
                .expect("bar should still be set");
            bar.update(cx, |bar, cx| {
                bar.select_next_match(&super::thread_search_bar::SelectNextThreadMatch, window, cx);
            });
        });
        cx.run_until_parked();

        let active_after = bar.read_with(cx, |bar, _| bar.active_match_index());
        assert_eq!(
            active_after,
            Some(1),
            "select_next_match should have advanced from match 0 to match 1",
        );
    }

    /// Past user-message hits must be painted on the inner `Editor`.
    #[gpui::test]
    async fn test_thread_search_highlights_user_message_editor(cx: &mut TestAppContext) {
        init_test(cx);

        let connection = StubAgentConnection::new();
        connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
            acp::ContentChunk::new("Sure, I can help with that.".into()),
        )]);

        let (conversation_view, cx) =
            setup_conversation_view(StubAgentServer::new(connection.clone()), cx).await;
        add_to_workspace(conversation_view.clone(), cx);

        let thread =
            active_thread(&conversation_view, cx).read_with(cx, |view, _| view.thread.clone());
        thread
            .update(cx, |thread, cx| {
                thread.send_raw("Where do I find a kumquat?", cx)
            })
            .await
            .unwrap();
        cx.run_until_parked();

        let thread_view = active_thread(&conversation_view, cx);
        thread_view.update_in(cx, |view, window, cx| {
            view.toggle_search(&crate::ToggleSearch, window, cx);
        });
        cx.run_until_parked();

        let bar = thread_view
            .read_with(cx, |view, _| view.thread_search_bar.clone())
            .expect("thread_search_bar should be set after toggle_search");
        bar.update_in(cx, |bar, window, cx| {
            bar.query_editor.update(cx, |editor, cx| {
                editor.set_text("kumquat", window, cx);
            });
            bar.update_matches(window, cx);
        });
        cx.run_until_parked();

        let match_count = bar.read_with(cx, |bar, _| bar.match_count());
        assert_eq!(
            match_count, 1,
            "expected exactly one match for 'kumquat' (in the user message)",
        );

        let user_message_editor = thread_view.read_with(cx, |view, cx| {
            view.entry_view_state
                .read(cx)
                .entry(0)
                .and_then(|entry| entry.message_editor())
                .map(|message_editor| message_editor.read(cx).editor().clone())
                .expect("entry 0 should be a user message with a message editor")
        });
        let has_highlight = user_message_editor.read_with(cx, |editor, _cx| {
            editor.has_background_highlights(editor::HighlightKey::BufferSearchHighlights)
        });
        assert!(
            has_highlight,
            "user message editor should carry BufferSearchHighlights after the bar's matcher ran",
        );

        bar.update(cx, |bar, cx| bar.clear_highlights(cx));
        cx.run_until_parked();
        let has_highlight_after_clear = user_message_editor.read_with(cx, |editor, _cx| {
            editor.has_background_highlights(editor::HighlightKey::BufferSearchHighlights)
        });
        assert!(
            !has_highlight_after_clear,
            "clear_highlights should remove the editor-backed highlights",
        );
    }

    /// `editor::Cancel` should dismiss thread search before reaching workspace handlers.
    #[gpui::test]
    async fn test_thread_search_editor_cancel_dismisses_bar(cx: &mut TestAppContext) {
        init_test(cx);

        let (conversation_view, cx) =
            setup_conversation_view(StubAgentServer::new(StubAgentConnection::new()), cx).await;
        add_to_workspace(conversation_view.clone(), cx);

        let thread_view = active_thread(&conversation_view, cx);
        thread_view.update_in(cx, |view, window, cx| {
            view.toggle_search(&crate::ToggleSearch, window, cx);
        });
        cx.run_until_parked();

        let visible_before = thread_view.read_with(cx, |view, _| view.thread_search_visible);
        assert!(
            visible_before,
            "search bar should be visible after toggle_search"
        );

        let bar = thread_view
            .read_with(cx, |view, _| view.thread_search_bar.clone())
            .expect("bar should be set");
        let query_focus = bar.read_with(cx, |bar, cx| bar.query_editor.focus_handle(cx));
        cx.update(|window, cx| {
            window.focus(&query_focus, cx);
        });
        cx.run_until_parked();

        conversation_view.update_in(cx, |_, window, cx| {
            window.dispatch_action(editor::actions::Cancel.boxed_clone(), cx);
        });
        cx.run_until_parked();

        let visible_after = thread_view.read_with(cx, |view, _| view.thread_search_visible);
        assert!(
            !visible_after,
            "editor::Cancel should have dismissed the bar before reaching the workspace",
        );
    }

    /// JetBrains keymaps route Shift+Enter through `editor::NewlineBelow`.
    #[gpui::test]
    async fn test_thread_search_shift_enter_navigates_with_jetbrains_keymap(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        cx.update(|cx| {
            // Load both the default binding and the conflicting base keymap.
            search::init(cx);

            let mut default_bindings = settings::KeymapFile::load_asset_allow_partial_failure(
                "keymaps/default-linux.json",
                cx,
            )
            .unwrap();
            for binding in &mut default_bindings {
                binding.set_meta(settings::KeybindSource::Default.meta());
            }
            cx.bind_keys(default_bindings);

            let mut jetbrains_bindings = settings::KeymapFile::load_asset_allow_partial_failure(
                "keymaps/linux/jetbrains.json",
                cx,
            )
            .unwrap();
            for binding in &mut jetbrains_bindings {
                binding.set_meta(settings::KeybindSource::Base.meta());
            }
            cx.bind_keys(jetbrains_bindings);
        });

        let connection = StubAgentConnection::new();
        connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
            acp::ContentChunk::new(
                "Banana banana banana, multiple banana mentions in this reply.".into(),
            ),
        )]);

        let (conversation_view, cx) =
            setup_conversation_view(StubAgentServer::new(connection.clone()), cx).await;
        add_to_workspace(conversation_view.clone(), cx);

        let thread =
            active_thread(&conversation_view, cx).read_with(cx, |view, _| view.thread.clone());
        thread
            .update(cx, |thread, cx| thread.send_raw("Need banana help", cx))
            .await
            .unwrap();
        cx.run_until_parked();

        let thread_view = active_thread(&conversation_view, cx);
        thread_view.update_in(cx, |view, window, cx| {
            view.toggle_search(&crate::ToggleSearch, window, cx);
        });
        cx.run_until_parked();

        let bar = thread_view
            .read_with(cx, |view, _| view.thread_search_bar.clone())
            .expect("thread_search_bar should be set after toggle_search");

        bar.update_in(cx, |bar, window, cx| {
            bar.query_editor.update(cx, |editor, cx| {
                editor.set_text("banana", window, cx);
            });
            bar.update_matches(window, cx);
        });
        cx.run_until_parked();

        let initial_count = bar.read_with(cx, |bar, _| bar.match_count());
        assert!(
            initial_count >= 2,
            "test precondition: need ≥2 matches across the thread, got {}",
            initial_count,
        );
        assert_eq!(
            bar.read_with(cx, |bar, _| bar.active_match_index()),
            Some(0),
            "first match should be active after the bar populates its match list",
        );

        let query_focus = bar.read_with(cx, |bar, cx| bar.query_editor.focus_handle(cx));
        cx.update(|window, cx| {
            window.focus(&query_focus, cx);
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            assert!(
                query_focus.contains_focused(window, cx),
                "query editor must be focused before simulating shift-enter",
            );
        });

        cx.simulate_keystrokes("shift-enter");
        cx.run_until_parked();

        let query_text_after = bar.read_with(cx, |bar, cx| bar.query_editor.read(cx).text(cx));
        assert!(
            !query_text_after.contains('\n'),
            "shift-enter must not insert a newline into the query buffer; got {:?}",
            query_text_after,
        );

        let active_after = bar.read_with(cx, |bar, _| bar.active_match_index());
        assert_eq!(
            active_after,
            Some(initial_count - 1),
            "shift-enter should have wrapped active match from 0 to {} (got {:?})",
            initial_count - 1,
            active_after,
        );
    }

    /// `f3`/`shift-f3` are bound in the broad `AcpThread` context (like buffer
    /// search's pane-level `cmd-g`), so they must navigate matches even when
    /// focus is outside the search bar.
    #[gpui::test]
    async fn test_thread_search_navigates_from_outside_search_bar(cx: &mut TestAppContext) {
        init_test(cx);
        cx.update(|cx| {
            search::init(cx);
            let mut default_bindings = settings::KeymapFile::load_asset_allow_partial_failure(
                "keymaps/default-linux.json",
                cx,
            )
            .unwrap();
            for binding in &mut default_bindings {
                binding.set_meta(settings::KeybindSource::Default.meta());
            }
            cx.bind_keys(default_bindings);
        });

        let connection = StubAgentConnection::new();
        connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
            acp::ContentChunk::new(
                "Banana banana banana, multiple banana mentions in this reply.".into(),
            ),
        )]);

        let (conversation_view, cx) =
            setup_conversation_view(StubAgentServer::new(connection.clone()), cx).await;
        add_to_workspace(conversation_view.clone(), cx);

        let thread =
            active_thread(&conversation_view, cx).read_with(cx, |view, _| view.thread.clone());
        thread
            .update(cx, |thread, cx| thread.send_raw("Need banana help", cx))
            .await
            .unwrap();
        cx.run_until_parked();

        let thread_view = active_thread(&conversation_view, cx);
        thread_view.update_in(cx, |view, window, cx| {
            view.toggle_search(&crate::ToggleSearch, window, cx);
        });
        cx.run_until_parked();

        let bar = thread_view
            .read_with(cx, |view, _| view.thread_search_bar.clone())
            .expect("thread_search_bar should be set after toggle_search");
        bar.update_in(cx, |bar, window, cx| {
            bar.query_editor.update(cx, |editor, cx| {
                editor.set_text("banana", window, cx);
            });
            bar.update_matches(window, cx);
        });
        cx.run_until_parked();

        let initial_count = bar.read_with(cx, |bar, _| bar.match_count());
        assert!(
            initial_count >= 2,
            "test precondition: need ≥ 2 matches, got {}",
            initial_count,
        );
        assert_eq!(
            bar.read_with(cx, |bar, _| bar.active_match_index()),
            Some(0)
        );

        // Move focus out of the search bar, back into the thread view itself.
        let thread_focus = thread_view.read_with(cx, |view, cx| view.focus_handle(cx));
        cx.update(|window, cx| window.focus(&thread_focus, cx));
        cx.run_until_parked();
        cx.update(|window, cx| {
            let bar_focused = bar.read_with(cx, |bar, cx| {
                bar.query_editor
                    .focus_handle(cx)
                    .contains_focused(window, cx)
            });
            assert!(!bar_focused, "search bar must not be focused for this test");
        });

        cx.simulate_keystrokes("f3");
        cx.run_until_parked();
        assert_eq!(
            bar.read_with(cx, |bar, _| bar.active_match_index()),
            Some(1),
            "f3 from outside the bar should advance to the next match",
        );

        cx.simulate_keystrokes("shift-f3");
        cx.run_until_parked();
        assert_eq!(
            bar.read_with(cx, |bar, _| bar.active_match_index()),
            Some(0),
            "shift-f3 from outside the bar should return to the previous match",
        );
    }

    #[gpui::test]
    async fn test_message_editing_cancel(cx: &mut TestAppContext) {
        init_test(cx);

        let connection = StubAgentConnection::new();

        connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
            acp::ContentChunk::new("Response".into()),
        )]);

        let (conversation_view, cx) =
            setup_conversation_view(StubAgentServer::new(connection), cx).await;
        add_to_workspace(conversation_view.clone(), cx);

        let message_editor = message_editor(&conversation_view, cx);
        message_editor.update_in(cx, |editor, window, cx| {
            editor.set_text("Original message to edit", window, cx);
        });
        active_thread(&conversation_view, cx)
            .update_in(cx, |view, window, cx| view.send(window, cx));

        cx.run_until_parked();

        let user_message_editor = conversation_view.read_with(cx, |view, cx| {
            assert_eq!(
                view.active_thread()
                    .and_then(|active| active.read(cx).editing_message),
                None
            );

            view.active_thread()
                .map(|active| &active.read(cx).entry_view_state)
                .as_ref()
                .unwrap()
                .read(cx)
                .entry(0)
                .unwrap()
                .message_editor()
                .unwrap()
                .clone()
        });

        // Focus
        cx.focus(&user_message_editor);
        conversation_view.read_with(cx, |view, cx| {
            assert_eq!(
                view.active_thread()
                    .and_then(|active| active.read(cx).editing_message),
                Some(0)
            );
        });

        // Edit
        user_message_editor.update_in(cx, |editor, window, cx| {
            editor.set_text("Edited message content", window, cx);
        });

        // Cancel
        user_message_editor.update_in(cx, |_editor, window, cx| {
            window.dispatch_action(Box::new(editor::actions::Cancel), cx);
        });

        conversation_view.read_with(cx, |view, cx| {
            assert_eq!(
                view.active_thread()
                    .and_then(|active| active.read(cx).editing_message),
                None
            );
        });

        user_message_editor.read_with(cx, |editor, cx| {
            assert_eq!(editor.text(cx), "Original message to edit");
        });
    }

    #[gpui::test]
    async fn test_message_doesnt_send_if_empty(cx: &mut TestAppContext) {
        init_test(cx);

        let connection = StubAgentConnection::new();

        let (conversation_view, cx) =
            setup_conversation_view(StubAgentServer::new(connection), cx).await;
        add_to_workspace(conversation_view.clone(), cx);

        let message_editor = message_editor(&conversation_view, cx);
        message_editor.update_in(cx, |editor, window, cx| {
            editor.set_text("", window, cx);
        });

        let thread = cx.read(|cx| {
            conversation_view
                .read(cx)
                .active_thread()
                .unwrap()
                .read(cx)
                .thread
                .clone()
        });
        let entries_before = cx.read(|cx| thread.read(cx).entries().len());

        active_thread(&conversation_view, cx).update_in(cx, |view, window, cx| {
            view.send(window, cx);
        });
        cx.run_until_parked();

        let entries_after = cx.read(|cx| thread.read(cx).entries().len());
        assert_eq!(
            entries_before, entries_after,
            "No message should be sent when editor is empty"
        );
    }

    #[gpui::test]
    async fn test_message_editing_regenerate(cx: &mut TestAppContext) {
        init_test(cx);

        let connection = StubAgentConnection::new();

        connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
            acp::ContentChunk::new("Response".into()),
        )]);

        let (conversation_view, cx) =
            setup_conversation_view(StubAgentServer::new(connection.clone()), cx).await;
        add_to_workspace(conversation_view.clone(), cx);

        let message_editor = message_editor(&conversation_view, cx);
        message_editor.update_in(cx, |editor, window, cx| {
            editor.set_text("Original message to edit", window, cx);
        });
        active_thread(&conversation_view, cx)
            .update_in(cx, |view, window, cx| view.send(window, cx));

        cx.run_until_parked();

        let user_message_editor = conversation_view.read_with(cx, |view, cx| {
            assert_eq!(
                view.active_thread()
                    .and_then(|active| active.read(cx).editing_message),
                None
            );
            assert_eq!(
                view.active_thread()
                    .unwrap()
                    .read(cx)
                    .thread
                    .read(cx)
                    .entries()
                    .len(),
                2
            );

            view.active_thread()
                .map(|active| &active.read(cx).entry_view_state)
                .as_ref()
                .unwrap()
                .read(cx)
                .entry(0)
                .unwrap()
                .message_editor()
                .unwrap()
                .clone()
        });

        // Focus
        cx.focus(&user_message_editor);

        // Edit
        user_message_editor.update_in(cx, |editor, window, cx| {
            editor.set_text("Edited message content", window, cx);
        });

        // Send
        connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
            acp::ContentChunk::new("New Response".into()),
        )]);

        user_message_editor.update_in(cx, |_editor, window, cx| {
            window.dispatch_action(Box::new(Chat), cx);
        });

        cx.run_until_parked();

        conversation_view.read_with(cx, |view, cx| {
            assert_eq!(
                view.active_thread()
                    .and_then(|active| active.read(cx).editing_message),
                None
            );

            let entries = view
                .active_thread()
                .unwrap()
                .read(cx)
                .thread
                .read(cx)
                .entries();
            assert_eq!(entries.len(), 2);
            assert_eq!(
                entries[0].to_markdown(cx),
                "## User\n\nEdited message content\n\n"
            );
            assert_eq!(
                entries[1].to_markdown(cx),
                "## Assistant\n\nNew Response\n\n"
            );

            let entry_view_state = view
                .active_thread()
                .map(|active| &active.read(cx).entry_view_state)
                .unwrap();
            let new_editor = entry_view_state.read_with(cx, |state, _cx| {
                assert!(!state.entry(1).unwrap().has_content());
                state.entry(0).unwrap().message_editor().unwrap().clone()
            });

            assert_eq!(new_editor.read(cx).text(cx), "Edited message content");
        })
    }

    #[gpui::test]
    async fn test_message_editing_while_generating(cx: &mut TestAppContext) {
        init_test(cx);

        let connection = StubAgentConnection::new();

        let (conversation_view, cx) =
            setup_conversation_view(StubAgentServer::new(connection.clone()), cx).await;
        add_to_workspace(conversation_view.clone(), cx);

        let message_editor = message_editor(&conversation_view, cx);
        message_editor.update_in(cx, |editor, window, cx| {
            editor.set_text("Original message to edit", window, cx);
        });
        active_thread(&conversation_view, cx)
            .update_in(cx, |view, window, cx| view.send(window, cx));

        cx.run_until_parked();

        let (user_message_editor, session_id) = conversation_view.read_with(cx, |view, cx| {
            let thread = view.active_thread().unwrap().read(cx).thread.read(cx);
            assert_eq!(thread.entries().len(), 1);

            let editor = view
                .active_thread()
                .map(|active| &active.read(cx).entry_view_state)
                .as_ref()
                .unwrap()
                .read(cx)
                .entry(0)
                .unwrap()
                .message_editor()
                .unwrap()
                .clone();

            (editor, thread.session_id().clone())
        });

        // Focus
        cx.focus(&user_message_editor);

        conversation_view.read_with(cx, |view, cx| {
            assert_eq!(
                view.active_thread()
                    .and_then(|active| active.read(cx).editing_message),
                Some(0)
            );
        });

        // Edit
        user_message_editor.update_in(cx, |editor, window, cx| {
            editor.set_text("Edited message content", window, cx);
        });

        conversation_view.read_with(cx, |view, cx| {
            assert_eq!(
                view.active_thread()
                    .and_then(|active| active.read(cx).editing_message),
                Some(0)
            );
        });

        // Finish streaming response
        cx.update(|_, cx| {
            connection.send_update(
                session_id.clone(),
                acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new("Response".into())),
                cx,
            );
            connection.end_turn(session_id, acp::StopReason::EndTurn);
        });

        conversation_view.read_with(cx, |view, cx| {
            assert_eq!(
                view.active_thread()
                    .and_then(|active| active.read(cx).editing_message),
                Some(0)
            );
        });

        cx.run_until_parked();

        // Should still be editing
        cx.update(|window, cx| {
            assert!(user_message_editor.focus_handle(cx).is_focused(window));
            assert_eq!(
                conversation_view
                    .read(cx)
                    .active_thread()
                    .and_then(|active| active.read(cx).editing_message),
                Some(0)
            );
            assert_eq!(
                user_message_editor.read(cx).text(cx),
                "Edited message content"
            );
        });
    }

    #[gpui::test]
    async fn test_stale_stop_does_not_disable_follow_tail_during_regenerate(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);

        let connection = StubAgentConnection::new();

        let (conversation_view, cx) =
            setup_conversation_view(StubAgentServer::new(connection.clone()), cx).await;
        add_to_workspace(conversation_view.clone(), cx);

        let message_editor = message_editor(&conversation_view, cx);
        message_editor.update_in(cx, |editor, window, cx| {
            editor.set_text("Original message to edit", window, cx);
        });
        active_thread(&conversation_view, cx)
            .update_in(cx, |view, window, cx| view.send(window, cx));

        cx.run_until_parked();

        let user_message_editor = conversation_view.read_with(cx, |view, cx| {
            view.active_thread()
                .map(|active| &active.read(cx).entry_view_state)
                .as_ref()
                .unwrap()
                .read(cx)
                .entry(0)
                .unwrap()
                .message_editor()
                .unwrap()
                .clone()
        });

        cx.focus(&user_message_editor);
        user_message_editor.update_in(cx, |editor, window, cx| {
            editor.set_text("Edited message content", window, cx);
        });

        user_message_editor.update_in(cx, |_editor, window, cx| {
            window.dispatch_action(Box::new(Chat), cx);
        });

        cx.run_until_parked();

        conversation_view.read_with(cx, |view, cx| {
            let active = view.active_thread().unwrap();
            let active = active.read(cx);

            assert_eq!(active.thread.read(cx).status(), ThreadStatus::Generating);
            assert!(
                active.list_state.is_following_tail(),
                "stale stop events from the cancelled turn must not disable follow-tail for the new turn"
            );
        });
    }

    struct GeneratingThreadSetup {
        conversation_view: Entity<ConversationView>,
        thread: Entity<AcpThread>,
        message_editor: Entity<MessageEditor>,
    }

    async fn setup_generating_thread(
        cx: &mut TestAppContext,
    ) -> (GeneratingThreadSetup, &mut VisualTestContext) {
        let connection = StubAgentConnection::new();

        let (conversation_view, cx) =
            setup_conversation_view(StubAgentServer::new(connection.clone()), cx).await;
        add_to_workspace(conversation_view.clone(), cx);

        let message_editor = message_editor(&conversation_view, cx);
        message_editor.update_in(cx, |editor, window, cx| {
            editor.set_text("Hello", window, cx);
        });
        active_thread(&conversation_view, cx)
            .update_in(cx, |view, window, cx| view.send(window, cx));

        let (thread, session_id) = conversation_view.read_with(cx, |view, cx| {
            let thread = view
                .active_thread()
                .as_ref()
                .unwrap()
                .read(cx)
                .thread
                .clone();
            (thread.clone(), thread.read(cx).session_id().clone())
        });

        cx.run_until_parked();

        cx.update(|_, cx| {
            connection.send_update(
                session_id.clone(),
                acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                    "Response chunk".into(),
                )),
                cx,
            );
        });

        cx.run_until_parked();

        thread.read_with(cx, |thread, _cx| {
            assert_eq!(thread.status(), ThreadStatus::Generating);
        });

        (
            GeneratingThreadSetup {
                conversation_view,
                thread,
                message_editor,
            },
            cx,
        )
    }

    #[gpui::test]
    async fn test_escape_cancels_generation_from_conversation_focus(cx: &mut TestAppContext) {
        init_test(cx);

        let (setup, cx) = setup_generating_thread(cx).await;

        let focus_handle = setup
            .conversation_view
            .read_with(cx, |view, cx| view.focus_handle(cx));
        cx.update(|window, cx| {
            window.focus(&focus_handle, cx);
        });

        setup.conversation_view.update_in(cx, |_, window, cx| {
            window.dispatch_action(menu::Cancel.boxed_clone(), cx);
        });

        cx.run_until_parked();

        setup.thread.read_with(cx, |thread, _cx| {
            assert_eq!(thread.status(), ThreadStatus::Idle);
        });
    }

    #[gpui::test]
    async fn test_escape_cancels_generation_from_editor_focus(cx: &mut TestAppContext) {
        init_test(cx);

        let (setup, cx) = setup_generating_thread(cx).await;

        let editor_focus_handle = setup
            .message_editor
            .read_with(cx, |editor, cx| editor.focus_handle(cx));
        cx.update(|window, cx| {
            window.focus(&editor_focus_handle, cx);
        });

        setup.message_editor.update_in(cx, |_, window, cx| {
            window.dispatch_action(editor::actions::Cancel.boxed_clone(), cx);
        });

        cx.run_until_parked();

        setup.thread.read_with(cx, |thread, _cx| {
            assert_eq!(thread.status(), ThreadStatus::Idle);
        });
    }

    #[gpui::test]
    async fn test_escape_when_idle_is_noop(cx: &mut TestAppContext) {
        init_test(cx);

        let (conversation_view, cx) =
            setup_conversation_view(StubAgentServer::new(StubAgentConnection::new()), cx).await;
        add_to_workspace(conversation_view.clone(), cx);

        let thread = conversation_view.read_with(cx, |view, cx| {
            view.active_thread().unwrap().read(cx).thread.clone()
        });

        thread.read_with(cx, |thread, _cx| {
            assert_eq!(thread.status(), ThreadStatus::Idle);
        });

        let focus_handle = conversation_view.read_with(cx, |view, _cx| view.focus_handle.clone());
        cx.update(|window, cx| {
            window.focus(&focus_handle, cx);
        });

        conversation_view.update_in(cx, |_, window, cx| {
            window.dispatch_action(menu::Cancel.boxed_clone(), cx);
        });

        cx.run_until_parked();

        thread.read_with(cx, |thread, _cx| {
            assert_eq!(thread.status(), ThreadStatus::Idle);
        });
    }

    #[gpui::test]
    async fn test_interrupt(cx: &mut TestAppContext) {
        init_test(cx);

        let connection = StubAgentConnection::new();

        let (conversation_view, cx) =
            setup_conversation_view(StubAgentServer::new(connection.clone()), cx).await;
        add_to_workspace(conversation_view.clone(), cx);

        let message_editor = message_editor(&conversation_view, cx);
        message_editor.update_in(cx, |editor, window, cx| {
            editor.set_text("Message 1", window, cx);
        });
        active_thread(&conversation_view, cx)
            .update_in(cx, |view, window, cx| view.send(window, cx));

        let (thread, session_id) = conversation_view.read_with(cx, |view, cx| {
            let thread = view.active_thread().unwrap().read(cx).thread.clone();

            (thread.clone(), thread.read(cx).session_id().clone())
        });

        cx.run_until_parked();

        cx.update(|_, cx| {
            connection.send_update(
                session_id.clone(),
                acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                    "Message 1 resp".into(),
                )),
                cx,
            );
        });

        cx.run_until_parked();

        thread.read_with(cx, |thread, cx| {
            assert_eq!(
                thread.to_markdown(cx),
                indoc::indoc! {"
                        ## User

                        Message 1

                        ## Assistant

                        Message 1 resp

                    "}
            )
        });

        message_editor.update_in(cx, |editor, window, cx| {
            editor.set_text("Message 2", window, cx);
        });
        active_thread(&conversation_view, cx)
            .update_in(cx, |view, window, cx| view.interrupt_and_send(window, cx));

        cx.update(|_, cx| {
            // Simulate a response sent after beginning to cancel
            connection.send_update(
                session_id.clone(),
                acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new("onse".into())),
                cx,
            );
        });

        cx.run_until_parked();

        // Last Message 1 response should appear before Message 2
        thread.read_with(cx, |thread, cx| {
            assert_eq!(
                thread.to_markdown(cx),
                indoc::indoc! {"
                        ## User

                        Message 1

                        ## Assistant

                        Message 1 response

                        ## User

                        Message 2

                    "}
            )
        });

        cx.update(|_, cx| {
            connection.send_update(
                session_id.clone(),
                acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                    "Message 2 response".into(),
                )),
                cx,
            );
            connection.end_turn(session_id.clone(), acp::StopReason::EndTurn);
        });

        cx.run_until_parked();

        thread.read_with(cx, |thread, cx| {
            assert_eq!(
                thread.to_markdown(cx),
                indoc::indoc! {"
                        ## User

                        Message 1

                        ## Assistant

                        Message 1 response

                        ## User

                        Message 2

                        ## Assistant

                        Message 2 response

                    "}
            )
        });
    }

    #[gpui::test]
    async fn test_message_editing_insert_selections(cx: &mut TestAppContext) {
        init_test(cx);

        let connection = StubAgentConnection::new();
        connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
            acp::ContentChunk::new("Response".into()),
        )]);

        let (conversation_view, cx) =
            setup_conversation_view(StubAgentServer::new(connection), cx).await;
        add_to_workspace(conversation_view.clone(), cx);

        let message_editor = message_editor(&conversation_view, cx);
        message_editor.update_in(cx, |editor, window, cx| {
            editor.set_text("Original message to edit", window, cx)
        });
        active_thread(&conversation_view, cx)
            .update_in(cx, |view, window, cx| view.send(window, cx));
        cx.run_until_parked();

        let user_message_editor = conversation_view.read_with(cx, |conversation_view, cx| {
            conversation_view
                .active_thread()
                .map(|active| &active.read(cx).entry_view_state)
                .as_ref()
                .unwrap()
                .read(cx)
                .entry(0)
                .expect("Should have at least one entry")
                .message_editor()
                .expect("Should have message editor")
                .clone()
        });

        cx.focus(&user_message_editor);
        conversation_view.read_with(cx, |view, cx| {
            assert_eq!(
                view.active_thread()
                    .and_then(|active| active.read(cx).editing_message),
                Some(0)
            );
        });

        // Ensure to edit the focused message before proceeding otherwise, since
        // its content is not different from what was sent, focus will be lost.
        user_message_editor.update_in(cx, |editor, window, cx| {
            editor.set_text("Original message to edit with ", window, cx)
        });

        // Create a simple buffer with some text so we can create a selection
        // that will then be added to the message being edited.
        let (workspace, project) = conversation_view.read_with(cx, |conversation_view, _cx| {
            (
                conversation_view.workspace.clone(),
                conversation_view.project.clone(),
            )
        });
        let buffer = project.update(cx, |project, cx| {
            project.create_local_buffer("let a = 10 + 10;", None, false, cx)
        });

        workspace
            .update_in(cx, |workspace, window, cx| {
                let editor = cx.new(|cx| {
                    let mut editor =
                        Editor::for_buffer(buffer.clone(), Some(project.clone()), window, cx);

                    editor.change_selections(Default::default(), window, cx, |selections| {
                        selections.select_ranges([MultiBufferOffset(8)..MultiBufferOffset(15)]);
                    });

                    editor
                });
                workspace.add_item_to_active_pane(Box::new(editor), None, false, window, cx);
            })
            .unwrap();

        conversation_view.update_in(cx, |view, window, cx| {
            assert_eq!(
                view.active_thread()
                    .and_then(|active| active.read(cx).editing_message),
                Some(0)
            );
            let workspace = workspace.upgrade().unwrap();
            let selection = workspace
                .update(cx, |workspace, cx| {
                    AgentContextSource::from_active(workspace, cx)?
                        .read_selection(workspace, false, cx)
                })
                .unwrap();
            view.insert_selection(selection, window, cx);
        });

        user_message_editor.read_with(cx, |editor, cx| {
            let text = editor.editor().read(cx).text(cx);
            let expected_text = String::from("Original message to edit with selection ");

            assert_eq!(text, expected_text);
        });
    }

    #[gpui::test]
    async fn test_insert_selections(cx: &mut TestAppContext) {
        init_test(cx);

        let connection = StubAgentConnection::new();
        connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
            acp::ContentChunk::new("Response".into()),
        )]);

        let (conversation_view, cx) =
            setup_conversation_view(StubAgentServer::new(connection), cx).await;
        add_to_workspace(conversation_view.clone(), cx);

        let message_editor = message_editor(&conversation_view, cx);
        message_editor.update_in(cx, |editor, window, cx| {
            editor.set_text("Can you review this snippet ", window, cx)
        });

        // Create a simple buffer with some text so we can create a selection
        // that will then be added to the message being edited.
        let (workspace, project) = conversation_view.read_with(cx, |conversation_view, _cx| {
            (
                conversation_view.workspace.clone(),
                conversation_view.project.clone(),
            )
        });
        let buffer = project.update(cx, |project, cx| {
            project.create_local_buffer("let a = 10 + 10;", None, false, cx)
        });

        workspace
            .update_in(cx, |workspace, window, cx| {
                let editor = cx.new(|cx| {
                    let mut editor =
                        Editor::for_buffer(buffer.clone(), Some(project.clone()), window, cx);

                    editor.change_selections(Default::default(), window, cx, |selections| {
                        selections.select_ranges([MultiBufferOffset(8)..MultiBufferOffset(15)]);
                    });

                    editor
                });
                workspace.add_item_to_active_pane(Box::new(editor), None, false, window, cx);
            })
            .unwrap();

        conversation_view.update_in(cx, |view, window, cx| {
            assert_eq!(
                view.active_thread()
                    .and_then(|active| active.read(cx).editing_message),
                None
            );
            let workspace = view.workspace.upgrade().unwrap();
            let selection = workspace
                .update(cx, |workspace, cx| {
                    AgentContextSource::from_active(workspace, cx)?
                        .read_selection(workspace, false, cx)
                })
                .unwrap();
            view.insert_selection(selection, window, cx);
        });

        message_editor.read_with(cx, |editor, cx| {
            let text = editor.text(cx);
            let expected_txt = String::from("Can you review this snippet selection ");

            assert_eq!(text, expected_txt);
        })
    }

    #[gpui::test]
    async fn test_tool_permission_buttons_terminal_with_pattern(cx: &mut TestAppContext) {
        init_test(cx);

        let tool_call_id = acp::ToolCallId::new("terminal-1");
        let tool_call = acp::ToolCall::new(tool_call_id.clone(), "Run `cargo build --release`")
            .kind(acp::ToolKind::Edit);

        let permission_options = ToolPermissionContext::new(
            TerminalTool::NAME,
            vec!["cargo build --release".to_string()],
        )
        .build_permission_options();

        let connection =
            StubAgentConnection::new().with_permission_requests(HashMap::from_iter([(
                tool_call_id.clone(),
                permission_options,
            )]));

        connection.set_next_prompt_updates(vec![acp::SessionUpdate::ToolCall(tool_call)]);

        let (conversation_view, cx) =
            setup_conversation_view(StubAgentServer::new(connection), cx).await;

        // Disable notifications to avoid popup windows
        cx.update(|_window, cx| {
            AgentSettings::override_global(
                AgentSettings {
                    notify_when_agent_waiting: NotifyWhenAgentWaiting::Never,
                    ..AgentSettings::get_global(cx).clone()
                },
                cx,
            );
        });

        let message_editor = message_editor(&conversation_view, cx);
        message_editor.update_in(cx, |editor, window, cx| {
            editor.set_text("Run cargo build", window, cx);
        });

        active_thread(&conversation_view, cx)
            .update_in(cx, |view, window, cx| view.send(window, cx));

        cx.run_until_parked();

        // Verify the tool call is in WaitingForConfirmation state with the expected options
        conversation_view.read_with(cx, |conversation_view, cx| {
            let thread = conversation_view
                .active_thread()
                .expect("Thread should exist")
                .read(cx)
                .thread
                .clone();
            let thread = thread.read(cx);

            let tool_call = thread.entries().iter().find_map(|entry| {
                if let acp_thread::AgentThreadEntry::ToolCall(call) = entry {
                    Some(call)
                } else {
                    None
                }
            });

            assert!(tool_call.is_some(), "Expected a tool call entry");
            let tool_call = tool_call.unwrap();

            // Verify it's waiting for confirmation
            assert!(
                matches!(
                    tool_call.status,
                    acp_thread::ToolCallStatus::WaitingForConfirmation { .. }
                ),
                "Expected WaitingForConfirmation status, got {:?}",
                tool_call.status
            );

            // Verify the options count (granularity options only, no separate Deny option)
            if let acp_thread::ToolCallStatus::WaitingForConfirmation { options, .. } =
                &tool_call.status
            {
                let PermissionOptions::Dropdown(choices) = options else {
                    panic!("Expected dropdown permission options");
                };

                assert_eq!(
                    choices.len(),
                    3,
                    "Expected 3 permission options (granularity only)"
                );

                // Verify specific button labels (now using neutral names)
                let labels: Vec<&str> = choices
                    .iter()
                    .map(|choice| choice.allow.name.as_ref())
                    .collect();
                assert!(
                    labels.contains(&"ターミナルは常に"),
                    "Missing 'ターミナルは常に' option"
                );
                assert!(
                    labels.contains(&"`cargo build` コマンドは常に"),
                    "Missing pattern option"
                );
                assert!(labels.contains(&"今回のみ"), "Missing '今回のみ' option");
            }
        });
    }

    #[gpui::test]
    async fn test_tool_permission_buttons_edit_file_with_path_pattern(cx: &mut TestAppContext) {
        init_test(cx);

        let tool_call_id = acp::ToolCallId::new("edit-file-1");
        let tool_call = acp::ToolCall::new(tool_call_id.clone(), "Edit `src/main.rs`")
            .kind(acp::ToolKind::Edit);

        let permission_options =
            ToolPermissionContext::new(EditFileTool::NAME, vec!["src/main.rs".to_string()])
                .build_permission_options();

        let connection =
            StubAgentConnection::new().with_permission_requests(HashMap::from_iter([(
                tool_call_id.clone(),
                permission_options,
            )]));

        connection.set_next_prompt_updates(vec![acp::SessionUpdate::ToolCall(tool_call)]);

        let (conversation_view, cx) =
            setup_conversation_view(StubAgentServer::new(connection), cx).await;

        // Disable notifications
        cx.update(|_window, cx| {
            AgentSettings::override_global(
                AgentSettings {
                    notify_when_agent_waiting: NotifyWhenAgentWaiting::Never,
                    ..AgentSettings::get_global(cx).clone()
                },
                cx,
            );
        });

        let message_editor = message_editor(&conversation_view, cx);
        message_editor.update_in(cx, |editor, window, cx| {
            editor.set_text("Edit the main file", window, cx);
        });

        active_thread(&conversation_view, cx)
            .update_in(cx, |view, window, cx| view.send(window, cx));

        cx.run_until_parked();

        // Verify the options
        conversation_view.read_with(cx, |conversation_view, cx| {
            let thread = conversation_view
                .active_thread()
                .expect("Thread should exist")
                .read(cx)
                .thread
                .clone();
            let thread = thread.read(cx);

            let tool_call = thread.entries().iter().find_map(|entry| {
                if let acp_thread::AgentThreadEntry::ToolCall(call) = entry {
                    Some(call)
                } else {
                    None
                }
            });

            assert!(tool_call.is_some(), "Expected a tool call entry");
            let tool_call = tool_call.unwrap();

            if let acp_thread::ToolCallStatus::WaitingForConfirmation { options, .. } =
                &tool_call.status
            {
                let PermissionOptions::Dropdown(choices) = options else {
                    panic!("Expected dropdown permission options");
                };

                let labels: Vec<&str> = choices
                    .iter()
                    .map(|choice| choice.allow.name.as_ref())
                    .collect();
                assert!(
                    labels.contains(&"ファイル編集は常に"),
                    "Missing 'ファイル編集は常に' option"
                );
                assert!(
                    labels.contains(&"`src/` は常に"),
                    "Missing path pattern option"
                );
            } else {
                panic!("Expected WaitingForConfirmation status");
            }
        });
    }

    #[gpui::test]
    async fn test_tool_permission_buttons_fetch_with_domain_pattern(cx: &mut TestAppContext) {
        init_test(cx);

        let tool_call_id = acp::ToolCallId::new("fetch-1");
        let tool_call = acp::ToolCall::new(tool_call_id.clone(), "Fetch `https://docs.rs/gpui`")
            .kind(acp::ToolKind::Fetch);

        let permission_options =
            ToolPermissionContext::new(FetchTool::NAME, vec!["https://docs.rs/gpui".to_string()])
                .build_permission_options();

        let connection =
            StubAgentConnection::new().with_permission_requests(HashMap::from_iter([(
                tool_call_id.clone(),
                permission_options,
            )]));

        connection.set_next_prompt_updates(vec![acp::SessionUpdate::ToolCall(tool_call)]);

        let (conversation_view, cx) =
            setup_conversation_view(StubAgentServer::new(connection), cx).await;

        // Disable notifications
        cx.update(|_window, cx| {
            AgentSettings::override_global(
                AgentSettings {
                    notify_when_agent_waiting: NotifyWhenAgentWaiting::Never,
                    ..AgentSettings::get_global(cx).clone()
                },
                cx,
            );
        });

        let message_editor = message_editor(&conversation_view, cx);
        message_editor.update_in(cx, |editor, window, cx| {
            editor.set_text("Fetch the docs", window, cx);
        });

        active_thread(&conversation_view, cx)
            .update_in(cx, |view, window, cx| view.send(window, cx));

        cx.run_until_parked();

        // Verify the options
        conversation_view.read_with(cx, |conversation_view, cx| {
            let thread = conversation_view
                .active_thread()
                .expect("Thread should exist")
                .read(cx)
                .thread
                .clone();
            let thread = thread.read(cx);

            let tool_call = thread.entries().iter().find_map(|entry| {
                if let acp_thread::AgentThreadEntry::ToolCall(call) = entry {
                    Some(call)
                } else {
                    None
                }
            });

            assert!(tool_call.is_some(), "Expected a tool call entry");
            let tool_call = tool_call.unwrap();

            if let acp_thread::ToolCallStatus::WaitingForConfirmation { options, .. } =
                &tool_call.status
            {
                let PermissionOptions::Dropdown(choices) = options else {
                    panic!("Expected dropdown permission options");
                };

                let labels: Vec<&str> = choices
                    .iter()
                    .map(|choice| choice.allow.name.as_ref())
                    .collect();
                assert!(
                    labels.contains(&"URL の取得は常に"),
                    "Missing 'URL の取得は常に' option"
                );
                assert!(
                    labels.contains(&"`docs.rs` は常に"),
                    "Missing domain pattern option"
                );
            } else {
                panic!("Expected WaitingForConfirmation status");
            }
        });
    }

    #[gpui::test]
    async fn test_tool_permission_buttons_without_pattern(cx: &mut TestAppContext) {
        init_test(cx);

        let tool_call_id = acp::ToolCallId::new("terminal-no-pattern-1");
        let tool_call = acp::ToolCall::new(tool_call_id.clone(), "Run `./deploy.sh --production`")
            .kind(acp::ToolKind::Edit);

        // No pattern button since ./deploy.sh doesn't match the alphanumeric pattern
        let permission_options = ToolPermissionContext::new(
            TerminalTool::NAME,
            vec!["./deploy.sh --production".to_string()],
        )
        .build_permission_options();

        let connection =
            StubAgentConnection::new().with_permission_requests(HashMap::from_iter([(
                tool_call_id.clone(),
                permission_options,
            )]));

        connection.set_next_prompt_updates(vec![acp::SessionUpdate::ToolCall(tool_call)]);

        let (conversation_view, cx) =
            setup_conversation_view(StubAgentServer::new(connection), cx).await;

        // Disable notifications
        cx.update(|_window, cx| {
            AgentSettings::override_global(
                AgentSettings {
                    notify_when_agent_waiting: NotifyWhenAgentWaiting::Never,
                    ..AgentSettings::get_global(cx).clone()
                },
                cx,
            );
        });

        let message_editor = message_editor(&conversation_view, cx);
        message_editor.update_in(cx, |editor, window, cx| {
            editor.set_text("Run the deploy script", window, cx);
        });

        active_thread(&conversation_view, cx)
            .update_in(cx, |view, window, cx| view.send(window, cx));

        cx.run_until_parked();

        // Verify only 2 options (no pattern button when command doesn't match pattern)
        conversation_view.read_with(cx, |conversation_view, cx| {
            let thread = conversation_view
                .active_thread()
                .expect("Thread should exist")
                .read(cx)
                .thread
                .clone();
            let thread = thread.read(cx);

            let tool_call = thread.entries().iter().find_map(|entry| {
                if let acp_thread::AgentThreadEntry::ToolCall(call) = entry {
                    Some(call)
                } else {
                    None
                }
            });

            assert!(tool_call.is_some(), "Expected a tool call entry");
            let tool_call = tool_call.unwrap();

            if let acp_thread::ToolCallStatus::WaitingForConfirmation { options, .. } =
                &tool_call.status
            {
                let PermissionOptions::Dropdown(choices) = options else {
                    panic!("Expected dropdown permission options");
                };

                assert_eq!(
                    choices.len(),
                    2,
                    "Expected 2 permission options (no pattern option)"
                );

                let labels: Vec<&str> = choices
                    .iter()
                    .map(|choice| choice.allow.name.as_ref())
                    .collect();
                assert!(
                    labels.contains(&"ターミナルは常に"),
                    "Missing 'ターミナルは常に' option"
                );
                assert!(labels.contains(&"今回のみ"), "Missing '今回のみ' option");
                // Should NOT contain a pattern option
                assert!(
                    !labels.iter().any(|l| l.contains("commands")),
                    "Should not have pattern option"
                );
            } else {
                panic!("Expected WaitingForConfirmation status");
            }
        });
    }

    #[gpui::test]
    async fn test_authorize_tool_call_action_triggers_authorization(cx: &mut TestAppContext) {
        init_test(cx);

        let tool_call_id = acp::ToolCallId::new("action-test-1");
        let tool_call =
            acp::ToolCall::new(tool_call_id.clone(), "Run `cargo test`").kind(acp::ToolKind::Edit);

        let permission_options =
            ToolPermissionContext::new(TerminalTool::NAME, vec!["cargo test".to_string()])
                .build_permission_options();

        let connection =
            StubAgentConnection::new().with_permission_requests(HashMap::from_iter([(
                tool_call_id.clone(),
                permission_options,
            )]));

        connection.set_next_prompt_updates(vec![acp::SessionUpdate::ToolCall(tool_call)]);

        let (conversation_view, cx) =
            setup_conversation_view(StubAgentServer::new(connection), cx).await;
        add_to_workspace(conversation_view.clone(), cx);

        cx.update(|_window, cx| {
            AgentSettings::override_global(
                AgentSettings {
                    notify_when_agent_waiting: NotifyWhenAgentWaiting::Never,
                    ..AgentSettings::get_global(cx).clone()
                },
                cx,
            );
        });

        let message_editor = message_editor(&conversation_view, cx);
        message_editor.update_in(cx, |editor, window, cx| {
            editor.set_text("Run tests", window, cx);
        });

        active_thread(&conversation_view, cx)
            .update_in(cx, |view, window, cx| view.send(window, cx));

        cx.run_until_parked();

        // Verify tool call is waiting for confirmation
        conversation_view.read_with(cx, |conversation_view, cx| {
            let tool_call = conversation_view.pending_tool_call(cx);
            assert!(
                tool_call.is_some(),
                "Expected a tool call waiting for confirmation"
            );
        });

        // Dispatch the AuthorizeToolCall action (simulating dropdown menu selection)
        conversation_view.update_in(cx, |_, window, cx| {
            window.dispatch_action(
                crate::AuthorizeToolCall {
                    tool_call_id: "action-test-1".to_string(),
                    option_id: "allow".to_string(),
                    option_kind: "AllowOnce".to_string(),
                }
                .boxed_clone(),
                cx,
            );
        });

        cx.run_until_parked();

        // Verify tool call is no longer waiting for confirmation (was authorized)
        conversation_view.read_with(cx, |conversation_view, cx| {
            let tool_call = conversation_view.pending_tool_call(cx);
            assert!(
                tool_call.is_none(),
                "Tool call should no longer be waiting for confirmation after AuthorizeToolCall action"
            );
        });
    }

    #[gpui::test]
    async fn test_authorize_tool_call_action_with_pattern_option(cx: &mut TestAppContext) {
        init_test(cx);

        let tool_call_id = acp::ToolCallId::new("pattern-action-test-1");
        let tool_call =
            acp::ToolCall::new(tool_call_id.clone(), "Run `npm install`").kind(acp::ToolKind::Edit);

        let permission_options =
            ToolPermissionContext::new(TerminalTool::NAME, vec!["npm install".to_string()])
                .build_permission_options();

        let connection =
            StubAgentConnection::new().with_permission_requests(HashMap::from_iter([(
                tool_call_id.clone(),
                permission_options.clone(),
            )]));

        connection.set_next_prompt_updates(vec![acp::SessionUpdate::ToolCall(tool_call)]);

        let (conversation_view, cx) =
            setup_conversation_view(StubAgentServer::new(connection), cx).await;
        add_to_workspace(conversation_view.clone(), cx);

        cx.update(|_window, cx| {
            AgentSettings::override_global(
                AgentSettings {
                    notify_when_agent_waiting: NotifyWhenAgentWaiting::Never,
                    ..AgentSettings::get_global(cx).clone()
                },
                cx,
            );
        });

        let message_editor = message_editor(&conversation_view, cx);
        message_editor.update_in(cx, |editor, window, cx| {
            editor.set_text("Install dependencies", window, cx);
        });

        active_thread(&conversation_view, cx)
            .update_in(cx, |view, window, cx| view.send(window, cx));

        cx.run_until_parked();

        // Find the pattern option ID (the choice with non-empty sub_patterns)
        let pattern_option = match &permission_options {
            PermissionOptions::Dropdown(choices) => choices
                .iter()
                .find(|choice| !choice.sub_patterns.is_empty())
                .map(|choice| &choice.allow)
                .expect("Should have a pattern option for npm command"),
            _ => panic!("Expected dropdown permission options"),
        };

        // Dispatch action with the pattern option (simulating "Always allow `npm` commands")
        conversation_view.update_in(cx, |_, window, cx| {
            window.dispatch_action(
                crate::AuthorizeToolCall {
                    tool_call_id: "pattern-action-test-1".to_string(),
                    option_id: pattern_option.option_id.0.to_string(),
                    option_kind: "AllowAlways".to_string(),
                }
                .boxed_clone(),
                cx,
            );
        });

        cx.run_until_parked();

        // Verify tool call was authorized
        conversation_view.read_with(cx, |conversation_view, cx| {
            let tool_call = conversation_view.pending_tool_call(cx);
            assert!(
                tool_call.is_none(),
                "Tool call should be authorized after selecting pattern option"
            );
        });
    }

    #[gpui::test]
    async fn test_granularity_selection_updates_state(cx: &mut TestAppContext) {
        init_test(cx);

        let tool_call_id = acp::ToolCallId::new("granularity-test-1");
        let tool_call =
            acp::ToolCall::new(tool_call_id.clone(), "Run `cargo build`").kind(acp::ToolKind::Edit);

        let permission_options =
            ToolPermissionContext::new(TerminalTool::NAME, vec!["cargo build".to_string()])
                .build_permission_options();

        let connection =
            StubAgentConnection::new().with_permission_requests(HashMap::from_iter([(
                tool_call_id.clone(),
                permission_options.clone(),
            )]));

        connection.set_next_prompt_updates(vec![acp::SessionUpdate::ToolCall(tool_call)]);

        let (thread_view, cx) = setup_conversation_view(StubAgentServer::new(connection), cx).await;
        add_to_workspace(thread_view.clone(), cx);

        cx.update(|_window, cx| {
            AgentSettings::override_global(
                AgentSettings {
                    notify_when_agent_waiting: NotifyWhenAgentWaiting::Never,
                    ..AgentSettings::get_global(cx).clone()
                },
                cx,
            );
        });

        let message_editor = message_editor(&thread_view, cx);
        message_editor.update_in(cx, |editor, window, cx| {
            editor.set_text("Build the project", window, cx);
        });

        active_thread(&thread_view, cx).update_in(cx, |view, window, cx| view.send(window, cx));

        cx.run_until_parked();

        // Verify default granularity is the last option (index 2 = "今回のみ")
        thread_view.read_with(cx, |thread_view, cx| {
            let state = thread_view.active_thread().unwrap();
            let selected = state.read(cx).permission_selections.get(&tool_call_id);
            assert!(
                selected.is_none(),
                "Should have no selection initially (defaults to last)"
            );
        });

        // Select the first option (index 0 = "ターミナルは常に")
        thread_view.update_in(cx, |_, window, cx| {
            window.dispatch_action(
                crate::SelectPermissionGranularity {
                    tool_call_id: "granularity-test-1".to_string(),
                    index: 0,
                }
                .boxed_clone(),
                cx,
            );
        });

        cx.run_until_parked();

        // Verify the selection was updated
        thread_view.read_with(cx, |thread_view, cx| {
            let state = thread_view.active_thread().unwrap();
            let selected = state.read(cx).permission_selections.get(&tool_call_id);
            assert_eq!(
                selected.and_then(|s| s.choice_index()),
                Some(0),
                "Should have selected index 0"
            );
        });
    }

    #[gpui::test]
    async fn test_allow_button_uses_selected_granularity(cx: &mut TestAppContext) {
        init_test(cx);

        let tool_call_id = acp::ToolCallId::new("allow-granularity-test-1");
        let tool_call =
            acp::ToolCall::new(tool_call_id.clone(), "Run `npm install`").kind(acp::ToolKind::Edit);

        let permission_options =
            ToolPermissionContext::new(TerminalTool::NAME, vec!["npm install".to_string()])
                .build_permission_options();

        // Verify we have the expected options
        let PermissionOptions::Dropdown(choices) = &permission_options else {
            panic!("Expected dropdown permission options");
        };

        assert_eq!(choices.len(), 3);
        assert!(
            choices[0]
                .allow
                .option_id
                .0
                .contains("always_allow:terminal")
        );
        assert!(
            choices[1]
                .allow
                .option_id
                .0
                .contains("always_allow:terminal")
        );
        assert!(!choices[1].sub_patterns.is_empty());
        assert_eq!(choices[2].allow.option_id.0.as_ref(), "allow");

        let connection =
            StubAgentConnection::new().with_permission_requests(HashMap::from_iter([(
                tool_call_id.clone(),
                permission_options.clone(),
            )]));

        connection.set_next_prompt_updates(vec![acp::SessionUpdate::ToolCall(tool_call)]);

        let (thread_view, cx) = setup_conversation_view(StubAgentServer::new(connection), cx).await;
        add_to_workspace(thread_view.clone(), cx);

        cx.update(|_window, cx| {
            AgentSettings::override_global(
                AgentSettings {
                    notify_when_agent_waiting: NotifyWhenAgentWaiting::Never,
                    ..AgentSettings::get_global(cx).clone()
                },
                cx,
            );
        });

        let message_editor = message_editor(&thread_view, cx);
        message_editor.update_in(cx, |editor, window, cx| {
            editor.set_text("Install dependencies", window, cx);
        });

        active_thread(&thread_view, cx).update_in(cx, |view, window, cx| view.send(window, cx));

        cx.run_until_parked();

        // Select the pattern option (index 1 = "`npm` コマンドは常に")
        thread_view.update_in(cx, |_, window, cx| {
            window.dispatch_action(
                crate::SelectPermissionGranularity {
                    tool_call_id: "allow-granularity-test-1".to_string(),
                    index: 1,
                }
                .boxed_clone(),
                cx,
            );
        });

        cx.run_until_parked();

        // Simulate clicking the Allow button by dispatching AllowOnce action
        // which should use the selected granularity
        active_thread(&thread_view, cx).update_in(cx, |view, window, cx| {
            view.allow_once(&AllowOnce, window, cx)
        });

        cx.run_until_parked();

        // Verify tool call was authorized
        thread_view.read_with(cx, |thread_view, cx| {
            let tool_call = thread_view.pending_tool_call(cx);
            assert!(
                tool_call.is_none(),
                "Tool call should be authorized after Allow with pattern granularity"
            );
        });
    }

    #[gpui::test]
    async fn test_deny_button_uses_selected_granularity(cx: &mut TestAppContext) {
        init_test(cx);

        let tool_call_id = acp::ToolCallId::new("deny-granularity-test-1");
        let tool_call =
            acp::ToolCall::new(tool_call_id.clone(), "Run `git push`").kind(acp::ToolKind::Edit);

        let permission_options =
            ToolPermissionContext::new(TerminalTool::NAME, vec!["git push".to_string()])
                .build_permission_options();

        let connection =
            StubAgentConnection::new().with_permission_requests(HashMap::from_iter([(
                tool_call_id.clone(),
                permission_options.clone(),
            )]));

        connection.set_next_prompt_updates(vec![acp::SessionUpdate::ToolCall(tool_call)]);

        let (conversation_view, cx) =
            setup_conversation_view(StubAgentServer::new(connection), cx).await;
        add_to_workspace(conversation_view.clone(), cx);

        cx.update(|_window, cx| {
            AgentSettings::override_global(
                AgentSettings {
                    notify_when_agent_waiting: NotifyWhenAgentWaiting::Never,
                    ..AgentSettings::get_global(cx).clone()
                },
                cx,
            );
        });

        let message_editor = message_editor(&conversation_view, cx);
        message_editor.update_in(cx, |editor, window, cx| {
            editor.set_text("Push changes", window, cx);
        });

        active_thread(&conversation_view, cx)
            .update_in(cx, |view, window, cx| view.send(window, cx));

        cx.run_until_parked();

        // Use default granularity (last option = "今回のみ")
        // Simulate clicking the Deny button
        active_thread(&conversation_view, cx).update_in(cx, |view, window, cx| {
            view.reject_once(&RejectOnce, window, cx)
        });

        cx.run_until_parked();

        // Verify tool call was rejected (no longer waiting for confirmation)
        conversation_view.read_with(cx, |conversation_view, cx| {
            let tool_call = conversation_view.pending_tool_call(cx);
            assert!(
                tool_call.is_none(),
                "Tool call should be rejected after Deny"
            );
        });
    }

    #[gpui::test]
    async fn test_option_id_transformation_for_allow() {
        let permission_options = ToolPermissionContext::new(
            TerminalTool::NAME,
            vec!["cargo build --release".to_string()],
        )
        .build_permission_options();

        let PermissionOptions::Dropdown(choices) = permission_options else {
            panic!("Expected dropdown permission options");
        };

        let allow_ids: Vec<String> = choices
            .iter()
            .map(|choice| choice.allow.option_id.0.to_string())
            .collect();

        assert!(allow_ids.contains(&"allow".to_string()));
        assert_eq!(
            allow_ids
                .iter()
                .filter(|id| *id == "always_allow:terminal")
                .count(),
            2,
            "Expected two always_allow:terminal IDs (one whole-tool, one pattern with sub_patterns)"
        );
    }

    #[gpui::test]
    async fn test_option_id_transformation_for_deny() {
        let permission_options = ToolPermissionContext::new(
            TerminalTool::NAME,
            vec!["cargo build --release".to_string()],
        )
        .build_permission_options();

        let PermissionOptions::Dropdown(choices) = permission_options else {
            panic!("Expected dropdown permission options");
        };

        let deny_ids: Vec<String> = choices
            .iter()
            .map(|choice| choice.deny.option_id.0.to_string())
            .collect();

        assert!(deny_ids.contains(&"deny".to_string()));
        assert_eq!(
            deny_ids
                .iter()
                .filter(|id| *id == "always_deny:terminal")
                .count(),
            2,
            "Expected two always_deny:terminal IDs (one whole-tool, one pattern with sub_patterns)"
        );
    }

    fn flat_allow_deny_options() -> PermissionOptions {
        PermissionOptions::Flat(vec![
            acp::PermissionOption::new(
                acp::PermissionOptionId::new("allow"),
                "Yes",
                acp::PermissionOptionKind::AllowOnce,
            ),
            acp::PermissionOption::new(
                acp::PermissionOptionId::new("deny"),
                "No",
                acp::PermissionOptionKind::RejectOnce,
            ),
        ])
    }

    fn sandbox_permission_options() -> PermissionOptions {
        PermissionOptions::Flat(vec![
            acp::PermissionOption::new(
                acp::PermissionOptionId::new("allow"),
                "許可 (今回のみ)",
                acp::PermissionOptionKind::AllowOnce,
            ),
            acp::PermissionOption::new(
                acp::PermissionOptionId::new("allow_thread"),
                "許可 (このスレッド)",
                acp::PermissionOptionKind::AllowAlways,
            ),
            acp::PermissionOption::new(
                acp::PermissionOptionId::new("allow_always"),
                "常に許可",
                acp::PermissionOptionKind::AllowAlways,
            ),
            acp::PermissionOption::new(
                acp::PermissionOptionId::new("deny"),
                "拒否",
                acp::PermissionOptionKind::RejectOnce,
            ),
        ])
    }

    /// On the sandbox *escalation* card, 「常に許可」 widens the sandbox for
    /// every thread (the command stays wrapped), so the shortcut picks it over
    /// 「許可 (このスレッド)」.
    #[test]
    fn permission_option_for_action_prefers_explicit_sandbox_allow_always() {
        let options = sandbox_permission_options();

        let option =
            super::permission_option_for_action(&options, acp::PermissionOptionKind::AllowAlways)
                .unwrap();

        assert_eq!(option.option_id.0.as_ref(), "allow_always");
    }

    /// ADR-F46: the card for a command outside the sandbox has no "always"
    /// choice, so the allow-always shortcut does nothing there instead of
    /// saving a standing allow rule.
    #[test]
    fn allow_always_shortcut_does_nothing_on_the_unsandboxed_command_card() {
        let options = agent::unsandboxed_command_permission_options();

        assert!(
            super::permission_option_for_action(&options, acp::PermissionOptionKind::AllowAlways)
                .is_none()
        );
        let allow_once = super::resolve_outcome_from_selection(&options, None, true)
            .expect("the one-time allow stays reachable");
        assert_eq!(allow_once.option_id.0.as_ref(), "allow");
    }

    /// When the sandbox can't be created the card offers only "retry" and
    /// "cancel", so neither allow shortcut picks anything and the reject
    /// shortcut cancels.
    #[test]
    fn allow_shortcuts_do_nothing_on_the_sandbox_retry_card() {
        let options = agent::sandbox_retry_permission_options(0);

        for kind in [
            acp::PermissionOptionKind::AllowAlways,
            acp::PermissionOptionKind::AllowOnce,
        ] {
            assert!(
                super::permission_option_for_action(&options, kind).is_none(),
                "{kind:?}"
            );
        }
        assert!(super::resolve_outcome_from_selection(&options, None, true).is_none());
        let cancel = super::resolve_outcome_from_selection(&options, None, false)
            .expect("the reject shortcut cancels");
        assert_eq!(cancel.option_id.0.as_ref(), "deny");
    }

    #[test]
    fn resolve_outcome_from_selection_flat_allow_picks_allow_once() {
        let options = flat_allow_deny_options();

        let outcome = super::resolve_outcome_from_selection(&options, None, true).unwrap();

        assert_eq!(outcome.option_id.0.as_ref(), "allow");
        assert_eq!(outcome.option_kind, acp::PermissionOptionKind::AllowOnce);
    }

    #[test]
    fn resolve_outcome_from_selection_flat_deny_picks_reject_once() {
        let options = flat_allow_deny_options();

        let outcome = super::resolve_outcome_from_selection(&options, None, false).unwrap();

        assert_eq!(outcome.option_id.0.as_ref(), "deny");
        assert_eq!(outcome.option_kind, acp::PermissionOptionKind::RejectOnce);
    }

    #[test]
    fn resolve_outcome_from_selection_flat_ignores_selection() {
        let options = flat_allow_deny_options();
        // Flat options never consult the granularity choice, even if one is set.
        let selection = thread_view::PermissionSelection::Choice(42);

        let outcome =
            super::resolve_outcome_from_selection(&options, Some(&selection), true).unwrap();

        assert_eq!(outcome.option_id.0.as_ref(), "allow");
    }

    #[test]
    fn resolve_outcome_from_selection_dropdown_defaults_to_last_choice_when_no_selection() {
        let options =
            ToolPermissionContext::new(TerminalTool::NAME, vec!["cargo build".to_string()])
                .build_permission_options();

        let outcome = super::resolve_outcome_from_selection(&options, None, true).unwrap();

        // Last choice is "今回のみ" → option_id "allow".
        assert_eq!(outcome.option_id.0.as_ref(), "allow");
        assert_eq!(outcome.option_kind, acp::PermissionOptionKind::AllowOnce);
    }

    #[test]
    fn resolve_outcome_from_selection_dropdown_uses_selected_choice() {
        let options =
            ToolPermissionContext::new(TerminalTool::NAME, vec!["cargo build".to_string()])
                .build_permission_options();
        let selection = thread_view::PermissionSelection::Choice(0);

        let outcome =
            super::resolve_outcome_from_selection(&options, Some(&selection), true).unwrap();

        // Choice 0 = "ターミナルは常に".
        assert!(outcome.option_id.0.contains("always_allow:terminal"));
        assert_eq!(outcome.option_kind, acp::PermissionOptionKind::AllowAlways);
    }

    #[test]
    fn resolve_outcome_from_selection_dropdown_out_of_range_falls_back_to_last() {
        let options =
            ToolPermissionContext::new(TerminalTool::NAME, vec!["cargo build".to_string()])
                .build_permission_options();
        let selection = thread_view::PermissionSelection::Choice(999);

        let outcome =
            super::resolve_outcome_from_selection(&options, Some(&selection), true).unwrap();

        // choices.get(999) is None, falls back to choices.last() → "今回のみ".
        assert_eq!(outcome.option_id.0.as_ref(), "allow");
    }

    #[test]
    fn resolve_outcome_from_selection_pattern_mode_with_empty_checked_falls_back_to_last_choice() {
        // Pipeline commands produce `DropdownWithPatterns`, which is required for
        // `SelectedPatterns` to be meaningful.
        let options = ToolPermissionContext::new(
            TerminalTool::NAME,
            vec!["cargo test 2>&1 | tail".to_string()],
        )
        .build_permission_options();
        assert!(matches!(
            options,
            PermissionOptions::DropdownWithPatterns { .. }
        ));
        // Pattern mode with zero checked patterns: `build_outcome_for_checked_patterns`
        // returns None, so we fall through to `choice_index()` (which is None for
        // `SelectedPatterns`) and default to `choices.last()`.
        let selection = thread_view::PermissionSelection::SelectedPatterns(vec![]);

        let outcome =
            super::resolve_outcome_from_selection(&options, Some(&selection), true).unwrap();

        assert_eq!(outcome.option_id.0.as_ref(), "allow");
        assert_eq!(outcome.option_kind, acp::PermissionOptionKind::AllowOnce);
    }

    #[test]
    fn resolve_outcome_from_selection_pattern_mode_with_checked_uses_always_with_params() {
        let options = ToolPermissionContext::new(
            TerminalTool::NAME,
            vec!["cargo test 2>&1 | tail".to_string()],
        )
        .build_permission_options();
        assert!(matches!(
            options,
            PermissionOptions::DropdownWithPatterns { .. }
        ));
        let selection = thread_view::PermissionSelection::SelectedPatterns(vec![0]);

        let outcome =
            super::resolve_outcome_from_selection(&options, Some(&selection), true).unwrap();

        assert_eq!(outcome.option_kind, acp::PermissionOptionKind::AllowAlways);
        assert!(
            outcome.params.is_some(),
            "checked patterns should attach terminal params"
        );
    }

    #[gpui::test]
    async fn test_manually_editing_title_updates_acp_thread_title(cx: &mut TestAppContext) {
        init_test(cx);

        let (conversation_view, cx) =
            setup_conversation_view(StubAgentServer::default_response(), cx).await;
        add_to_workspace(conversation_view.clone(), cx);

        let active = active_thread(&conversation_view, cx);
        let title_editor = cx.read(|cx| active.read(cx).title_editor.clone());
        let thread = cx.read(|cx| active.read(cx).thread.clone());

        title_editor.read_with(cx, |editor, cx| {
            assert!(!editor.read_only(cx));
        });

        cx.focus(&conversation_view);
        cx.focus(&title_editor);

        cx.dispatch_action(editor::actions::DeleteLine);
        cx.simulate_input("My Custom Title");

        cx.run_until_parked();

        title_editor.read_with(cx, |editor, cx| {
            assert_eq!(editor.text(cx), "My Custom Title");
        });
        thread.read_with(cx, |thread, _cx| {
            assert_eq!(thread.title(), Some("My Custom Title".into()));
        });
    }

    #[gpui::test]
    async fn test_max_tokens_error_is_rendered(cx: &mut TestAppContext) {
        init_test(cx);

        let connection = StubAgentConnection::new();

        let (conversation_view, cx) =
            setup_conversation_view(StubAgentServer::new(connection.clone()), cx).await;

        let message_editor = message_editor(&conversation_view, cx);
        message_editor.update_in(cx, |editor, window, cx| {
            editor.set_text("Some prompt", window, cx);
        });
        active_thread(&conversation_view, cx)
            .update_in(cx, |view, window, cx| view.send(window, cx));

        let session_id = conversation_view.read_with(cx, |view, cx| {
            view.active_thread()
                .unwrap()
                .read(cx)
                .thread
                .read(cx)
                .session_id()
                .clone()
        });

        cx.run_until_parked();

        cx.update(|_, _cx| {
            connection.end_turn(session_id, acp::StopReason::MaxTokens);
        });

        cx.run_until_parked();

        conversation_view.read_with(cx, |conversation_view, cx| {
            let state = conversation_view.active_thread().unwrap();
            let error = &state.read(cx).thread_error;
            assert!(
                matches!(error, Some(ThreadError::MaxOutputTokens)),
                "Expected ThreadError::MaxOutputTokens, got: {:?}",
                error.is_some()
            );
        });
    }

    fn create_test_acp_thread(
        parent_session_id: Option<acp::SessionId>,
        session_id: &str,
        connection: Rc<dyn AgentConnection>,
        project: Entity<Project>,
        cx: &mut App,
    ) -> Entity<AcpThread> {
        let action_log = cx.new(|_| ActionLog::new(project.clone()));
        cx.new(|cx| {
            AcpThread::new(
                parent_session_id,
                None,
                None,
                connection,
                project,
                action_log,
                acp::SessionId::new(session_id),
                watch::Receiver::constant(acp::PromptCapabilities::new()),
                cx,
            )
        })
    }

    fn request_test_tool_authorization(
        thread: &Entity<AcpThread>,
        tool_call_id: &str,
        option_id: &str,
        cx: &mut TestAppContext,
    ) -> Task<acp_thread::RequestPermissionOutcome> {
        let tool_call_id = acp::ToolCallId::new(tool_call_id);
        let label = format!("Tool {tool_call_id}");
        let option_id = acp::PermissionOptionId::new(option_id);
        cx.update(|cx| {
            thread.update(cx, |thread, cx| {
                thread
                    .request_tool_call_authorization(
                        acp::ToolCall::new(tool_call_id, label)
                            .kind(acp::ToolKind::Edit)
                            .into(),
                        PermissionOptions::Flat(vec![acp::PermissionOption::new(
                            option_id,
                            "Allow",
                            acp::PermissionOptionKind::AllowOnce,
                        )]),
                        acp_thread::AuthorizationKind::PermissionGrant,
                        cx,
                    )
                    .unwrap()
            })
        })
    }

    #[gpui::test]
    async fn test_conversation_multiple_tool_calls_fifo_ordering(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let connection: Rc<dyn AgentConnection> = Rc::new(StubAgentConnection::new());

        let session_id = acp::SessionId::new("session-1");
        let (thread, conversation) = cx.update(|cx| {
            let thread =
                create_test_acp_thread(None, "session-1", connection.clone(), project.clone(), cx);
            let conversation = cx.new(|cx| {
                let mut conversation = Conversation::default();
                conversation.register_thread(thread.clone(), cx);
                conversation
            });
            (thread, conversation)
        });

        let _task1 = request_test_tool_authorization(&thread, "tc-1", "allow-1", cx);
        let _task2 = request_test_tool_authorization(&thread, "tc-2", "allow-2", cx);

        cx.read(|cx| {
            let (_, tool_call_id, _) = conversation
                .read(cx)
                .pending_tool_call(&session_id, cx)
                .expect("Expected a pending tool call");
            assert_eq!(tool_call_id, acp::ToolCallId::new("tc-1"));
        });

        cx.update(|cx| {
            conversation.update(cx, |conversation, cx| {
                conversation.authorize_tool_call(
                    session_id.clone(),
                    acp::ToolCallId::new("tc-1"),
                    SelectedPermissionOutcome::new(
                        acp::PermissionOptionId::new("allow-1"),
                        acp::PermissionOptionKind::AllowOnce,
                    ),
                    cx,
                );
            });
        });

        cx.run_until_parked();

        cx.read(|cx| {
            let (_, tool_call_id, _) = conversation
                .read(cx)
                .pending_tool_call(&session_id, cx)
                .expect("Expected tc-2 to be pending after tc-1 was authorized");
            assert_eq!(tool_call_id, acp::ToolCallId::new("tc-2"));
        });

        cx.update(|cx| {
            conversation.update(cx, |conversation, cx| {
                conversation.authorize_tool_call(
                    session_id.clone(),
                    acp::ToolCallId::new("tc-2"),
                    SelectedPermissionOutcome::new(
                        acp::PermissionOptionId::new("allow-2"),
                        acp::PermissionOptionKind::AllowOnce,
                    ),
                    cx,
                );
            });
        });

        cx.run_until_parked();

        cx.read(|cx| {
            assert!(
                conversation
                    .read(cx)
                    .pending_tool_call(&session_id, cx)
                    .is_none(),
                "Expected no pending tool calls after both were authorized"
            );
        });
    }

    #[gpui::test]
    async fn test_conversation_subagent_scoped_pending_tool_call(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let connection: Rc<dyn AgentConnection> = Rc::new(StubAgentConnection::new());

        let parent_session_id = acp::SessionId::new("parent");
        let subagent_session_id = acp::SessionId::new("subagent");
        let (parent_thread, subagent_thread, conversation) = cx.update(|cx| {
            let parent_thread =
                create_test_acp_thread(None, "parent", connection.clone(), project.clone(), cx);
            let subagent_thread = create_test_acp_thread(
                Some(acp::SessionId::new("parent")),
                "subagent",
                connection.clone(),
                project.clone(),
                cx,
            );
            let conversation = cx.new(|cx| {
                let mut conversation = Conversation::default();
                conversation.register_thread(parent_thread.clone(), cx);
                conversation.register_thread(subagent_thread.clone(), cx);
                conversation
            });
            (parent_thread, subagent_thread, conversation)
        });

        let _parent_task =
            request_test_tool_authorization(&parent_thread, "parent-tc", "allow-parent", cx);
        let _subagent_task =
            request_test_tool_authorization(&subagent_thread, "subagent-tc", "allow-subagent", cx);

        // Querying with the subagent's session ID returns only the
        // subagent's own tool call (subagent path is scoped to its session)
        cx.read(|cx| {
            let (returned_session_id, tool_call_id, _) = conversation
                .read(cx)
                .pending_tool_call(&subagent_session_id, cx)
                .expect("Expected subagent's pending tool call");
            assert_eq!(returned_session_id, subagent_session_id);
            assert_eq!(tool_call_id, acp::ToolCallId::new("subagent-tc"));
        });

        // Querying with the parent's session ID returns the first pending
        // request in FIFO order across all sessions
        cx.read(|cx| {
            let (returned_session_id, tool_call_id, _) = conversation
                .read(cx)
                .pending_tool_call(&parent_session_id, cx)
                .expect("Expected a pending tool call from parent query");
            assert_eq!(returned_session_id, parent_session_id);
            assert_eq!(tool_call_id, acp::ToolCallId::new("parent-tc"));
        });
    }

    #[gpui::test]
    async fn test_conversation_parent_pending_tool_call_returns_first_across_threads(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let connection: Rc<dyn AgentConnection> = Rc::new(StubAgentConnection::new());

        let session_id_a = acp::SessionId::new("thread-a");
        let session_id_b = acp::SessionId::new("thread-b");
        let (thread_a, thread_b, conversation) = cx.update(|cx| {
            let thread_a =
                create_test_acp_thread(None, "thread-a", connection.clone(), project.clone(), cx);
            let thread_b =
                create_test_acp_thread(None, "thread-b", connection.clone(), project.clone(), cx);
            let conversation = cx.new(|cx| {
                let mut conversation = Conversation::default();
                conversation.register_thread(thread_a.clone(), cx);
                conversation.register_thread(thread_b.clone(), cx);
                conversation
            });
            (thread_a, thread_b, conversation)
        });

        let _task_a = request_test_tool_authorization(&thread_a, "tc-a", "allow-a", cx);
        let _task_b = request_test_tool_authorization(&thread_b, "tc-b", "allow-b", cx);

        // Both threads are non-subagent, so pending_tool_call always returns
        // the first entry from permission_requests (FIFO across all sessions)
        cx.read(|cx| {
            let (returned_session_id, tool_call_id, _) = conversation
                .read(cx)
                .pending_tool_call(&session_id_a, cx)
                .expect("Expected a pending tool call");
            assert_eq!(returned_session_id, session_id_a);
            assert_eq!(tool_call_id, acp::ToolCallId::new("tc-a"));
        });

        // Querying with thread-b also returns thread-a's tool call,
        // because non-subagent queries always use permission_requests.first()
        cx.read(|cx| {
            let (returned_session_id, tool_call_id, _) = conversation
                .read(cx)
                .pending_tool_call(&session_id_b, cx)
                .expect("Expected a pending tool call from thread-b query");
            assert_eq!(
                returned_session_id, session_id_a,
                "Non-subagent queries always return the first pending request in FIFO order"
            );
            assert_eq!(tool_call_id, acp::ToolCallId::new("tc-a"));
        });

        // After authorizing thread-a's tool call, thread-b's becomes first
        cx.update(|cx| {
            conversation.update(cx, |conversation, cx| {
                conversation.authorize_tool_call(
                    session_id_a.clone(),
                    acp::ToolCallId::new("tc-a"),
                    SelectedPermissionOutcome::new(
                        acp::PermissionOptionId::new("allow-a"),
                        acp::PermissionOptionKind::AllowOnce,
                    ),
                    cx,
                );
            });
        });

        cx.run_until_parked();

        cx.read(|cx| {
            let (returned_session_id, tool_call_id, _) = conversation
                .read(cx)
                .pending_tool_call(&session_id_b, cx)
                .expect("Expected thread-b's tool call after thread-a's was authorized");
            assert_eq!(returned_session_id, session_id_b);
            assert_eq!(tool_call_id, acp::ToolCallId::new("tc-b"));
        });
    }

    /// Set up a `ConversationView` whose active thread has a single tool call
    /// awaiting permission. Returns the conversation view, its active
    /// `ThreadView`, and the entry index of the tool call within the thread.
    async fn setup_pending_permission_thread<'a>(
        tool_call_id: &str,
        cx: &'a mut TestAppContext,
    ) -> (
        Entity<ConversationView>,
        Entity<ThreadView>,
        usize,
        &'a mut VisualTestContext,
    ) {
        let tool_call_id_value = acp::ToolCallId::new(tool_call_id);
        let tool_call = acp::ToolCall::new(tool_call_id_value.clone(), "Run something")
            .kind(acp::ToolKind::Edit);

        let connection =
            StubAgentConnection::new().with_permission_requests(HashMap::from_iter([(
                tool_call_id_value.clone(),
                PermissionOptions::Flat(vec![acp::PermissionOption::new(
                    "allow",
                    "Allow",
                    acp::PermissionOptionKind::AllowOnce,
                )]),
            )]));
        connection.set_next_prompt_updates(vec![acp::SessionUpdate::ToolCall(tool_call)]);

        let (conversation_view, cx) =
            setup_conversation_view(StubAgentServer::new(connection), cx).await;
        add_to_workspace(conversation_view.clone(), cx);

        cx.update(|_window, cx| {
            AgentSettings::override_global(
                AgentSettings {
                    notify_when_agent_waiting: NotifyWhenAgentWaiting::Never,
                    ..AgentSettings::get_global(cx).clone()
                },
                cx,
            );
        });

        let message_editor = message_editor(&conversation_view, cx);
        message_editor.update_in(cx, |editor, window, cx| {
            editor.set_text("Hello", window, cx);
        });

        active_thread(&conversation_view, cx)
            .update_in(cx, |view, window, cx| view.send(window, cx));

        cx.run_until_parked();

        let thread_view = active_thread(&conversation_view, cx);
        let entry_ix = thread_view.read_with(cx, |view, cx| {
            view.thread
                .read(cx)
                .entries()
                .iter()
                .position(|entry| {
                    matches!(
                        entry,
                        acp_thread::AgentThreadEntry::ToolCall(call)
                            if call.id == tool_call_id_value
                    )
                })
                .expect("tool call entry should exist after run_until_parked")
        });

        (conversation_view, thread_view, entry_ix, cx)
    }

    struct TestListView {
        list_state: ListState,
    }

    impl Render for TestListView {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            list(self.list_state.clone(), |_, _, _| {
                div().h(px(20.0)).w_full().into_any_element()
            })
            .size_full()
        }
    }

    fn draw_thread_list_at(
        thread_view: &Entity<ThreadView>,
        scroll_top: ListOffset,
        cx: &mut VisualTestContext,
    ) {
        let list_state = thread_view.read_with(cx, |view, _cx| view.list_state.clone());
        list_state.scroll_to(scroll_top);
        cx.draw(
            point(px(0.0), px(0.0)),
            size(px(100.0), px(20.0)),
            |_, cx| {
                cx.new(|_| TestListView {
                    list_state: list_state.clone(),
                })
                .into_any_element()
            },
        );
    }

    #[gpui::test]
    async fn test_permission_row_hidden_when_inline_bounds_unavailable(cx: &mut TestAppContext) {
        init_test(cx);

        let (_view, thread_view, entry_ix, cx) =
            setup_pending_permission_thread("perm-no-bounds", cx).await;

        // Pin the scroll top to the entry so it isn't treated as above the
        // viewport, forcing the unmeasured-bounds path we want to exercise.
        thread_view.read_with(cx, |view, _cx| {
            view.list_state.scroll_to(ListOffset {
                item_ix: entry_ix,
                offset_in_item: px(0.0),
            });
        });
        thread_view.update_in(cx, |view, window, cx| {
            assert!(
                view.render_main_agent_awaiting_permission(window, cx)
                    .is_none(),
                "Floating row should stay hidden until the inline prompt has known list bounds"
            );
        });
    }

    #[gpui::test]
    async fn test_pending_tool_call_for_session_scopes_to_that_session(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let connection: Rc<dyn AgentConnection> = Rc::new(StubAgentConnection::new());

        let session_id_a = acp::SessionId::new("thread-a");
        let session_id_b = acp::SessionId::new("thread-b");
        let (thread_a, thread_b, conversation) = cx.update(|cx| {
            let thread_a =
                create_test_acp_thread(None, "thread-a", connection.clone(), project.clone(), cx);
            let thread_b =
                create_test_acp_thread(None, "thread-b", connection.clone(), project.clone(), cx);
            let conversation = cx.new(|cx| {
                let mut conversation = Conversation::default();
                conversation.register_thread(thread_a.clone(), cx);
                conversation.register_thread(thread_b.clone(), cx);
                conversation
            });
            (thread_a, thread_b, conversation)
        });

        // Pending tool calls in both threads. Unlike `pending_tool_call`,
        // `pending_tool_call_for_session` must not fall back across threads.
        let _task_a = request_test_tool_authorization(&thread_a, "tc-a", "allow-a", cx);
        let _task_b = request_test_tool_authorization(&thread_b, "tc-b", "allow-b", cx);

        cx.read(|cx| {
            let tool_call_id_a = conversation
                .read(cx)
                .pending_tool_call_for_session(&session_id_a, cx)
                .expect("Expected a pending tool call in thread A");
            assert_eq!(tool_call_id_a, acp::ToolCallId::new("tc-a"));

            let tool_call_id_b = conversation
                .read(cx)
                .pending_tool_call_for_session(&session_id_b, cx)
                .expect("Expected a pending tool call in thread B");
            assert_eq!(tool_call_id_b, acp::ToolCallId::new("tc-b"));
        });
    }

    #[gpui::test]
    async fn test_permission_row_scroll_to_dismisses_row(cx: &mut TestAppContext) {
        init_test(cx);

        let (_view, thread_view, entry_ix, cx) =
            setup_pending_permission_thread("perm-scroll", cx).await;

        // Start off-screen below the viewport. The row is visible because the
        // item has bounds that do not intersect the viewport.
        draw_thread_list_at(
            &thread_view,
            ListOffset {
                item_ix: 0,
                offset_in_item: px(0.0),
            },
            cx,
        );
        thread_view.read_with(cx, |view, _cx| {
            assert!(
                view.list_state.bounds_for_item(entry_ix).is_some(),
                "The tool call entry must be measured for this test to exercise the\
                 \"entry below viewport\" branch. If list overdraw stops measuring\
                 offscreen items, this test needs to drive measurement another way."
            );
        });
        thread_view.update_in(cx, |view, window, cx| {
            assert!(
                view.render_main_agent_awaiting_permission(window, cx)
                    .is_some()
            );
        });

        // Simulate clicking "Scroll to": the list scrolls to the entry and the
        // measured item bounds intersect the viewport.
        draw_thread_list_at(
            &thread_view,
            ListOffset {
                item_ix: entry_ix,
                offset_in_item: px(0.0),
            },
            cx,
        );

        thread_view.update_in(cx, |view, window, cx| {
            assert!(
                view.render_main_agent_awaiting_permission(window, cx)
                    .is_none(),
                "Floating row should disappear after scrolling brings the inline prompt into view"
            );
        });
    }

    #[gpui::test]
    async fn test_permission_row_does_not_flicker_when_activity_bar_squeezes_list(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);

        let (_view, thread_view, _entry_ix, cx) =
            setup_pending_permission_thread("perm-flicker", cx).await;

        // Give the pending tool call tall content (like a full plan awaiting
        // approval), so the floating row embedding it dwarfs the panel.
        let thread = thread_view.read_with(cx, |view, _cx| view.thread.clone());
        thread.update(cx, |thread, cx| {
            thread
                .handle_session_update(
                    acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
                        acp::ToolCallId::new("perm-flicker"),
                        acp::ToolCallUpdateFields::new().content(vec![
                            acp::ToolCallContent::Content(acp::Content::new(
                                acp::ContentBlock::Text(acp::TextContent::new(
                                    "Plan step\n\n".repeat(100),
                                )),
                            )),
                        ]),
                    )),
                    cx,
                )
                .expect("tool call content update should be accepted");
        });
        cx.run_until_parked();

        // Park the inline prompt below the viewport so the floating row renders.
        thread_view.read_with(cx, |view, _cx| {
            view.list_state.scroll_to(ListOffset {
                item_ix: 0,
                offset_in_item: px(0.0),
            });
        });

        // Drive several real window draws. Each draw lays out the activity bar
        // (containing the floating row) and the conversation list together, so
        // the row's height feeds back into the list viewport height that the
        // next frame's visibility decision is based on. Since showing the row
        // squeezes the list to zero height, a decision that treats a
        // zero-height viewport as "unknown" makes the row's visibility
        // oscillate from frame to frame, flickering between the conversation
        // and the permission prompt.
        let mut row_visibility = Vec::new();
        for _ in 0..4 {
            thread_view.update(cx, |_, cx| cx.notify());
            cx.run_until_parked();
            thread_view.update_in(cx, |view, window, cx| {
                row_visibility.push(
                    view.render_main_agent_awaiting_permission(window, cx)
                        .is_some(),
                );
            });
        }
        assert_eq!(
            row_visibility,
            vec![true; 4],
            "Floating row visibility must be stable across frames (false entries mean flicker)"
        );
    }

    #[gpui::test]
    async fn test_permission_row_shown_when_inline_prompt_is_above_viewport(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);

        let (_view, thread_view, entry_ix, cx) =
            setup_pending_permission_thread("perm-above", cx).await;

        let thread = thread_view.read_with(cx, |view, _cx| view.thread.clone());
        thread.update(cx, |thread, cx| {
            let result = thread.handle_session_update(
                acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                    "More content".into(),
                )),
                cx,
            );
            assert!(
                result.is_ok(),
                "following assistant message should be accepted"
            );
        });

        draw_thread_list_at(
            &thread_view,
            ListOffset {
                item_ix: entry_ix + 1,
                offset_in_item: px(0.0),
            },
            cx,
        );
        thread_view.read_with(cx, |view, _cx| {
            assert!(
                entry_ix < view.list_state.logical_scroll_top().item_ix,
                "The tool call entry should be above the logical scroll top"
            );
        });
        thread_view.update_in(cx, |view, window, cx| {
            assert!(
                view.render_main_agent_awaiting_permission(window, cx)
                    .is_some(),
                "Floating row should be visible when the inline prompt is above the viewport"
            );
        });

        // Scrolling up to the entry brings it back into view.
        draw_thread_list_at(
            &thread_view,
            ListOffset {
                item_ix: entry_ix,
                offset_in_item: px(0.0),
            },
            cx,
        );
        thread_view.update_in(cx, |view, window, cx| {
            assert!(
                view.render_main_agent_awaiting_permission(window, cx)
                    .is_none(),
                "Floating row should disappear after scrolling brings the inline prompt into view"
            );
        });
    }

    #[gpui::test]
    async fn test_permission_row_disappears_when_authorized(cx: &mut TestAppContext) {
        init_test(cx);

        let (conversation_view, thread_view, _entry_ix, cx) =
            setup_pending_permission_thread("perm-allow", cx).await;

        // Park the inline prompt below the viewport so the floating row would render.
        draw_thread_list_at(
            &thread_view,
            ListOffset {
                item_ix: 0,
                offset_in_item: px(0.0),
            },
            cx,
        );
        thread_view.update_in(cx, |view, window, cx| {
            assert!(
                view.render_main_agent_awaiting_permission(window, cx)
                    .is_some(),
                "Floating row should be visible before authorizing"
            );
        });

        // Dispatch the same AuthorizeToolCall action the row's Allow button
        // wires up.
        conversation_view.update_in(cx, |_, window, cx| {
            window.dispatch_action(
                crate::AuthorizeToolCall {
                    tool_call_id: "perm-allow".to_string(),
                    option_id: "allow".to_string(),
                    option_kind: "AllowOnce".to_string(),
                }
                .boxed_clone(),
                cx,
            );
        });
        cx.run_until_parked();

        conversation_view.read_with(cx, |view, cx| {
            assert!(
                view.pending_tool_call(cx).is_none(),
                "Tool call should no longer be pending after Allow is clicked"
            );
        });
        thread_view.update_in(cx, |view, window, cx| {
            assert!(
                view.render_main_agent_awaiting_permission(window, cx)
                    .is_none(),
                "Floating row should disappear once the permission is granted"
            );
        });
    }

    #[gpui::test]
    async fn test_permission_row_ignores_subagent_requests(cx: &mut TestAppContext) {
        init_test(cx);

        // Build a baseline ConversationView with no permission requests, so we
        // have a real `ThreadView` to call `render_main_agent_awaiting_permission` on.
        let (conversation_view, cx) =
            setup_conversation_view(StubAgentServer::default_response(), cx).await;
        add_to_workspace(conversation_view.clone(), cx);

        let message_editor = message_editor(&conversation_view, cx);
        message_editor.update_in(cx, |editor, window, cx| {
            editor.set_text("Hello", window, cx);
        });
        active_thread(&conversation_view, cx)
            .update_in(cx, |view, window, cx| view.send(window, cx));
        cx.run_until_parked();

        let thread_view = active_thread(&conversation_view, cx);
        let parent_session_id =
            thread_view.read_with(cx, |view, cx| view.thread.read(cx).session_id().clone());
        let conversation = thread_view.read_with(cx, |view, _cx| view.conversation.clone());

        // Attach a subagent thread with a pending tool-call permission request.
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let stub: Rc<dyn AgentConnection> = Rc::new(StubAgentConnection::new());
        let subagent_thread = cx.update(|_window, cx| {
            create_test_acp_thread(
                Some(parent_session_id.clone()),
                "subagent",
                stub,
                project,
                cx,
            )
        });
        conversation.update(cx, |conversation, cx| {
            conversation.register_thread(subagent_thread.clone(), cx);
        });
        let _subagent_task =
            request_test_tool_authorization(&subagent_thread, "sub-tc", "allow-sub", cx);
        cx.run_until_parked();

        cx.read(|cx| {
            assert!(
                conversation
                    .read(cx)
                    .pending_tool_call_for_session(&parent_session_id, cx)
                    .is_none(),
                "Subagent requests must not surface as pending in the parent session"
            );
            assert!(
                !conversation
                    .read(cx)
                    .subagents_awaiting_permission(cx)
                    .is_empty(),
                "Subagent permission row should still see the pending request"
            );
        });

        thread_view.update_in(cx, |view, window, cx| {
            assert!(
                view.render_main_agent_awaiting_permission(window, cx)
                    .is_none(),
                "Subagent permission requests should not trigger the main-agent floating row"
            );
        });
    }

    #[gpui::test]
    async fn test_move_queued_message_to_empty_main_editor(cx: &mut TestAppContext) {
        init_test(cx);

        let (conversation_view, cx) =
            setup_conversation_view(StubAgentServer::default_response(), cx).await;

        // Add a plain-text message to the queue directly.
        active_thread(&conversation_view, cx).update_in(cx, |thread, window, cx| {
            thread.add_to_queue(
                vec![acp::ContentBlock::Text(acp::TextContent::new(
                    "queued message".to_string(),
                ))],
                vec![],
                window,
                cx,
            );
            // Main editor must be empty for this path — it is by default, but
            // assert to make the precondition explicit.
            assert!(thread.message_editor.read(cx).is_empty(cx));
            let id = thread.message_queue.first_id().unwrap();
            thread.move_queued_message_to_main_editor(id, None, None, window, cx);
        });

        cx.run_until_parked();

        // Queue should now be empty.
        let queue_len = active_thread(&conversation_view, cx)
            .read_with(cx, |thread, _cx| thread.message_queue.len());
        assert_eq!(queue_len, 0, "Queue should be empty after move");

        // Main editor should contain the queued message text.
        let text = message_editor(&conversation_view, cx).update(cx, |editor, cx| editor.text(cx));
        assert_eq!(
            text, "queued message",
            "Main editor should contain the moved queued message"
        );
    }

    #[gpui::test]
    async fn test_move_queued_message_to_non_empty_main_editor(cx: &mut TestAppContext) {
        init_test(cx);

        let (conversation_view, cx) =
            setup_conversation_view(StubAgentServer::default_response(), cx).await;

        // Seed the main editor with existing content.
        message_editor(&conversation_view, cx).update_in(cx, |editor, window, cx| {
            editor.set_message(
                vec![acp::ContentBlock::Text(acp::TextContent::new(
                    "existing content".to_string(),
                ))],
                window,
                cx,
            );
        });

        // Add a plain-text message to the queue.
        active_thread(&conversation_view, cx).update_in(cx, |thread, window, cx| {
            thread.add_to_queue(
                vec![acp::ContentBlock::Text(acp::TextContent::new(
                    "queued message".to_string(),
                ))],
                vec![],
                window,
                cx,
            );
            let id = thread.message_queue.first_id().unwrap();
            thread.move_queued_message_to_main_editor(id, None, None, window, cx);
        });

        cx.run_until_parked();

        // Queue should now be empty.
        let queue_len = active_thread(&conversation_view, cx)
            .read_with(cx, |thread, _cx| thread.message_queue.len());
        assert_eq!(queue_len, 0, "Queue should be empty after move");

        // Main editor should contain existing content + separator + queued content.
        let text = message_editor(&conversation_view, cx).update(cx, |editor, cx| editor.text(cx));
        assert_eq!(
            text, "existing content\n\nqueued message",
            "Main editor should have existing content and queued message separated by two newlines"
        );
    }

    #[gpui::test]
    async fn test_move_up_in_empty_editor_restores_last_queued_message(cx: &mut TestAppContext) {
        init_test(cx);

        let (conversation_view, cx) =
            setup_conversation_view(StubAgentServer::default_response(), cx).await;
        add_to_workspace(conversation_view.clone(), cx);

        active_thread(&conversation_view, cx).update_in(cx, |thread, window, cx| {
            thread.add_to_queue(
                vec![acp::ContentBlock::Text(acp::TextContent::new(
                    "first queued".to_string(),
                ))],
                vec![],
                window,
                cx,
            );
            thread.add_to_queue(
                vec![acp::ContentBlock::Text(acp::TextContent::new(
                    "second queued".to_string(),
                ))],
                vec![],
                window,
                cx,
            );
        });
        cx.run_until_parked();

        let editor = message_editor(&conversation_view, cx);
        cx.focus(&editor);

        editor.update_in(cx, |_editor, window, cx| {
            window.dispatch_action(Box::new(zed_actions::editor::MoveUp), cx);
        });
        cx.run_until_parked();

        let queue_len = active_thread(&conversation_view, cx)
            .read_with(cx, |thread, _cx| thread.message_queue.len());
        assert_eq!(
            queue_len, 1,
            "Up arrow should pull the last queued message out of the queue"
        );
        let text = editor.update(cx, |editor, cx| editor.text(cx));
        assert_eq!(
            text, "second queued",
            "Main editor should contain the last queued message"
        );

        // With a non-empty editor, another MoveUp must not consume the queue.
        editor.update_in(cx, |_editor, window, cx| {
            window.dispatch_action(Box::new(zed_actions::editor::MoveUp), cx);
        });
        cx.run_until_parked();

        let queue_len = active_thread(&conversation_view, cx)
            .read_with(cx, |thread, _cx| thread.message_queue.len());
        assert_eq!(queue_len, 1, "Queue should be untouched");
        let text = editor.update(cx, |editor, cx| editor.text(cx));
        assert_eq!(text, "second queued");
    }

    #[gpui::test]
    async fn test_paste_text_into_queued_message_promotes_to_main_editor(cx: &mut TestAppContext) {
        init_test(cx);

        let (conversation_view, cx) =
            paste_into_queued_message(cx, ClipboardItem::new_string("PASTED".to_string())).await;

        let queue_len = active_thread(&conversation_view, cx)
            .read_with(cx, |thread, _cx| thread.message_queue.len());
        assert_eq!(queue_len, 0);

        let text = message_editor(&conversation_view, cx).update(cx, |editor, cx| editor.text(cx));
        assert_eq!(text, "queued PASTEDmessage");
    }

    #[gpui::test]
    async fn test_paste_image_into_queued_message_promotes_to_main_editor(cx: &mut TestAppContext) {
        init_test(cx);

        use base64::Engine as _;
        use std::io::Write as _;
        let png_bytes = base64::prelude::BASE64_STANDARD
            .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==")
            .unwrap();
        let mut image_file = tempfile::Builder::new().suffix(".png").tempfile().unwrap();
        image_file.write_all(&png_bytes).unwrap();

        let (conversation_view, cx) = paste_into_queued_message(
            cx,
            ClipboardItem {
                entries: vec![gpui::ClipboardEntry::ExternalPaths(gpui::ExternalPaths(
                    vec![image_file.path().to_path_buf()].into(),
                ))],
            },
        )
        .await;

        let queue_len = active_thread(&conversation_view, cx)
            .read_with(cx, |thread, _cx| thread.message_queue.len());
        assert_eq!(queue_len, 0);

        let text = message_editor(&conversation_view, cx).update(cx, |editor, cx| editor.text(cx));
        let image_name = image_file.path().file_name().unwrap().to_string_lossy();
        let expected_uri = acp_thread::MentionUri::PastedImage {
            name: image_name.to_string(),
        }
        .to_uri()
        .to_string();
        assert_eq!(
            text,
            format!("queued [@{image_name}]({expected_uri}) message"),
        );
    }

    async fn paste_into_queued_message(
        cx: &mut TestAppContext,
        clipboard: ClipboardItem,
    ) -> (Entity<ConversationView>, &mut VisualTestContext) {
        let (conversation_view, cx) =
            setup_conversation_view(StubAgentServer::default_response(), cx).await;
        add_to_workspace(conversation_view.clone(), cx);

        active_thread(&conversation_view, cx).update_in(cx, |thread, window, cx| {
            thread
                .session_capabilities
                .write()
                .set_prompt_capabilities(acp::PromptCapabilities::new().image(true));
            thread.add_to_queue(
                vec![acp::ContentBlock::Text(acp::TextContent::new(
                    "queued message".to_string(),
                ))],
                vec![],
                window,
                cx,
            );
        });
        conversation_view.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();

        let queued_editor = active_thread(&conversation_view, cx).read_with(cx, |thread, _cx| {
            thread
                .message_queue
                .first()
                .map(|entry| entry.editor.clone())
                .expect("queued message editor not created")
        });

        cx.write_to_clipboard(clipboard);

        queued_editor.update_in(cx, |message_editor, window, cx| {
            message_editor.editor().update(cx, |editor, cx| {
                editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
                    selections.select_ranges([MultiBufferOffset(7)..MultiBufferOffset(7)]);
                });
            });
            message_editor.paste(&Paste, window, cx);
        });
        cx.run_until_parked();

        (conversation_view, cx)
    }

    #[gpui::test]
    async fn test_close_all_sessions_skips_when_unsupported(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        let thread_store = cx.update(|_window, cx| cx.new(|cx| ThreadStore::new(cx)));
        let connection_store =
            cx.update(|_window, cx| cx.new(|cx| AgentConnectionStore::new(project.clone(), cx)));

        // StubAgentConnection defaults to supports_close_session() -> false
        let conversation_view = cx.update(|window, cx| {
            cx.new(|cx| {
                ConversationView::new(
                    Rc::new(StubAgentServer::default_response()),
                    connection_store,
                    Agent::Custom { id: "Test".into() },
                    None,
                    None,
                    None,
                    None,
                    None,
                    workspace.downgrade(),
                    project,
                    Some(thread_store),
                    AgentThreadSource::AgentPanel,
                    window,
                    cx,
                )
            })
        });

        cx.run_until_parked();

        conversation_view.read_with(cx, |view, _cx| {
            let connected = view.as_connected().expect("Should be connected");
            assert!(
                !connected.threads.is_empty(),
                "There should be at least one thread"
            );
            assert!(
                !connected.connection.supports_close_session(),
                "StubAgentConnection should not support close"
            );
        });

        conversation_view
            .update(cx, |view, cx| {
                view.as_connected()
                    .expect("Should be connected")
                    .close_all_sessions(cx)
            })
            .await;
    }

    #[gpui::test]
    async fn test_close_all_sessions_calls_close_when_supported(cx: &mut TestAppContext) {
        init_test(cx);

        let (conversation_view, cx) =
            setup_conversation_view(StubAgentServer::new(CloseCapableConnection::new()), cx).await;

        cx.run_until_parked();

        let close_capable = conversation_view.read_with(cx, |view, _cx| {
            let connected = view.as_connected().expect("Should be connected");
            assert!(
                !connected.threads.is_empty(),
                "There should be at least one thread"
            );
            assert!(
                connected.connection.supports_close_session(),
                "CloseCapableConnection should support close"
            );
            connected
                .connection
                .clone()
                .into_any()
                .downcast::<CloseCapableConnection>()
                .expect("Should be CloseCapableConnection")
        });

        conversation_view
            .update(cx, |view, cx| {
                view.as_connected()
                    .expect("Should be connected")
                    .close_all_sessions(cx)
            })
            .await;

        let closed_count = close_capable.closed_sessions.lock().len();
        assert!(
            closed_count > 0,
            "close_session should have been called for each thread"
        );
    }

    #[gpui::test]
    async fn test_close_session_returns_error_when_unsupported(cx: &mut TestAppContext) {
        init_test(cx);

        let (conversation_view, cx) =
            setup_conversation_view(StubAgentServer::default_response(), cx).await;

        cx.run_until_parked();

        let result = conversation_view
            .update(cx, |view, cx| {
                let connected = view.as_connected().expect("Should be connected");
                assert!(
                    !connected.connection.supports_close_session(),
                    "StubAgentConnection should not support close"
                );
                let thread_view = connected
                    .threads
                    .values()
                    .next()
                    .expect("Should have at least one thread");
                let session_id = thread_view.read(cx).thread.read(cx).session_id().clone();
                connected.connection.clone().close_session(&session_id, cx)
            })
            .await;

        assert!(
            result.is_err(),
            "close_session should return an error when close is not supported"
        );
        assert!(
            result.unwrap_err().to_string().contains("not supported"),
            "Error message should indicate that closing is not supported"
        );
    }

    #[derive(Clone)]
    struct CloseCapableConnection {
        closed_sessions: Arc<Mutex<Vec<acp::SessionId>>>,
    }

    impl CloseCapableConnection {
        fn new() -> Self {
            Self {
                closed_sessions: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    impl AgentConnection for CloseCapableConnection {
        fn agent_id(&self) -> AgentId {
            AgentId::new("close-capable")
        }

        fn telemetry_id(&self) -> SharedString {
            "close-capable".into()
        }

        fn new_session(
            self: Rc<Self>,
            project: Entity<Project>,
            work_dirs: PathList,
            cx: &mut gpui::App,
        ) -> Task<gpui::Result<Entity<AcpThread>>> {
            let action_log = cx.new(|_| ActionLog::new(project.clone()));
            let thread = cx.new(|cx| {
                AcpThread::new(
                    None,
                    Some("CloseCapableConnection".into()),
                    Some(work_dirs),
                    self,
                    project,
                    action_log,
                    acp::SessionId::new("close-capable-session"),
                    watch::Receiver::constant(
                        acp::PromptCapabilities::new()
                            .image(true)
                            .audio(true)
                            .embedded_context(true),
                    ),
                    cx,
                )
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
            self.closed_sessions.lock().push(session_id.clone());
            Task::ready(Ok(()))
        }

        fn auth_methods(&self) -> &[acp::AuthMethod] {
            &[]
        }

        fn authenticate(
            &self,
            _method_id: acp::AuthMethodId,
            _cx: &mut App,
        ) -> Task<gpui::Result<()>> {
            Task::ready(Ok(()))
        }

        fn prompt(
            &self,
            _params: acp::PromptRequest,
            _cx: &mut App,
        ) -> Task<gpui::Result<acp::PromptResponse>> {
            Task::ready(Ok(acp::PromptResponse::new(acp::StopReason::EndTurn)))
        }

        fn cancel(&self, _session_id: &acp::SessionId, _cx: &mut App) {}

        fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
            self
        }
    }
}
