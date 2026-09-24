use crate::db::DbPermissionJudgement;
use crate::tool_permissions::{
    PERMISSION_JUDGE_ERROR_REASON, PERMISSION_JUDGE_NO_MODEL_REASON,
    PERMISSION_JUDGE_SYSTEM_PROMPT, PERMISSION_JUDGE_TIMEOUT, PERMISSION_JUDGE_TIMEOUT_REASON,
    PermissionJudgeCacheKey, PermissionJudgeContext, PermissionJudgeVerdict, ThreadPermissionJudge,
    build_permission_judge_prompt, parse_permission_judge_response,
};
use crate::{
    AnnotateCodeTool, ClearAnnotationsTool, CodeTourTool, HighlightCodeTool, HoverTool,
    SearchSymbolsTool, ZED_AGENT_ASSIST_LABEL,
};
use crate::{
    ApplyCodeActionTool, AskUserTool, CodeActionStore, ContextServerRegistry, CopyPathTool,
    CreateDirectoryTool, CreateThreadTool, DbLanguageModel, DbThread, DeletePathTool,
    DiagnosticsTool, EditFileTool, FetchTool, FindPathTool, FindReferencesTool, GetCodeActionsTool,
    GitDiffTool, GoToDefinitionTool, GrepTool, ListAgentsAndModelsTool, ListDirectoryTool,
    MovePathTool, ProjectSnapshot, ReadFileTool, RenameTool, RepoMapTool, RepoMapToolInput,
    SandboxedTerminalTool, SpawnAgentTool, SystemPromptTemplate, Template, Templates,
    TerminalExecution, TerminalTool, ToolPermissionDecision, WebSearchTool, WriteFileTool,
    decide_permission_for_paths_outside_project, decide_permission_from_settings,
    decide_terminal_permission,
};
use acp_thread::{ClientUserMessageId, MentionUri};
use action_log::ActionLog;
use agent_settings::UserAgentsMd;
use ide_query::AssistSource;

use crate::sandboxing::{
    SandboxRequest, ThreadSandbox, ThreadSandboxGrants, sandbox_git_dirs,
    sandbox_worktree_writable_paths, sandboxing_available_for_project,
    sandboxing_enabled_for_project,
};
use agent_client_protocol::schema::v1 as acp;
use agent_settings::{
    AgentProfileId, AgentProfileSettings, AgentSettings, AutoCompactThreshold, COMPACTION_PROMPT,
    SUMMARIZE_THREAD_DETAILED_PROMPT, SUMMARIZE_THREAD_PROMPT, builtin_profiles,
};
use anyhow::{Context as _, Result, anyhow};
use chrono::{DateTime, Local, Utc};
use client::UserStore;
use cloud_api_types::Plan;
use collections::{HashMap, HashSet, IndexMap};
use fs::Fs;
use futures::{
    FutureExt,
    channel::{mpsc, oneshot},
    future::Shared,
    stream::FuturesUnordered,
};
use futures::{StreamExt, stream};
use gpui::{
    App, AppContext, AsyncApp, Context, Entity, EventEmitter, ReadGlobal as _, SharedString, Task,
    WeakEntity,
};
use heck::ToSnakeCase as _;
use language_model::{
    CompletionIntent, LanguageModel, LanguageModelCompletionError, LanguageModelCompletionEvent,
    LanguageModelId, LanguageModelImage, LanguageModelProviderId, LanguageModelRegistry,
    LanguageModelRequest, LanguageModelRequestMessage, LanguageModelRequestTool,
    LanguageModelToolResult, LanguageModelToolResultContent, LanguageModelToolSchemaFormat,
    LanguageModelToolUse, LanguageModelToolUseId, MessageContent, Role, SelectedModel, Speed,
    StopReason, TokenUsage, ZED_CLOUD_PROVIDER_ID,
};
use project::{Project, WorktreeId, trusted_worktrees::TrustedWorktrees};
use prompt_store::ProjectContext;
use schemars::{JsonSchema, Schema};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use settings::{
    LanguageModelSelection, Settings, SettingsStore, ToolPermissionMode, update_settings_file,
};
use std::fmt::Write;
use std::{cell::RefCell, ops::ControlFlow};
use std::{
    collections::BTreeMap,
    marker::PhantomData,
    ops::RangeInclusive,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};
use util::{ResultExt, debug_panic, markdown::MarkdownCodeBlock, paths::PathStyle};
use uuid::Uuid;

const TOOL_CANCELED_MESSAGE: &str = "Tool canceled by user";
const TOOL_CALL_INTERRUPTED_BY_FOLLOW_UP_MESSAGE: &str =
    "Permission denied: user sent a follow-up message instead of approving the tool call.";
pub(crate) const FOLLOW_UP_PERMISSION_DENIED_OPTION_ID: &str = "follow_up_permission_denied";
pub const MAX_TOOL_NAME_LENGTH: usize = 64;
pub const MAX_SUBAGENT_DEPTH: u8 = 1;

pub(crate) fn provider_compatible_tool_name(tool_name: &str) -> String {
    let mut sanitized = String::new();
    for character in tool_name.chars() {
        if sanitized.len() >= MAX_TOOL_NAME_LENGTH {
            break;
        }

        if character.is_ascii_alphanumeric() || character == '_' || character == '-' {
            sanitized.push(character);
        } else {
            sanitized.push('_');
        }
    }

    if sanitized.is_empty() {
        sanitized.push_str("tool");
    }

    sanitized
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SandboxStatusKey {
    pub settings_sandbox: ThreadSandbox,
    pub thread_policy: sandbox::SandboxPolicy,
    pub baseline_writable_paths: Vec<PathBuf>,
    pub git_paths: Vec<PathBuf>,
}

/// The sandbox status the UI shows. The settings layer can turn the sandbox
/// off entirely, while this thread's grants only ever widen it (ADR-F46), so
/// the thread layer is always a policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedSandboxStatus {
    pub settings_sandbox: ThreadSandbox,
    pub thread_policy: sandbox::SandboxPolicy,
    pub baseline_writable_paths: Vec<PathBuf>,
}

pub enum SandboxStatusRefresh {
    Ready(VerifiedSandboxStatus),
    Pending(Task<VerifiedSandboxStatus>),
}

/// The smallest context window for which auto-compaction still runs. Below it
/// there isn't enough headroom for a compaction pass to be worthwhile, so the
/// thread is left uncompacted and the UI warns the user instead.
///
/// Upstream hardcoded 80,000; this fork reads
/// `agent.auto_compact_min_context_window` because local models routinely ship a
/// context window below that and would otherwise never compact at all
/// (TODO F13 ③).
pub fn min_compaction_context_window(cx: &App) -> u64 {
    AgentSettings::get_global(cx).auto_compact_min_context_window
}

// Using the heuristic that 1 token is about 4 bytes, keep the last 80K bytes of user-message content (~20k tokens).
const COMPACTION_RETAINED_USER_MESSAGES_BYTE_BUDGET: usize = 80_000;

/// Returned when a turn is attempted but no language model has been selected.
#[derive(Debug)]
pub struct NoModelConfiguredError;

impl std::fmt::Display for NoModelConfiguredError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "no language model configured")
    }
}

impl std::error::Error for NoModelConfiguredError {}

/// Context passed to a subagent thread for lifecycle management
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SubagentContext {
    /// ID of the parent thread
    pub parent_thread_id: acp::SessionId,

    /// Current depth level (0 = root agent, 1 = first-level subagent, etc.)
    pub depth: u8,
}

/// The ID of the user prompt that initiated a request.
///
/// This equates to the user physically submitting a message to the model (e.g., by pressing the Enter key).
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Clone, Serialize, Deserialize)]
pub struct PromptId(Arc<str>);

impl PromptId {
    pub fn new() -> Self {
        Self(Uuid::new_v4().to_string().into())
    }
}

impl std::fmt::Display for PromptId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

pub(crate) const MAX_RETRY_ATTEMPTS: u8 = 4;
pub(crate) const BASE_RETRY_DELAY: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
enum RetryStrategy {
    ExponentialBackoff {
        initial_delay: Duration,
        max_attempts: u8,
    },
    Fixed {
        delay: Duration,
        max_attempts: u8,
    },
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub enum Message {
    User(UserMessage),
    Agent(AgentMessage),
    Resume,
    Compaction(CompactionInfo),
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub enum CompactionInfo {
    Summary(SharedString),
    ProviderNative {
        provider: LanguageModelProviderId,
        items: Vec<serde_json::Value>,
    },
}

impl CompactionInfo {
    fn to_request(&self) -> Vec<LanguageModelRequestMessage> {
        match self {
            Self::Summary(summary) => vec![LanguageModelRequestMessage {
                role: Role::User,
                content: vec![format!(
                    "The previous conversation was compacted. Use this summary as context:\n\n{}",
                    summary
                )
                .into()],
                cache: false,
                reasoning_details: None,
            }],
            Self::ProviderNative { .. } => Vec::new(),
        }
    }
}

impl Message {
    pub fn as_agent_message(&self) -> Option<&AgentMessage> {
        match self {
            Message::Agent(agent_message) => Some(agent_message),
            _ => None,
        }
    }

    pub fn to_request(&self) -> Vec<LanguageModelRequestMessage> {
        match self {
            Message::User(message) => {
                if message.content.is_empty() {
                    vec![]
                } else {
                    vec![message.to_request()]
                }
            }
            Message::Agent(message) => message.to_request(),
            Message::Compaction(info) => info.to_request(),
            Message::Resume => vec![LanguageModelRequestMessage {
                role: Role::User,
                content: vec!["Continue where you left off".into()],
                cache: false,
                reasoning_details: None,
            }],
        }
    }

    pub fn to_markdown(&self) -> String {
        match self {
            Message::User(message) => message.to_markdown(),
            Message::Agent(message) => message.to_markdown(),
            Message::Resume => "[resume]\n".into(),
            Message::Compaction(_) => "--- Context Compacted ---\n".into(),
        }
    }

    pub fn role(&self) -> Role {
        match self {
            Message::User(_) | Message::Resume | Message::Compaction(_) => Role::User,
            Message::Agent(_) => Role::Assistant,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserMessage {
    pub id: ClientUserMessageId,
    pub content: Arc<[UserMessageContent]>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum UserMessageContent {
    Text(String),
    Mention {
        uri: MentionUri,
        content: SharedString,
    },
    Image(LanguageModelImage),
}

impl UserMessage {
    pub fn to_markdown(&self) -> String {
        let mut markdown = String::new();

        for content in &*self.content {
            match content {
                UserMessageContent::Text(text) => {
                    markdown.push_str(text);
                    markdown.push('\n');
                }
                UserMessageContent::Image(_) => {
                    markdown.push_str("<image />\n");
                }
                UserMessageContent::Mention { uri, content } => {
                    if !content.is_empty() {
                        let _ = writeln!(&mut markdown, "{}\n\n{}", uri.as_link(), content);
                    } else {
                        let _ = writeln!(&mut markdown, "{}", uri.as_link());
                    }
                }
            }
        }

        markdown
    }

    fn to_request(&self) -> LanguageModelRequestMessage {
        let mut message = LanguageModelRequestMessage {
            role: Role::User,
            content: Vec::with_capacity(self.content.len()),
            cache: false,
            reasoning_details: None,
        };

        const OPEN_CONTEXT: &str = "<context>\n\
            The following items were attached by the user. \
            They are up-to-date and don't need to be re-read.\n\n";

        const OPEN_FILES_TAG: &str = "<files>";
        const OPEN_DIRECTORIES_TAG: &str = "<directories>";
        const OPEN_SYMBOLS_TAG: &str = "<symbols>";
        const OPEN_SELECTIONS_TAG: &str = "<selections>";
        const OPEN_THREADS_TAG: &str = "<threads>";
        const OPEN_FETCH_TAG: &str = "<fetched_urls>";
        const OPEN_RULES_TAG: &str =
            "<rules>\nThe user has specified the following rules that should be applied:\n";
        const OPEN_DIAGNOSTICS_TAG: &str = "<diagnostics>";
        const OPEN_DIFFS_TAG: &str = "<diffs>";
        const MERGE_CONFLICT_TAG: &str = "<merge_conflicts>";
        const OPEN_SKILLS_TAG: &str =
            "<skills>\nThe user has attached the following agent skills:\n";

        let mut file_context = OPEN_FILES_TAG.to_string();
        let mut directory_context = OPEN_DIRECTORIES_TAG.to_string();
        let mut symbol_context = OPEN_SYMBOLS_TAG.to_string();
        let mut selection_context = OPEN_SELECTIONS_TAG.to_string();
        let mut thread_context = OPEN_THREADS_TAG.to_string();
        let mut fetch_context = OPEN_FETCH_TAG.to_string();
        let mut rules_context = OPEN_RULES_TAG.to_string();
        let mut diagnostics_context = OPEN_DIAGNOSTICS_TAG.to_string();
        let mut diffs_context = OPEN_DIFFS_TAG.to_string();
        let mut merge_conflict_context = MERGE_CONFLICT_TAG.to_string();
        let mut skills_context = OPEN_SKILLS_TAG.to_string();

        for chunk in &*self.content {
            let chunk = match chunk {
                UserMessageContent::Text(text) => {
                    language_model::MessageContent::Text(text.clone())
                }
                UserMessageContent::Image(value) => {
                    language_model::MessageContent::Image(value.clone())
                }
                UserMessageContent::Mention { uri, content } => {
                    match uri {
                        MentionUri::File { abs_path } => {
                            write!(
                                &mut file_context,
                                "\n{}",
                                MarkdownCodeBlock {
                                    tag: &codeblock_tag(abs_path, None),
                                    text: content,
                                }
                            )
                            .ok();
                        }
                        MentionUri::PastedImage { .. } => {
                            debug_panic!("pasted image URI should not be used in mention content")
                        }
                        MentionUri::Directory { .. } => {
                            write!(&mut directory_context, "\n{}\n", content).ok();
                        }
                        MentionUri::Symbol {
                            abs_path: path,
                            line_range,
                            ..
                        } => {
                            write!(
                                &mut symbol_context,
                                "\n{}",
                                MarkdownCodeBlock {
                                    tag: &codeblock_tag(path, Some(line_range)),
                                    text: content
                                }
                            )
                            .ok();
                        }
                        MentionUri::Selection {
                            abs_path: path,
                            line_range,
                            ..
                        } => {
                            write!(
                                &mut selection_context,
                                "\n{}",
                                MarkdownCodeBlock {
                                    tag: &codeblock_tag(
                                        path.as_deref().unwrap_or("Untitled".as_ref()),
                                        Some(line_range)
                                    ),
                                    text: content
                                }
                            )
                            .ok();
                        }
                        MentionUri::Thread { .. } => {
                            write!(&mut thread_context, "\n{}\n", content).ok();
                        }
                        MentionUri::Rule { .. } => {
                            // Deprecated: keeps legacy rule mentions as context.
                            write!(
                                &mut rules_context,
                                "\n{}",
                                MarkdownCodeBlock {
                                    tag: "",
                                    text: content
                                }
                            )
                            .ok();
                        }
                        MentionUri::Fetch { url } => {
                            write!(&mut fetch_context, "\nFetch: {}\n\n{}", url, content).ok();
                        }
                        MentionUri::Diagnostics { .. } => {
                            write!(&mut diagnostics_context, "\n{}\n", content).ok();
                        }
                        MentionUri::TerminalSelection { .. } => {
                            write!(
                                &mut selection_context,
                                "\n{}",
                                MarkdownCodeBlock {
                                    tag: "console",
                                    text: content
                                }
                            )
                            .ok();
                        }
                        MentionUri::GitDiff { base_ref } => {
                            write!(
                                &mut diffs_context,
                                "\nBranch diff against {}:\n{}",
                                base_ref,
                                MarkdownCodeBlock {
                                    tag: "diff",
                                    text: content
                                }
                            )
                            .ok();
                        }
                        MentionUri::MergeConflict { file_path } => {
                            write!(
                                &mut merge_conflict_context,
                                "\nMerge conflict in {}:\n{}",
                                file_path,
                                MarkdownCodeBlock {
                                    tag: "diff",
                                    text: content
                                }
                            )
                            .ok();
                        }
                        MentionUri::Skill { name, source, .. } => {
                            let label = format!("{} ({})", name, source);
                            write!(&mut skills_context, "\nSkill: {}\n{}\n", label, content).ok();
                        }
                    }

                    language_model::MessageContent::Text(uri.as_link().to_string())
                }
            };

            message.content.push(chunk);
        }

        let len_before_context = message.content.len();

        if file_context.len() > OPEN_FILES_TAG.len() {
            file_context.push_str("</files>\n");
            message
                .content
                .push(language_model::MessageContent::Text(file_context));
        }

        if directory_context.len() > OPEN_DIRECTORIES_TAG.len() {
            directory_context.push_str("</directories>\n");
            message
                .content
                .push(language_model::MessageContent::Text(directory_context));
        }

        if symbol_context.len() > OPEN_SYMBOLS_TAG.len() {
            symbol_context.push_str("</symbols>\n");
            message
                .content
                .push(language_model::MessageContent::Text(symbol_context));
        }

        if selection_context.len() > OPEN_SELECTIONS_TAG.len() {
            selection_context.push_str("</selections>\n");
            message
                .content
                .push(language_model::MessageContent::Text(selection_context));
        }

        if diffs_context.len() > OPEN_DIFFS_TAG.len() {
            diffs_context.push_str("</diffs>\n");
            message
                .content
                .push(language_model::MessageContent::Text(diffs_context));
        }

        if thread_context.len() > OPEN_THREADS_TAG.len() {
            thread_context.push_str("</threads>\n");
            message
                .content
                .push(language_model::MessageContent::Text(thread_context));
        }

        if fetch_context.len() > OPEN_FETCH_TAG.len() {
            fetch_context.push_str("</fetched_urls>\n");
            message
                .content
                .push(language_model::MessageContent::Text(fetch_context));
        }

        if rules_context.len() > OPEN_RULES_TAG.len() {
            rules_context.push_str("</user_rules>\n");
            message
                .content
                .push(language_model::MessageContent::Text(rules_context));
        }

        if diagnostics_context.len() > OPEN_DIAGNOSTICS_TAG.len() {
            diagnostics_context.push_str("</diagnostics>\n");
            message
                .content
                .push(language_model::MessageContent::Text(diagnostics_context));
        }

        if skills_context.len() > OPEN_SKILLS_TAG.len() {
            skills_context.push_str("</skills>\n");
            message
                .content
                .push(language_model::MessageContent::Text(skills_context));
        }

        if merge_conflict_context.len() > MERGE_CONFLICT_TAG.len() {
            merge_conflict_context.push_str("</merge_conflicts>\n");
            message
                .content
                .push(language_model::MessageContent::Text(merge_conflict_context));
        }

        if message.content.len() > len_before_context {
            message.content.insert(
                len_before_context,
                language_model::MessageContent::Text(OPEN_CONTEXT.into()),
            );
            message
                .content
                .push(language_model::MessageContent::Text("</context>".into()));
        }

        message
    }
}

fn codeblock_tag(full_path: &Path, line_range: Option<&RangeInclusive<u32>>) -> String {
    let mut result = String::new();

    if let Some(extension) = full_path.extension().and_then(|ext| ext.to_str()) {
        let _ = write!(result, "{} ", extension);
    }

    let _ = write!(result, "{}", full_path.display());

    if let Some(range) = line_range {
        if range.start() == range.end() {
            let _ = write!(result, ":{}", range.start() + 1);
        } else {
            let _ = write!(result, ":{}-{}", range.start() + 1, range.end() + 1);
        }
    }

    result
}

impl AgentMessage {
    pub fn to_markdown(&self) -> String {
        let mut markdown = String::new();

        for content in &self.content {
            match content {
                AgentMessageContent::Text(text) => {
                    markdown.push_str(text);
                    markdown.push('\n');
                }
                AgentMessageContent::Thinking { text, .. } => {
                    markdown.push_str("<think>");
                    markdown.push_str(text);
                    markdown.push_str("</think>\n");
                }
                AgentMessageContent::RedactedThinking(_) => {
                    markdown.push_str("<redacted_thinking />\n")
                }
                AgentMessageContent::ToolUse(tool_use) => {
                    markdown.push_str(&format!(
                        "**Tool Use**: {} (ID: {})\n",
                        tool_use.name, tool_use.id
                    ));
                    markdown.push_str(&format!(
                        "{}\n",
                        MarkdownCodeBlock {
                            tag: "json",
                            text: &format!("{:#}", tool_use.input.to_display_json())
                        }
                    ));
                }
            }
        }

        for tool_result in self.tool_results.values() {
            markdown.push_str(&format!(
                "**Tool Result**: {} (ID: {})\n\n",
                tool_result.tool_name, tool_result.tool_use_id
            ));
            if tool_result.is_error {
                markdown.push_str("**ERROR:**\n");
            }

            for part in &tool_result.content {
                match part {
                    LanguageModelToolResultContent::Text(text) => {
                        writeln!(markdown, "{text}\n").ok();
                    }
                    LanguageModelToolResultContent::Image(_) => {
                        writeln!(markdown, "<image />\n").ok();
                    }
                }
            }

            if let Some(output) = tool_result.output.as_ref() {
                writeln!(
                    markdown,
                    "**Debug Output**:\n\n```json\n{}\n```\n",
                    serde_json::to_string_pretty(output).unwrap()
                )
                .unwrap();
            }
        }

        markdown
    }

    pub fn to_request(&self) -> Vec<LanguageModelRequestMessage> {
        let mut assistant_message = LanguageModelRequestMessage {
            role: Role::Assistant,
            content: Vec::with_capacity(self.content.len()),
            cache: false,
            reasoning_details: self.reasoning_details.clone(),
        };
        for chunk in &self.content {
            match chunk {
                AgentMessageContent::Text(text) => {
                    assistant_message
                        .content
                        .push(language_model::MessageContent::Text(text.clone()));
                }
                AgentMessageContent::Thinking { text, signature } => {
                    assistant_message
                        .content
                        .push(language_model::MessageContent::Thinking {
                            text: text.clone(),
                            signature: signature.clone(),
                        });
                }
                AgentMessageContent::RedactedThinking(value) => {
                    assistant_message.content.push(
                        language_model::MessageContent::RedactedThinking(value.clone()),
                    );
                }
                AgentMessageContent::ToolUse(tool_use) => {
                    if self.tool_results.contains_key(&tool_use.id) {
                        assistant_message
                            .content
                            .push(language_model::MessageContent::ToolUse(tool_use.clone()));
                    }
                }
            };
        }

        let mut user_message = LanguageModelRequestMessage {
            role: Role::User,
            content: Vec::new(),
            cache: false,
            reasoning_details: None,
        };

        for tool_result in self.tool_results.values() {
            let mut tool_result = tool_result.clone();
            // Surprisingly, the API fails if we return an empty string here.
            // It thinks we are sending a tool use without a tool result.
            if tool_result.is_content_empty() {
                tool_result.content = vec!["<Tool returned an empty string>".into()];
            }
            user_message
                .content
                .push(language_model::MessageContent::ToolResult(tool_result));
        }

        let mut messages = Vec::new();
        if !assistant_message.content.is_empty() {
            messages.push(assistant_message);
        }
        if !user_message.content.is_empty() {
            messages.push(user_message);
        }
        messages
    }
}

#[derive(Default, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentMessage {
    pub(crate) content: Vec<AgentMessageContent>,
    pub(crate) tool_results: IndexMap<LanguageModelToolUseId, LanguageModelToolResult>,
    pub(crate) reasoning_details: Option<Arc<serde_json::Value>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentMessageContent {
    Text(String),
    Thinking {
        text: String,
        signature: Option<String>,
    },
    RedactedThinking(String),
    ToolUse(LanguageModelToolUse),
}

pub trait TerminalHandle {
    fn id(&self, cx: &AsyncApp) -> Result<acp::TerminalId>;
    fn current_output(&self, cx: &AsyncApp) -> Result<acp::TerminalOutputResponse>;
    fn wait_for_exit(&self, cx: &AsyncApp) -> Result<Shared<Task<acp::TerminalExitStatus>>>;
    fn kill(&self, cx: &AsyncApp) -> Result<()>;
    fn was_stopped_by_user(&self, cx: &AsyncApp) -> Result<bool>;
}

pub trait SubagentHandle {
    /// The session ID of this subagent thread
    fn id(&self) -> acp::SessionId;
    /// The current number of entries in the thread.
    /// Useful for knowing where the next turn will begin
    fn num_entries(&self, cx: &App) -> usize;
    /// Runs a turn for a given message and returns both the response and the index of that output message.
    fn send(&self, message: String, cx: &AsyncApp) -> Task<Result<String>>;
}

pub trait ThreadEnvironment {
    fn create_terminal(
        &self,
        command: String,
        extra_env: Vec<acp::EnvVariable>,
        cwd: Option<PathBuf>,
        output_byte_limit: Option<u64>,
        sandbox_wrap: Option<acp_thread::SandboxWrap>,
        cx: &mut AsyncApp,
    ) -> Task<Result<Rc<dyn TerminalHandle>>>;

    fn create_subagent(&self, label: String, cx: &mut App) -> Result<Rc<dyn SubagentHandle>>;

    fn resume_subagent(
        &self,
        _session_id: acp::SessionId,
        _cx: &mut App,
    ) -> Result<Rc<dyn SubagentHandle>> {
        Err(anyhow::anyhow!(
            "Resuming subagent sessions is not supported"
        ))
    }

    /// Creates an independent sibling thread visible in the agent sidebar.
    /// Unlike subagents, sibling threads are first-class threads that persist
    /// and run in parallel without reporting results back to the parent.
    fn create_sibling_thread(
        &self,
        request: SiblingThreadRequest,
        cx: &mut AsyncApp,
    ) -> Task<Result<SiblingThreadInfo>> {
        let _ = request;
        let _ = cx;
        Task::ready(Err(anyhow::anyhow!(
            "Creating sibling threads is not supported in this environment"
        )))
    }

    /// Lists the agents and models available for use with `create_sibling_thread`.
    fn list_available_agents(&self, cx: &mut App) -> Result<AvailableAgents> {
        let _ = cx;
        Err(anyhow::anyhow!(
            "Listing available agents is not supported in this environment"
        ))
    }
}

/// A request to create a new sibling thread.
#[derive(Debug, Clone)]
pub struct SiblingThreadRequest {
    /// A short title for the new thread, shown in the sidebar.
    pub title: SharedString,
    /// The initial prompt to send to the new thread.
    pub prompt: String,
    /// Optional agent ID to use. Defaults to the native Zed agent.
    pub agent_id: Option<String>,
    /// Optional model override, as `provider/model-id`.
    /// Defaults to the user's configured default model for the agent.
    pub model: Option<String>,
    /// Whether to create the thread in a new git worktree workspace.
    pub use_new_worktree: bool,
    /// Optional worktree directory name. When `None`, the UI generates a
    /// random non-colliding name (matching the manual "Create worktree"
    /// flow). Only relevant when `use_new_worktree` is true.
    pub worktree_name: Option<String>,
    /// Git ref (branch, tag, or commit) to base the new worktree on.
    /// Only relevant when `use_new_worktree` is true.
    pub base_ref: Option<String>,
}

/// Information returned when a sibling thread is successfully created.
#[derive(Debug, Clone)]
pub struct SiblingThreadInfo {
    /// The title assigned to the thread.
    pub title: SharedString,
    /// The agent ID used for the thread.
    pub agent_id: String,
    /// The model ID used for the thread, if known.
    pub model: Option<String>,
    /// An optional, non-fatal heads-up about the created thread that the
    /// caller should relay or take into account (e.g., the project had an
    /// unusual worktree layout that affected how the new worktree was set
    /// up). Empty when nothing noteworthy happened.
    pub warning: Option<String>,
}

/// A list of agents and, for each, the models available for use.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AvailableAgents {
    pub agents: Vec<AvailableAgent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AvailableAgent {
    /// Identifier used when creating a thread.
    pub id: String,
    /// Human-readable name shown in the UI.
    pub name: SharedString,
    /// Whether this is Zed's built-in native agent.
    pub is_native: bool,
    /// Models available for this agent. May be empty if models are not
    /// enumerated up front (e.g., external agents that choose their own).
    pub models: Vec<AvailableModel>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AvailableModel {
    /// Identifier to pass as the `model` field when creating a thread.
    pub id: String,
    /// Human-readable name.
    pub name: SharedString,
    /// Whether this is the default model for the agent.
    pub is_default: bool,
}

#[derive(Debug)]
pub enum ThreadEvent {
    UserMessage(UserMessage),
    AgentText(String),
    AgentThinking(String),
    ToolCall(acp::ToolCall),
    ToolCallUpdate(acp_thread::ToolCallUpdate),
    ToolCallAuthorization(ToolCallAuthorization),
    ToolCallAuthorizationResolved {
        tool_call_id: acp::ToolCallId,
        outcome: acp_thread::SelectedPermissionOutcome,
    },
    Elicitation(ElicitationRequest),
    SubagentSpawned(acp::SessionId),
    Retry(acp_thread::RetryStatus),
    ContextCompaction(acp_thread::ContextCompaction),
    ContextCompactionUpdate(acp_thread::ContextCompactionUpdate),
    Stop(acp::StopReason),
}

#[derive(Debug)]
pub struct NewTerminal {
    pub command: String,
    pub output_byte_limit: Option<u64>,
    pub cwd: Option<PathBuf>,
    pub response: oneshot::Sender<Result<Entity<acp_thread::Terminal>>>,
}

#[derive(Debug, Clone)]
pub struct ToolPermissionContext {
    pub tool_name: String,
    pub input_values: Vec<String>,
    pub scope: ToolPermissionScope,
    /// Where the command will run, for the terminal tool. `None` for every
    /// other tool, and for a terminal command whose execution isn't
    /// established at this point; both are then judged as if the command could
    /// run with Zed's own reach (決定 2).
    pub terminal_execution: Option<TerminalExecution>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolPermissionScope {
    ToolInput,
    SymlinkTarget,
    AgentSkills,
    /// A write to the project's `.zed` folder or to the user's global
    /// settings, which is always confirmed (ADR-F52 決定 6).
    SensitiveSettings,
}

/// Japanese display name for a built-in Agent tool's internal name, used in the
/// "always allow/deny for <tool>" permission-scope dropdown. Only the tools that
/// commonly reach this dropdown (write/execute/network tools) are mapped; any
/// other tool name (including MCP tool names, which are defined by external
/// servers and can't be mapped statically) falls back to the internal name with
/// underscores replaced by spaces.
fn localized_tool_name_for_permission(tool_name: &str) -> String {
    let localized = if tool_name == TerminalTool::NAME || tool_name == SandboxedTerminalTool::NAME {
        "ターミナル"
    } else if tool_name == CopyPathTool::NAME {
        "コピー"
    } else if tool_name == MovePathTool::NAME {
        "移動"
    } else if tool_name == EditFileTool::NAME {
        "ファイル編集"
    } else if tool_name == WriteFileTool::NAME {
        "ファイル書き込み"
    } else if tool_name == DeletePathTool::NAME {
        "削除"
    } else if tool_name == CreateDirectoryTool::NAME {
        "ディレクトリ作成"
    } else if tool_name == FetchTool::NAME {
        "URL の取得"
    } else if tool_name == ApplyCodeActionTool::NAME {
        "コードアクションの適用"
    } else if tool_name == RenameTool::NAME {
        "シンボルの名前変更"
    } else {
        return tool_name.replace('_', " ");
    };
    localized.to_string()
}

impl ToolPermissionContext {
    pub fn new(tool_name: impl Into<String>, input_values: Vec<String>) -> Self {
        Self {
            tool_name: tool_name.into(),
            input_values,
            scope: ToolPermissionScope::ToolInput,
            terminal_execution: None,
        }
    }

    pub fn symlink_target(tool_name: impl Into<String>, target_paths: Vec<String>) -> Self {
        Self {
            tool_name: tool_name.into(),
            input_values: target_paths,
            scope: ToolPermissionScope::SymlinkTarget,
            terminal_execution: None,
        }
    }

    pub fn for_agent_skills(mut self) -> Self {
        self.scope = ToolPermissionScope::AgentSkills;
        self
    }

    /// Marks a card about the project's `.zed` folder or the user's global
    /// settings, so that it doesn't offer standing approvals it can't honour
    /// (see [`Self::build_permission_options`]).
    pub fn for_sensitive_settings(mut self) -> Self {
        self.scope = ToolPermissionScope::SensitiveSettings;
        self
    }

    /// Marks a terminal command as one that will run inside the sandbox, so
    /// the permission rules can take the sandbox's own boundary into account
    /// (決定 2).
    pub fn in_sandbox(mut self) -> Self {
        self.terminal_execution = Some(TerminalExecution::Sandboxed);
        self
    }

    /// Builds the permission options for this tool context.
    ///
    /// This is the canonical source for permission option generation.
    /// Tests should use this function rather than manually constructing options.
    ///
    /// # Shell Compatibility for Terminal Tool
    ///
    /// For the terminal tool, "Always allow" options are only shown when the user's
    /// shell supports POSIX-like command chaining syntax (`&&`, `||`, `;`, `|`).
    ///
    /// **Why this matters:** When a user sets up an "always allow" pattern like `^cargo`,
    /// we need to parse the command to extract all sub-commands and verify that EVERY
    /// sub-command matches the pattern. Otherwise, an attacker could craft a command like
    /// `cargo build && rm -rf /` that would bypass the security check.
    ///
    /// **Supported shells:** Posix (sh, bash, dash, zsh), Fish 3.0+, PowerShell 7+/Pwsh,
    /// Cmd, Xonsh, Csh, Tcsh
    ///
    /// **Unsupported shells:** Nushell (uses `and`/`or` keywords), Elvish (uses `and`/`or`
    /// keywords), Rc (Plan 9 shell - no `&&`/`||` operators)
    ///
    /// For unsupported shells, we hide the "Always allow" UI options entirely, and if
    /// the user has `always_allow` rules configured in settings, `ToolPermissionDecision::from_input`
    /// will return a `Deny` with an explanatory error message.
    pub fn build_permission_options(&self) -> acp_thread::PermissionOptions {
        use crate::pattern_extraction::*;
        use util::shell::ShellKind;

        let tool_name = &self.tool_name;
        let input_values = &self.input_values;
        if self.scope == ToolPermissionScope::SymlinkTarget {
            return acp_thread::PermissionOptions::Flat(vec![
                acp::PermissionOption::new(
                    acp::PermissionOptionId::new("allow"),
                    "許可",
                    acp::PermissionOptionKind::AllowOnce,
                ),
                acp::PermissionOption::new(
                    acp::PermissionOptionId::new("deny"),
                    "拒否",
                    acp::PermissionOptionKind::RejectOnce,
                ),
            ]);
        }

        // Skills, the project's `.zed` folder and the global settings always
        // prompt (ADR-F52 決定 6): `authorize_always_prompt` never reads the
        // tool permissions for them. An "always" choice here would write a
        // rule that this card and the next one both ignore — approval that
        // silently does nothing — so only the once-only pair is offered.
        if matches!(
            self.scope,
            ToolPermissionScope::AgentSkills | ToolPermissionScope::SensitiveSettings
        ) {
            return acp_thread::PermissionOptions::Flat(vec![
                acp::PermissionOption::new(
                    acp::PermissionOptionId::new("allow"),
                    "許可",
                    acp::PermissionOptionKind::AllowOnce,
                ),
                acp::PermissionOption::new(
                    acp::PermissionOptionId::new("deny"),
                    "拒否",
                    acp::PermissionOptionKind::RejectOnce,
                ),
            ]);
        }

        // Check if the user's shell supports POSIX-like command chaining.
        // See the doc comment above for the full explanation of why this is needed.
        let shell_supports_always_allow = if tool_name == TerminalTool::NAME {
            ShellKind::system().supports_posix_chaining()
        } else {
            true
        };

        // For terminal commands with multiple pipeline commands, use DropdownWithPatterns
        // to let users individually select which command patterns to always allow.
        if tool_name == TerminalTool::NAME && shell_supports_always_allow {
            if let Some(input) = input_values.first() {
                let all_patterns = extract_all_terminal_patterns(input);
                if all_patterns.len() > 1 {
                    let mut choices = Vec::new();
                    choices.push(acp_thread::PermissionOptionChoice {
                        allow: acp::PermissionOption::new(
                            acp::PermissionOptionId::new(format!("always_allow:{}", tool_name)),
                            format!("{}は常に", localized_tool_name_for_permission(tool_name)),
                            acp::PermissionOptionKind::AllowAlways,
                        ),
                        deny: acp::PermissionOption::new(
                            acp::PermissionOptionId::new(format!("always_deny:{}", tool_name)),
                            format!("{}は常に", localized_tool_name_for_permission(tool_name)),
                            acp::PermissionOptionKind::RejectAlways,
                        ),
                        sub_patterns: vec![],
                    });
                    choices.push(acp_thread::PermissionOptionChoice {
                        allow: acp::PermissionOption::new(
                            acp::PermissionOptionId::new("allow"),
                            "今回のみ",
                            acp::PermissionOptionKind::AllowOnce,
                        ),
                        deny: acp::PermissionOption::new(
                            acp::PermissionOptionId::new("deny"),
                            "今回のみ",
                            acp::PermissionOptionKind::RejectOnce,
                        ),
                        sub_patterns: vec![],
                    });
                    return acp_thread::PermissionOptions::DropdownWithPatterns {
                        choices,
                        patterns: all_patterns,
                        tool_name: tool_name.clone(),
                    };
                }
            }
        }

        let extract_for_value = |value: &str| -> (Option<String>, Option<String>) {
            if tool_name == TerminalTool::NAME {
                (
                    extract_terminal_pattern(value),
                    extract_terminal_pattern_display(value),
                )
            } else if tool_name == CopyPathTool::NAME
                || tool_name == MovePathTool::NAME
                || tool_name == EditFileTool::NAME
                || tool_name == WriteFileTool::NAME
                || tool_name == DeletePathTool::NAME
                || tool_name == CreateDirectoryTool::NAME
            {
                (
                    extract_path_pattern(value),
                    extract_path_pattern_display(value),
                )
            } else if tool_name == FetchTool::NAME {
                (
                    extract_url_pattern(value),
                    extract_url_pattern_display(value),
                )
            } else {
                (None, None)
            }
        };

        // Extract patterns from all input values. Only offer a pattern-specific
        // "always allow/deny" button when every value produces the same pattern.
        let (pattern, pattern_display) = match input_values.as_slice() {
            [single] => extract_for_value(single),
            _ => {
                let mut iter = input_values.iter().map(|v| extract_for_value(v));
                match iter.next() {
                    Some(first) => {
                        if iter.all(|pair| pair.0 == first.0) {
                            first
                        } else {
                            (None, None)
                        }
                    }
                    None => (None, None),
                }
            }
        };

        let mut choices = Vec::new();

        let mut push_choice =
            |label: String, allow_id, deny_id, allow_kind, deny_kind, sub_patterns: Vec<String>| {
                choices.push(acp_thread::PermissionOptionChoice {
                    allow: acp::PermissionOption::new(
                        acp::PermissionOptionId::new(allow_id),
                        label.clone(),
                        allow_kind,
                    ),
                    deny: acp::PermissionOption::new(
                        acp::PermissionOptionId::new(deny_id),
                        label,
                        deny_kind,
                    ),
                    sub_patterns,
                });
            };

        if shell_supports_always_allow {
            push_choice(
                format!("{}は常に", localized_tool_name_for_permission(tool_name)),
                format!("always_allow:{}", tool_name),
                format!("always_deny:{}", tool_name),
                acp::PermissionOptionKind::AllowAlways,
                acp::PermissionOptionKind::RejectAlways,
                vec![],
            );

            if let (Some(pattern), Some(display)) = (pattern, pattern_display) {
                let button_text = if tool_name == TerminalTool::NAME {
                    format!("`{}` コマンドは常に", display)
                } else {
                    format!("`{}` は常に", display)
                };
                push_choice(
                    button_text,
                    format!("always_allow:{}", tool_name),
                    format!("always_deny:{}", tool_name),
                    acp::PermissionOptionKind::AllowAlways,
                    acp::PermissionOptionKind::RejectAlways,
                    vec![pattern],
                );
            }
        }

        push_choice(
            "今回のみ".to_string(),
            "allow".to_string(),
            "deny".to_string(),
            acp::PermissionOptionKind::AllowOnce,
            acp::PermissionOptionKind::RejectOnce,
            vec![],
        );

        acp_thread::PermissionOptions::Dropdown(choices)
    }
}

#[derive(Debug)]
pub struct ToolCallAuthorization {
    pub tool_call: acp::ToolCallUpdate,
    pub options: acp_thread::PermissionOptions,
    pub response: oneshot::Sender<acp_thread::SelectedPermissionOutcome>,
    pub context: Option<ToolPermissionContext>,
    pub kind: acp_thread::AuthorizationKind,
}

fn ensure_tool_call_authorization_not_interrupted(
    outcome: &acp_thread::SelectedPermissionOutcome,
) -> Result<()> {
    if outcome.option_id.0.as_ref() == FOLLOW_UP_PERMISSION_DENIED_OPTION_ID {
        Err(anyhow!(TOOL_CALL_INTERRUPTED_BY_FOLLOW_UP_MESSAGE))
    } else {
        Ok(())
    }
}

/// A request from a tool to elicit structured input from the user via a form.
///
/// The scope (session id) is filled in by the connection layer when the request
/// is handed to the [`AcpThread`], so tools only supply the form schema and the
/// message to display.
#[derive(Debug)]
pub struct ElicitationRequest {
    pub tool_call_id: acp::ToolCallId,
    pub message: String,
    pub schema: acp::ElicitationSchema,
    pub response: oneshot::Sender<acp::CreateElicitationResponse>,
}

fn auto_resolve_permission_outcome(
    options: &acp_thread::PermissionOptions,
    is_allow: bool,
) -> Result<acp_thread::SelectedPermissionOutcome> {
    let kind = if is_allow {
        acp::PermissionOptionKind::AllowOnce
    } else {
        acp::PermissionOptionKind::RejectOnce
    };
    let option = options
        .first_option_of_kind(kind)
        .ok_or_else(|| anyhow!("permission prompt has no auto-resolution option"))?;

    Ok(acp_thread::SelectedPermissionOutcome::new(
        option.option_id.clone(),
        option.kind,
    ))
}

#[derive(Debug, thiserror::Error)]
enum CompletionError {
    #[error("max tokens")]
    MaxTokens,
    #[error("refusal")]
    Refusal,
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub enum ThreadModel {
    Ready(Arc<dyn LanguageModel>),
    Unresolved(SelectedModel),
    Unset,
}

impl ThreadModel {
    fn as_model(&self) -> Option<&Arc<dyn LanguageModel>> {
        match self {
            Self::Ready(model) => Some(model),
            Self::Unresolved(_) | Self::Unset => None,
        }
    }
}

impl From<&ThreadModel> for Option<DbLanguageModel> {
    fn from(model: &ThreadModel) -> Self {
        match model {
            ThreadModel::Ready(model) => Some(DbLanguageModel {
                provider: model.provider_id().to_string(),
                model: model.id().0.to_string(),
            }),
            ThreadModel::Unresolved(selection) => Some(DbLanguageModel {
                provider: selection.provider.0.to_string(),
                model: selection.model.0.to_string(),
            }),
            ThreadModel::Unset => None,
        }
    }
}

pub struct Thread {
    id: acp::SessionId,
    prompt_id: PromptId,
    updated_at: DateTime<Utc>,
    title: Option<SharedString>,
    pending_title_generation: Option<Task<()>>,
    title_generation_error: Option<SharedString>,
    pending_summary_generation: Option<Shared<Task<Option<SharedString>>>>,
    summary: Option<SharedString>,
    messages: Vec<Arc<Message>>,
    user_store: Entity<UserStore>,
    /// Holds the task that handles agent interaction until the end of the turn.
    /// Survives across multiple requests as the model performs tool calls and
    /// we run tools, report their results.
    running_turn: Option<RunningTurn>,
    /// When set, the current turn ends at the next message boundary instead of
    /// running to completion. The UI sets this to deliver a "steering" queued
    /// message mid-task; by default queued messages wait for the turn to finish.
    end_turn_at_next_boundary: bool,
    pending_message: Option<AgentMessage>,
    pub(crate) tools: BTreeMap<SharedString, Arc<dyn AnyAgentTool>>,
    request_token_usage: HashMap<ClientUserMessageId, language_model::TokenUsage>,
    cumulative_token_usage: TokenUsage,
    /// The per-field maximum usage snapshot already added to
    /// `cumulative_token_usage` for the in-flight completion request. Reset at
    /// the start of each request.
    current_request_token_usage: TokenUsage,
    pending_compaction_telemetry: Option<CompactionTelemetry>,
    #[allow(unused)]
    initial_project_snapshot: Shared<Task<Option<Arc<ProjectSnapshot>>>>,
    pub(crate) context_server_registry: Entity<ContextServerRegistry>,
    /// The profile the user chose. In a restricted workspace this is *not* what
    /// the thread runs with — see [`Thread::profile`].
    profile_id: AgentProfileId,
    /// Whether `profile_id` is exempt from the restricted-workspace downgrade,
    /// because the user picked it explicitly (or it was restored from the
    /// database, which already stores the effective profile).
    ///
    /// The downgrade is re-evaluated on every read instead of being applied
    /// once at thread start: trust can be granted at any point in a thread's
    /// life, and a user who trusts the folder mid-thread should get the tools
    /// back right away rather than having to re-pick the profile or start a new
    /// thread.
    restricted_downgrade_exempt: bool,
    project_context: Entity<ProjectContext>,
    pub(crate) templates: Arc<Templates>,
    model: ThreadModel,
    summarization_model: Option<Arc<dyn LanguageModel>>,
    thinking_enabled: bool,
    thinking_effort: Option<String>,
    speed: Option<Speed>,
    prompt_capabilities_tx: watch::Sender<acp::PromptCapabilities>,
    pub(crate) prompt_capabilities_rx: watch::Receiver<acp::PromptCapabilities>,
    pub(crate) project: Entity<Project>,
    pub(crate) action_log: Entity<ActionLog>,
    /// If this is a subagent thread, contains context about the parent
    subagent_context: Option<SubagentContext>,
    /// The user's unsent prompt text, persisted so it can be restored when reloading the thread.
    draft_prompt: Option<Vec<acp::ContentBlock>>,
    ui_scroll_position: Option<gpui::ListOffset>,
    /// Weak references to running subagent threads for cancellation propagation
    running_subagents: Vec<WeakEntity<Thread>>,
    inherits_parent_model_settings: bool,
    sandboxed_terminal_temp_dir: Option<PathBuf>,
    /// Sandbox permissions the user approved "for the rest of the thread".
    /// Shared with each tool call's event stream so repeated requests for
    /// already-granted permissions skip the approval prompt.
    /// Never persisted — lives and dies with this thread.
    sandbox_grants: Rc<RefCell<ThreadSandboxGrants>>,
    /// The permission judge's thread-scoped memory (ADR-F52): verdicts already
    /// given, and what it decided about each tool use. Shared with each tool
    /// call's event stream, the same way the sandbox grants are; the verdicts
    /// live and die with the thread, the decisions are persisted.
    permission_judge: Rc<RefCell<ThreadPermissionJudge>>,
    /// The `<repo_map>` section of the system prompt and the inputs it was
    /// built from (the byte budget plus every worktree's scan id), so it is
    /// rebuilt only when the project actually changed rather than on every
    /// turn — `agent.system_prompt_repo_map_bytes` (TODO F13 ③).
    system_prompt_repo_map: Option<(SystemPromptRepoMapKey, SharedString)>,
}

/// Identifies the project state a cached system-prompt repository map was built
/// from: the configured byte budget, and each worktree's id and scan id.
#[derive(Clone, PartialEq, Eq, Debug)]
struct SystemPromptRepoMapKey {
    max_bytes: usize,
    worktree_scan_ids: Vec<(WorktreeId, usize)>,
}

impl Thread {
    fn prompt_capabilities(model: Option<&dyn LanguageModel>) -> acp::PromptCapabilities {
        let image = model.map_or(true, |model| model.supports_images());
        acp::PromptCapabilities::new()
            .image(image)
            .embedded_context(true)
    }

    pub fn new_subagent(parent_thread: &Entity<Thread>, cx: &mut Context<Self>) -> Self {
        let project = parent_thread.read(cx).project.clone();
        let project_context = parent_thread.read(cx).project_context.clone();
        let context_server_registry = parent_thread.read(cx).context_server_registry.clone();
        let templates = parent_thread.read(cx).templates.clone();
        let model = parent_thread.read(cx).model().cloned();
        let parent_action_log = parent_thread.read(cx).action_log().clone();
        let action_log =
            cx.new(|_cx| ActionLog::new(project.clone()).with_linked_action_log(parent_action_log));
        let mut thread = Self::new_internal(
            project,
            project_context,
            context_server_registry,
            templates,
            model,
            action_log,
            cx,
        );
        thread.subagent_context = Some(SubagentContext {
            parent_thread_id: parent_thread.read(cx).id().clone(),
            depth: parent_thread.read(cx).depth() + 1,
        });
        thread.inherit_parent_settings(parent_thread, cx);
        if let Some(subagent_model) = AgentSettings::get_global(cx).subagent_model.clone() {
            thread.inherits_parent_model_settings = false;
            thread.apply_model_selection(&subagent_model, cx);
        }
        thread
    }

    pub fn new(
        project: Entity<Project>,
        project_context: Entity<ProjectContext>,
        context_server_registry: Entity<ContextServerRegistry>,
        templates: Arc<Templates>,
        model: Option<Arc<dyn LanguageModel>>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_internal(
            project.clone(),
            project_context,
            context_server_registry,
            templates,
            model,
            cx.new(|_cx| ActionLog::new(project)),
            cx,
        )
    }

    fn new_internal(
        project: Entity<Project>,
        project_context: Entity<ProjectContext>,
        context_server_registry: Entity<ContextServerRegistry>,
        templates: Arc<Templates>,
        model: Option<Arc<dyn LanguageModel>>,
        action_log: Entity<ActionLog>,
        cx: &mut Context<Self>,
    ) -> Self {
        let settings = AgentSettings::get_global(cx);
        let profile_id = settings.default_profile.clone();
        let enable_thinking = settings
            .default_model
            .as_ref()
            .is_some_and(|model| model.enable_thinking);
        let thinking_effort = settings
            .default_model
            .as_ref()
            .and_then(|model| model.effort.clone());
        let speed = settings
            .default_model
            .as_ref()
            .and_then(|model| model.speed);
        let (prompt_capabilities_tx, prompt_capabilities_rx) =
            watch::channel(Self::prompt_capabilities(model.as_deref()));
        let model = match model {
            Some(model) => ThreadModel::Ready(model),
            None => Self::user_configured_model_selection(cx)
                .map_or(ThreadModel::Unset, ThreadModel::Unresolved),
        };
        Self {
            id: acp::SessionId::new(uuid::Uuid::new_v4().to_string()),
            prompt_id: PromptId::new(),
            updated_at: Utc::now(),
            title: None,
            pending_title_generation: None,
            title_generation_error: None,
            pending_summary_generation: None,
            summary: None,
            messages: Vec::new(),
            user_store: project.read(cx).user_store(),
            running_turn: None,
            end_turn_at_next_boundary: false,
            pending_message: None,
            tools: BTreeMap::default(),
            request_token_usage: HashMap::default(),
            cumulative_token_usage: TokenUsage::default(),
            current_request_token_usage: TokenUsage::default(),
            pending_compaction_telemetry: None,
            initial_project_snapshot: {
                let project_snapshot = Self::project_snapshot(project.clone(), cx);
                cx.foreground_executor()
                    .spawn(async move { Some(project_snapshot.await) })
                    .shared()
            },
            context_server_registry,
            profile_id,
            restricted_downgrade_exempt: false,
            project_context,
            templates,
            model,
            summarization_model: None,
            thinking_enabled: enable_thinking,
            speed,
            thinking_effort,
            prompt_capabilities_tx,
            prompt_capabilities_rx,
            project,
            action_log,
            subagent_context: None,
            draft_prompt: None,
            ui_scroll_position: None,
            running_subagents: Vec::new(),
            inherits_parent_model_settings: true,
            sandboxed_terminal_temp_dir: None,
            sandbox_grants: Rc::new(RefCell::new(ThreadSandboxGrants::default())),
            permission_judge: Rc::new(RefCell::new(ThreadPermissionJudge::default())),
            system_prompt_repo_map: None,
        }
    }

    /// Copies runtime-mutable settings from the parent thread so that
    /// subagents start with the same configuration the user selected.
    /// Every property that `set_*` propagates to `running_subagents`
    /// should be inherited here as well.
    fn inherit_parent_settings(&mut self, parent_thread: &Entity<Thread>, cx: &mut Context<Self>) {
        let parent = parent_thread.read(cx);
        self.speed = parent.speed;
        self.thinking_enabled = parent.thinking_enabled;
        self.thinking_effort = parent.thinking_effort.clone();
        self.summarization_model = parent.summarization_model.clone();
        self.profile_id = parent.profile_id.clone();
        self.restricted_downgrade_exempt = parent.restricted_downgrade_exempt;
    }

    fn apply_model_selection(
        &mut self,
        selection: &LanguageModelSelection,
        cx: &mut Context<Self>,
    ) {
        let Some(model) = Self::resolve_model_from_selection(selection, cx) else {
            log::warn!(
                "failed to resolve configured subagent model: {}/{}",
                selection.provider.0,
                selection.model
            );
            return;
        };

        self.thinking_enabled = selection.enable_thinking && model.supports_thinking();
        self.thinking_effort = selection.effort.clone();
        self.speed = selection.speed.filter(|_| model.supports_fast_mode());
        self.prompt_capabilities_tx
            .send(Self::prompt_capabilities(Some(model.as_ref())))
            .log_err();
        self.model = ThreadModel::Ready(model);
    }

    pub fn id(&self) -> &acp::SessionId {
        &self.id
    }

    // Only used by Seatbelt-style sandboxes (macOS); Linux relies on bwrap's
    // tmpfs `/tmp` and Windows on the WSL bwrap tmpfs, so neither needs a
    // per-thread temp directory.
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    pub(crate) fn sandboxed_terminal_temp_dir(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Result<PathBuf> {
        if let Some(temp_dir) = &self.sandboxed_terminal_temp_dir {
            std::fs::create_dir_all(temp_dir).with_context(|| {
                format!(
                    "failed to recreate sandboxed terminal temp directory {}",
                    temp_dir.display()
                )
            })?;
            return Ok(temp_dir.clone());
        }

        let temp_dir = tempfile::Builder::new()
            .prefix("zed-agent-terminal-")
            .tempdir()
            .context("failed to create sandboxed terminal temp directory")?;
        let temp_dir = temp_dir.keep();
        self.sandboxed_terminal_temp_dir = Some(temp_dir.clone());
        cx.notify();
        Ok(temp_dir)
    }

    pub fn replay(
        &mut self,
        cx: &mut Context<Self>,
    ) -> mpsc::UnboundedReceiver<Result<ThreadEvent>> {
        let (tx, rx) = mpsc::unbounded();
        let stream = ThreadEventStream(tx);
        for (message_ix, message) in self.messages.iter().enumerate() {
            match &**message {
                Message::User(user_message) => stream.send_user_message(user_message),
                Message::Agent(assistant_message) => {
                    for content in &assistant_message.content {
                        match content {
                            AgentMessageContent::Text(text) => stream.send_text(text),
                            AgentMessageContent::Thinking { text, .. } => {
                                stream.send_thinking(text)
                            }
                            AgentMessageContent::RedactedThinking(_) => {}
                            AgentMessageContent::ToolUse(tool_use) => {
                                self.replay_tool_call(
                                    tool_use,
                                    assistant_message.tool_results.get(&tool_use.id),
                                    message_ix,
                                    &stream,
                                    cx,
                                );
                            }
                        }
                    }
                }
                Message::Resume => {}
                Message::Compaction(info) => {
                    let compaction_id = acp_thread::ContextCompactionId(
                        format!("replay-compaction-{message_ix}").into(),
                    );
                    match info {
                        CompactionInfo::Summary(summary) => {
                            stream.send_context_compaction(
                                compaction_id.clone(),
                                acp_thread::ContextCompactionStatus::Completed,
                            );
                            stream.send_context_compaction_update(compaction_id.clone(), summary);
                        }
                        CompactionInfo::ProviderNative { .. } => {
                            stream.send_context_compaction(
                                compaction_id,
                                acp_thread::ContextCompactionStatus::Completed,
                            );
                        }
                    }
                }
            }
        }
        rx
    }

    fn replay_tool_call(
        &self,
        tool_use: &LanguageModelToolUse,
        tool_result: Option<&LanguageModelToolResult>,
        owning_message_ix: usize,
        stream: &ThreadEventStream,
        cx: &mut Context<Self>,
    ) {
        // A tool call left only with the canceled sentinel produced nothing useful
        // (the sentinel is model-facing only, and is inserted exactly when a tool
        // had no real result). Don't replay it into the UI at all.
        if tool_result.is_some_and(Self::is_canceled_tool_result) {
            return;
        }

        let tool_call_id = scoped_tool_call_id(owning_message_ix, &tool_use.id);
        // The judge's verdict is meta on the tool call, not part of the
        // model-facing tool use, so rebuilding the call from the tool use alone
        // would silently lose which calls ran without a human reviewing them
        // (ADR-F52).
        let permission_judge = self
            .permission_judge
            .borrow()
            .judgement(&tool_use.id)
            .map(|judgement| acp_thread::meta_with_permission_judge(judgement.to_details()));
        let output = tool_result
            .as_ref()
            .and_then(|result| result.output.clone());
        let replay_content = tool_result.and_then(Self::tool_result_content_for_replay);
        let status = tool_result
            .as_ref()
            .map_or(acp::ToolCallStatus::Failed, |result| {
                if result.is_error {
                    acp::ToolCallStatus::Failed
                } else {
                    acp::ToolCallStatus::Completed
                }
            });

        // Recorded tool calls use the model-facing name, so a terminal call is
        // always keyed as `terminal` and resolves to the non-sandboxed
        // `TerminalTool` here, even if it originally ran under
        // `SandboxedTerminalTool`. That's safe because both variants share the
        // same `replay` behavior; replay only reconstructs UI state and never
        // re-runs the command or re-applies sandbox policy.
        let tool = self.tools.get(tool_use.name.as_ref()).cloned().or_else(|| {
            self.context_server_registry
                .read(cx)
                .servers()
                .find_map(|(_, tools)| {
                    if let Some(tool) = tools.get(tool_use.name.as_ref()) {
                        Some(tool.clone())
                    } else {
                        None
                    }
                })
        });

        let Some(tool) = tool else {
            // Tool not found (e.g., MCP server not connected after restart),
            // but still display the saved result if available.
            // We need to send both ToolCall and ToolCallUpdate events because the UI
            // only converts raw_output to displayable content in update_fields, not from_acp.
            stream
                .0
                .unbounded_send(Ok(ThreadEvent::ToolCall(
                    acp::ToolCall::new(tool_call_id.clone(), tool_use.name.to_string())
                        .status(status)
                        .raw_input(tool_use.input.to_display_json()),
                )))
                .ok();
            let mut fields = acp::ToolCallUpdateFields::new()
                .status(status)
                .raw_output(output);
            if let Some(content) = replay_content {
                fields = fields.content(content);
            }
            stream.update_tool_call_fields(&tool_call_id, fields, permission_judge);
            return;
        };

        let Ok(input) = tool_use.input.clone().into_json() else {
            return;
        };
        let title = tool.initial_title(input.clone(), cx);
        let kind = tool.kind();
        stream.send_tool_call(&tool_call_id, &tool_use.name, title, kind, input.clone());

        if let Some(content) = replay_content {
            stream.update_tool_call_fields(
                &tool_call_id,
                acp::ToolCallUpdateFields::new().content(content),
                None,
            );
        }

        if let Some(output) = output.clone() {
            // For replay, we use a dummy cancellation receiver since the tool already completed
            let (_cancellation_tx, cancellation_rx) = watch::channel(false);
            let tool_event_stream = ToolCallEventStream::new(
                tool_use.id.clone(),
                tool_call_id.clone(),
                stream.clone(),
                Some(self.project.read(cx).fs().clone()),
                cancellation_rx,
                self.sandbox_grants.clone(),
                self.permission_judge.clone(),
                Some(cx.weak_entity()),
            );
            tool.replay(input, output, tool_event_stream, cx).log_err();
        }

        stream.update_tool_call_fields(
            &tool_call_id,
            acp::ToolCallUpdateFields::new()
                .status(status)
                .raw_output(output),
            permission_judge,
        );
    }

    /// A canceled tool result carries only the model-facing `TOOL_CANCELED_MESSAGE`
    /// sentinel (inserted exactly when a tool had no real result). It's never
    /// meaningful to the user, so we detect it to skip replaying the tool call.
    fn is_canceled_tool_result(tool_result: &LanguageModelToolResult) -> bool {
        tool_result.is_error
            && matches!(
                tool_result.content.as_slice(),
                [LanguageModelToolResultContent::Text(text)]
                    if text.as_ref() == TOOL_CANCELED_MESSAGE
            )
    }

    fn tool_result_content_for_replay(
        tool_result: &LanguageModelToolResult,
    ) -> Option<Vec<acp::ToolCallContent>> {
        let has_image = tool_result
            .content
            .iter()
            .any(|part| matches!(part, LanguageModelToolResultContent::Image(_)));
        if !has_image && tool_result.output.is_some() {
            return None;
        }

        let content = tool_result
            .content
            .iter()
            .filter_map(|part| match part {
                LanguageModelToolResultContent::Text(text) => {
                    if text.is_empty() {
                        None
                    } else {
                        Some(acp::ToolCallContent::Content(acp::Content::new(
                            acp::ContentBlock::Text(acp::TextContent::new(text.to_string())),
                        )))
                    }
                }
                LanguageModelToolResultContent::Image(image) => Some(
                    acp::ToolCallContent::Content(acp::Content::new(acp::ContentBlock::Image(
                        acp::ImageContent::new(image.source.clone(), "image/png"),
                    ))),
                ),
            })
            .collect::<Vec<_>>();

        if content.is_empty() {
            None
        } else {
            Some(content)
        }
    }

    pub fn from_db(
        id: acp::SessionId,
        db_thread: DbThread,
        project: Entity<Project>,
        project_context: Entity<ProjectContext>,
        context_server_registry: Entity<ContextServerRegistry>,
        templates: Arc<Templates>,
        cx: &mut Context<Self>,
    ) -> Self {
        let settings = AgentSettings::get_global(cx);
        let profile_id = db_thread
            .profile
            .unwrap_or_else(|| settings.default_profile.clone());

        let saved_selection = db_thread.model.map(|model| SelectedModel {
            provider: model.provider.into(),
            model: model.model.into(),
        });

        let resolved_saved_model = LanguageModelRegistry::global(cx).update(cx, |registry, cx| {
            saved_selection
                .as_ref()
                .and_then(|selection| registry.select_model(selection, cx))
                .map(|configured| configured.model)
        });

        let model = match (resolved_saved_model, saved_selection) {
            (Some(model), _) => ThreadModel::Ready(model),
            (None, Some(selection)) => ThreadModel::Unresolved(selection),
            (None, None) => Self::resolve_profile_model(&profile_id, cx)
                .or_else(|| {
                    LanguageModelRegistry::global(cx).update(cx, |registry, _cx| {
                        registry.default_model().map(|model| model.model)
                    })
                })
                .map_or(ThreadModel::Unset, ThreadModel::Ready),
        };

        let (prompt_capabilities_tx, prompt_capabilities_rx) = watch::channel(
            Self::prompt_capabilities(model.as_model().map(|model| model.as_ref())),
        );

        let action_log = cx.new(|_| ActionLog::new(project.clone()));

        Self {
            id,
            prompt_id: PromptId::new(),
            title: if db_thread.title.is_empty() {
                None
            } else {
                Some(db_thread.title.clone())
            },
            pending_title_generation: None,
            title_generation_error: None,
            pending_summary_generation: None,
            summary: db_thread.detailed_summary,
            messages: db_thread.messages,
            user_store: project.read(cx).user_store(),
            running_turn: None,
            end_turn_at_next_boundary: false,
            pending_message: None,
            tools: BTreeMap::default(),
            request_token_usage: db_thread.request_token_usage.clone(),
            cumulative_token_usage: db_thread.cumulative_token_usage,
            current_request_token_usage: TokenUsage::default(),
            pending_compaction_telemetry: None,
            initial_project_snapshot: Task::ready(db_thread.initial_project_snapshot).shared(),
            context_server_registry,
            profile_id,
            // An explicit "use write anyway" is not persisted, so a reloaded
            // thread is downgraded again while the workspace is still
            // restricted. Erring towards the downgrade is the safe direction,
            // and it lifts on its own once the folder is trusted.
            restricted_downgrade_exempt: false,
            project_context,
            templates,
            model,
            summarization_model: None,
            thinking_enabled: db_thread.thinking_enabled,
            thinking_effort: db_thread.thinking_effort,
            speed: db_thread.speed,
            project,
            action_log,
            updated_at: db_thread.updated_at,
            prompt_capabilities_tx,
            prompt_capabilities_rx,
            subagent_context: db_thread.subagent_context,
            draft_prompt: db_thread.draft_prompt,
            ui_scroll_position: db_thread.ui_scroll_position.map(|sp| gpui::ListOffset {
                item_ix: sp.item_ix,
                offset_in_item: gpui::px(sp.offset_in_item),
            }),
            running_subagents: Vec::new(),
            inherits_parent_model_settings: true,
            sandboxed_terminal_temp_dir: db_thread.sandboxed_terminal_temp_dir,
            sandbox_grants: Rc::new(RefCell::new(ThreadSandboxGrants::from_db(
                &db_thread.sandbox_grants,
            ))),
            permission_judge: Rc::new(RefCell::new(ThreadPermissionJudge::from_db(
                &db_thread.permission_judgements,
            ))),
            system_prompt_repo_map: None,
        }
    }

    pub fn sandbox_status(&self, cx: &App) -> Option<(ThreadSandbox, sandbox::SandboxPolicy)> {
        if !self.sandboxing_available(cx) {
            return None;
        }
        let persistent = AgentSettings::get_global(cx).sandbox_permissions.clone();
        let git_dirs = sandbox_git_dirs(self.project.read(cx), cx);
        let settings = crate::sandboxing::settings_thread_sandbox(&persistent)
            .with_protected_paths(git_dirs.clone());
        let thread = crate::sandboxing::policy_with_protected_paths(
            self.sandbox_grants.borrow().to_policy(),
            git_dirs,
        );
        Some((settings, thread))
    }

    pub fn refresh_verified_sandbox_status(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<(SandboxStatusKey, SandboxStatusRefresh)> {
        if !self.sandboxing_available(cx) {
            return None;
        }

        let persistent = AgentSettings::get_global(cx).sandbox_permissions.clone();
        let settings_sandbox = crate::sandboxing::settings_thread_sandbox(&persistent);
        let thread_policy = self.sandbox_grants.borrow().to_policy();

        let project = self.project.read(cx);
        let baseline_writable_paths = sandbox_worktree_writable_paths(project, cx);
        let git_paths = sandbox_git_dirs(project, cx);

        let key = SandboxStatusKey {
            settings_sandbox: settings_sandbox.clone(),
            thread_policy: thread_policy.clone(),
            baseline_writable_paths: baseline_writable_paths.clone(),
            git_paths: git_paths.clone(),
        };

        Some((
            key,
            SandboxStatusRefresh::Ready(VerifiedSandboxStatus {
                settings_sandbox: settings_sandbox.with_protected_paths(git_paths.clone()),
                thread_policy: crate::sandboxing::policy_with_protected_paths(
                    thread_policy,
                    git_paths,
                ),
                baseline_writable_paths,
            }),
        ))
    }

    /// Whether agent terminal commands are sandboxed for this thread's project,
    /// so the UI can decide whether to surface the sandbox status at all.
    pub fn sandboxing_enabled(&self, cx: &App) -> bool {
        sandboxing_enabled_for_project(self.project.read(cx), cx)
    }

    /// Whether sandboxing is *applicable* for this thread's project (feature on,
    /// local project, supported platform), regardless of whether it's been
    /// turned off in settings. The UI shows the sandbox indicator whenever this
    /// is true, drawing it struck-out when sandboxing is disabled.
    pub fn sandboxing_available(&self, cx: &App) -> bool {
        sandboxing_available_for_project(self.project.read(cx), cx)
    }

    /// The directory subtrees the sandbox always grants write access to for this
    /// thread's project (its worktree roots), derived from the same source the
    /// terminal tool uses when it actually builds the sandbox.
    pub fn sandbox_baseline_writable_paths(&self, cx: &App) -> Vec<PathBuf> {
        crate::sandboxing::sandbox_worktree_writable_paths(self.project.read(cx), cx)
    }

    pub fn to_db(&self, cx: &App) -> Task<DbThread> {
        let initial_project_snapshot = self.initial_project_snapshot.clone();
        let mut thread = DbThread {
            title: self.title().unwrap_or_default(),
            messages: self.messages.clone(),
            updated_at: self.updated_at,
            detailed_summary: self.summary.clone(),
            initial_project_snapshot: None,
            cumulative_token_usage: self.cumulative_token_usage,
            request_token_usage: self.request_token_usage.clone(),
            model: (&self.model).into(),
            profile: Some(self.profile_id.clone()),
            subagent_context: self.subagent_context.clone(),
            speed: self.speed,
            thinking_enabled: self.thinking_enabled,
            thinking_effort: self.thinking_effort.clone(),
            draft_prompt: self.draft_prompt.clone(),
            ui_scroll_position: self.ui_scroll_position.map(|lo| {
                crate::db::SerializedScrollPosition {
                    item_ix: lo.item_ix,
                    offset_in_item: lo.offset_in_item.as_f32(),
                }
            }),
            sandboxed_terminal_temp_dir: self.sandboxed_terminal_temp_dir.clone(),
            sandbox_grants: self.sandbox_grants.borrow().to_db(),
            permission_judgements: self.permission_judge.borrow().to_db(),
        };

        cx.background_spawn(async move {
            let initial_project_snapshot = initial_project_snapshot.await;
            thread.initial_project_snapshot = initial_project_snapshot;
            thread
        })
    }

    /// Create a snapshot of the current project state including git information and unsaved buffers.
    fn project_snapshot(
        project: Entity<Project>,
        cx: &mut Context<Self>,
    ) -> Task<Arc<ProjectSnapshot>> {
        let task = project::telemetry_snapshot::TelemetrySnapshot::new(&project, cx);
        cx.spawn(async move |_, _| {
            let snapshot = task.await;

            Arc::new(ProjectSnapshot {
                worktree_snapshots: snapshot.worktree_snapshots,
                timestamp: Utc::now(),
            })
        })
    }

    pub fn project_context(&self) -> &Entity<ProjectContext> {
        &self.project_context
    }

    pub fn project(&self) -> &Entity<Project> {
        &self.project
    }

    pub fn action_log(&self) -> &Entity<ActionLog> {
        &self.action_log
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty() && self.title.is_none()
    }

    pub fn draft_prompt(&self) -> Option<&[acp::ContentBlock]> {
        self.draft_prompt.as_deref()
    }

    pub fn set_draft_prompt(&mut self, prompt: Option<Vec<acp::ContentBlock>>) {
        self.draft_prompt = prompt;
    }

    pub fn ui_scroll_position(&self) -> Option<gpui::ListOffset> {
        self.ui_scroll_position
    }

    pub fn set_ui_scroll_position(&mut self, position: Option<gpui::ListOffset>) {
        self.ui_scroll_position = position;
    }

    pub fn model(&self) -> Option<&Arc<dyn LanguageModel>> {
        self.model.as_model()
    }

    pub fn thread_model(&self) -> &ThreadModel {
        &self.model
    }

    pub(crate) fn ensure_model(
        &mut self,
        default_model: Option<&Arc<dyn LanguageModel>>,
        cx: &mut Context<Self>,
    ) {
        let resolved = match &self.model {
            ThreadModel::Ready(_) => return,
            ThreadModel::Unresolved(selection) => {
                LanguageModelRegistry::global(cx).update(cx, |registry, cx| {
                    registry
                        .select_model(selection, cx)
                        .map(|configured| configured.model)
                })
            }
            ThreadModel::Unset => default_model.cloned(),
        };

        if let Some(model) = resolved {
            self.set_model(model, cx);
        }
    }

    pub fn set_model(&mut self, model: Arc<dyn LanguageModel>, cx: &mut Context<Self>) {
        let old_usage = self.latest_token_usage();
        self.model = ThreadModel::Ready(model.clone());
        let new_caps = Self::prompt_capabilities(self.model.as_model().map(|model| model.as_ref()));
        let new_usage = self.latest_token_usage();
        if old_usage != new_usage {
            cx.emit(TokenUsageUpdated(new_usage));
        }
        self.prompt_capabilities_tx.send(new_caps).log_err();
        cx.emit(ModelChanged);

        for subagent in &self.running_subagents {
            subagent
                .update(cx, |thread, cx| {
                    if thread.inherits_parent_model_settings {
                        thread.set_model(model.clone(), cx);
                    }
                })
                .ok();
        }

        cx.notify()
    }

    pub fn summarization_model(&self) -> Option<&Arc<dyn LanguageModel>> {
        self.summarization_model.as_ref()
    }

    pub fn set_summarization_model(
        &mut self,
        model: Option<Arc<dyn LanguageModel>>,
        cx: &mut Context<Self>,
    ) {
        self.summarization_model = model.clone();

        for subagent in &self.running_subagents {
            subagent
                .update(cx, |thread, cx| {
                    thread.set_summarization_model(model.clone(), cx)
                })
                .ok();
        }
        cx.notify()
    }

    pub fn thinking_enabled(&self) -> bool {
        self.thinking_enabled
    }

    pub fn set_thinking_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.thinking_enabled = enabled;

        for subagent in &self.running_subagents {
            subagent
                .update(cx, |thread, cx| {
                    if thread.inherits_parent_model_settings {
                        thread.set_thinking_enabled(enabled, cx);
                    }
                })
                .ok();
        }
        cx.notify();
    }

    pub fn thinking_effort(&self) -> Option<&String> {
        self.thinking_effort.as_ref()
    }

    pub fn set_thinking_effort(&mut self, effort: Option<String>, cx: &mut Context<Self>) {
        self.thinking_effort = effort.clone();

        for subagent in &self.running_subagents {
            subagent
                .update(cx, |thread, cx| {
                    if thread.inherits_parent_model_settings {
                        thread.set_thinking_effort(effort.clone(), cx)
                    }
                })
                .ok();
        }
        cx.notify();
    }

    pub fn speed(&self) -> Option<Speed> {
        self.speed
    }

    pub fn set_speed(&mut self, speed: Speed, cx: &mut Context<Self>) {
        self.speed = Some(speed);

        for subagent in &self.running_subagents {
            subagent
                .update(cx, |thread, cx| {
                    if thread.inherits_parent_model_settings {
                        thread.set_speed(speed, cx);
                    }
                })
                .ok();
        }
        cx.notify();
    }

    pub fn last_message(&self) -> Option<&Message> {
        self.messages.last().map(std::ops::Deref::deref)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn last_received_or_pending_message(&self) -> Option<Arc<Message>> {
        if let Some(message) = self.pending_message.clone() {
            Some(Arc::new(Message::Agent(message)))
        } else {
            self.messages.last().cloned()
        }
    }

    pub fn add_default_tools(
        &mut self,
        environment: Rc<dyn ThreadEnvironment>,
        cx: &mut Context<Self>,
    ) {
        // Only update the agent location for the root thread, not for subagents.
        let update_agent_location = self.parent_thread_id().is_none();

        let language_registry = self.project.read(cx).languages().clone();
        self.add_tool(CopyPathTool::new(self.project.clone()));
        self.add_tool(CreateDirectoryTool::new(self.project.clone()));
        self.add_tool(DeletePathTool::new(
            self.project.clone(),
            self.action_log.clone(),
        ));
        self.add_tool(EditFileTool::new(
            self.project.clone(),
            cx.weak_entity(),
            self.action_log.clone(),
            language_registry.clone(),
        ));
        self.add_tool(WriteFileTool::new(
            self.project.clone(),
            cx.weak_entity(),
            self.action_log.clone(),
            language_registry,
        ));
        self.add_tool(FetchTool::new(self.project.read(cx).client().http_client()));
        self.add_tool(FindPathTool::new(self.project.clone()));
        self.add_tool(GrepTool::new(self.project.clone()));
        self.add_tool(ListDirectoryTool::new(self.project.clone()));
        self.add_tool(MovePathTool::new(self.project.clone()));
        self.add_tool(ReadFileTool::new(
            self.project.clone(),
            self.action_log.clone(),
            update_agent_location,
        ));
        self.add_tool(RepoMapTool::new(self.project.clone()));
        self.add_tool(GitDiffTool::new(self.project.clone()));
        // Register terminal tool variants; `enabled_tools` exposes the one
        // matching the current sandbox state to the model as `terminal`.
        self.add_tool(TerminalTool::new(self.project.clone(), environment.clone()));
        self.add_tool(SandboxedTerminalTool::new(
            self.project.clone(),
            environment.clone(),
        ));
        self.add_tool(WebSearchTool);

        self.add_tool(AskUserTool);

        self.add_tool(DiagnosticsTool::new(self.project.clone()));

        let code_action_store: CodeActionStore = cx.new(|_cx| None);
        self.add_tool(FindReferencesTool::new(self.project.clone()));
        self.add_tool(GetCodeActionsTool::new(
            self.project.clone(),
            code_action_store.clone(),
        ));
        self.add_tool(ApplyCodeActionTool::new(
            self.project.clone(),
            code_action_store,
        ));
        self.add_tool(GoToDefinitionTool::new(self.project.clone()));
        self.add_tool(RenameTool::new(self.project.clone()));

        self.add_tool(HoverTool::new(self.project.clone()));
        self.add_tool(SearchSymbolsTool::new(self.project.clone()));
        let assist_source = AssistSource {
            id: self.id.0.clone().into(),
            label: ZED_AGENT_ASSIST_LABEL.into(),
        };
        self.add_tool(HighlightCodeTool::new(
            self.project.clone(),
            assist_source.clone(),
        ));
        self.add_tool(AnnotateCodeTool::new(
            self.project.clone(),
            assist_source.clone(),
        ));
        self.add_tool(CodeTourTool::new(
            self.project.clone(),
            assist_source.clone(),
        ));
        self.add_tool(ClearAnnotationsTool::new(
            self.project.clone(),
            assist_source,
        ));

        if self.depth() < MAX_SUBAGENT_DEPTH {
            self.add_tool(SpawnAgentTool::new(environment.clone()));
        }

        // Sibling-thread tools are exposed at every depth: a subagent should
        // still be able to kick off independent sibling work on behalf of the
        // user, even when it can no longer nest further subagents. Visibility
        // to the model is gated by `CreateThreadToolFeatureFlag` in
        // `Thread::enabled_tools`.
        self.add_tool(CreateThreadTool::new(environment.clone()));
        self.add_tool(ListAgentsAndModelsTool::new(environment));
    }

    pub fn add_tool<T: AgentTool>(&mut self, tool: T) {
        debug_assert!(
            !self.tools.contains_key(T::NAME),
            "Duplicate tool name: {}",
            T::NAME,
        );
        self.tools.insert(T::NAME.into(), tool.erase());
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn remove_tool(&mut self, name: &str) -> bool {
        self.tools.remove(name).is_some()
    }

    /// The profile the thread currently runs with: the user's choice, or
    /// `minimal` while the workspace is restricted (see
    /// [`Self::profile_for_restricted_workspace`]). Recomputed on every read so
    /// that trusting the folder mid-thread restores the tools immediately.
    pub fn profile(&self, cx: &App) -> AgentProfileId {
        if self.restricted_downgrade_exempt {
            return self.profile_id.clone();
        }
        Self::profile_for_restricted_workspace(self.profile_id.clone(), &self.project, cx).0
    }

    /// Whether the restricted workspace is currently forcing this thread down to
    /// `minimal`, i.e. the model has no tools at all.
    pub fn profile_was_downgraded(&self, cx: &App) -> bool {
        !self.restricted_downgrade_exempt
            && Self::profile_for_restricted_workspace(self.profile_id.clone(), &self.project, cx).1
    }

    /// Applies the restricted-workspace downgrade to the user's chosen profile.
    /// In a restricted workspace, the built-in `write`/`edit`/`ask` profiles
    /// are downgraded to `minimal` — but only when both the chosen profile and
    /// `minimal` are unmodified, shipped defaults, so we never override a user's
    /// custom or customized profiles.
    ///
    /// Returns the (possibly downgraded) profile and whether a downgrade
    /// happened.
    fn profile_for_restricted_workspace(
        profile_id: AgentProfileId,
        project: &Entity<Project>,
        cx: &App,
    ) -> (AgentProfileId, bool) {
        let is_write_or_ask = profile_id.as_str() == builtin_profiles::WRITE
            || profile_id.as_str() == builtin_profiles::EDIT
            || profile_id.as_str() == builtin_profiles::ASK;
        let minimal = AgentProfileId(builtin_profiles::MINIMAL.into());
        if is_write_or_ask
            && TrustedWorktrees::has_restricted_worktrees(&project.read(cx).worktree_store(), cx)
            && AgentProfileSettings::is_unmodified_default(&profile_id, cx)
            && AgentProfileSettings::is_unmodified_default(&minimal, cx)
        {
            (minimal, true)
        } else {
            (profile_id, false)
        }
    }

    pub fn set_profile(&mut self, profile_id: AgentProfileId, cx: &mut Context<Self>) {
        // An explicit selection means the automatic downgrade no longer
        // applies, even if the user re-selects the same profile.
        self.restricted_downgrade_exempt = true;

        if self.profile_id == profile_id {
            return;
        }

        self.profile_id = profile_id.clone();

        // Swap to the profile's preferred model when available.
        if let Some(model) = Self::resolve_profile_model(&self.profile_id, cx) {
            self.set_model(model, cx);
        }

        for subagent in &self.running_subagents {
            subagent
                .update(cx, |thread, cx| thread.set_profile(profile_id.clone(), cx))
                .ok();
        }
    }

    pub fn cancel(&mut self, cx: &mut Context<Self>) -> Task<()> {
        for subagent in self.running_subagents.drain(..) {
            if let Some(subagent) = subagent.upgrade() {
                subagent.update(cx, |thread, cx| thread.cancel(cx)).detach();
            }
        }

        let Some(running_turn) = self.running_turn.take() else {
            self.flush_pending_message(cx);
            return Task::ready(());
        };

        let turn_task = running_turn.cancel();

        cx.spawn(async move |this, cx| {
            turn_task.await;
            this.update(cx, |this, cx| {
                this.flush_pending_message(cx);
            })
            .ok();
        })
    }

    pub fn set_end_turn_at_next_boundary(&mut self, end_at_boundary: bool) {
        self.end_turn_at_next_boundary = end_at_boundary;
    }

    pub fn end_turn_at_next_boundary(&self) -> bool {
        self.end_turn_at_next_boundary
    }

    fn accumulate_token_usage(&mut self, update: language_model::TokenUsage) {
        let previous_accounted_usage = self.current_request_token_usage;
        let current_accounted_usage = TokenUsage {
            input_tokens: previous_accounted_usage
                .input_tokens
                .max(update.input_tokens),
            output_tokens: previous_accounted_usage
                .output_tokens
                .max(update.output_tokens),
            cache_creation_input_tokens: previous_accounted_usage
                .cache_creation_input_tokens
                .max(update.cache_creation_input_tokens),
            cache_read_input_tokens: previous_accounted_usage
                .cache_read_input_tokens
                .max(update.cache_read_input_tokens),
        };
        self.current_request_token_usage = current_accounted_usage;
        self.cumulative_token_usage = self.cumulative_token_usage
            + TokenUsage {
                input_tokens: current_accounted_usage
                    .input_tokens
                    .saturating_sub(previous_accounted_usage.input_tokens),
                output_tokens: current_accounted_usage
                    .output_tokens
                    .saturating_sub(previous_accounted_usage.output_tokens),
                cache_creation_input_tokens: current_accounted_usage
                    .cache_creation_input_tokens
                    .saturating_sub(previous_accounted_usage.cache_creation_input_tokens),
                cache_read_input_tokens: current_accounted_usage
                    .cache_read_input_tokens
                    .saturating_sub(previous_accounted_usage.cache_read_input_tokens),
            };
    }

    fn update_token_usage(&mut self, update: language_model::TokenUsage, cx: &mut Context<Self>) {
        self.accumulate_token_usage(update);

        let Some(last_user_message) = self.last_user_message() else {
            return;
        };

        self.request_token_usage
            .insert(last_user_message.id.clone(), update);
        cx.emit(TokenUsageUpdated(self.latest_token_usage()));
        cx.notify();
    }

    /// Records that the last request overflowed the model's context window so
    /// the token usage indicator reports `Exceeded` instead of the stale usage
    /// from the last successful request. Providers don't report usage for
    /// failed requests, so we synthesize one from the reported overflow (when
    /// available) or the model's context size.
    fn mark_token_limit_exceeded(&mut self, tokens: Option<u64>, cx: &mut Context<Self>) {
        let Some(model) = self.model() else {
            return;
        };
        let input_tokens = tokens.unwrap_or(0).max(model.max_token_count());
        let Some(last_user_message) = self.last_user_message() else {
            return;
        };

        self.request_token_usage.insert(
            last_user_message.id.clone(),
            language_model::TokenUsage {
                input_tokens,
                ..Default::default()
            },
        );
        cx.emit(TokenUsageUpdated(self.latest_token_usage()));
        cx.notify();
    }

    pub fn truncate(
        &mut self,
        client_user_message_id: ClientUserMessageId,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        self.cancel(cx).detach();
        // Clear pending message since cancel will try to flush it asynchronously,
        // and we don't want that content to be added after we truncate
        self.pending_message.take();
        let Some(position) = self.messages.iter().position(|msg| {
            matches!(&**msg, Message::User(UserMessage { id, .. }) if id == &client_user_message_id)
        }) else {
            return Err(anyhow!("Message not found"));
        };

        for message in self.messages.drain(position..) {
            match &*message {
                Message::User(message) => {
                    self.request_token_usage.remove(&message.id);
                }
                Message::Agent(_) | Message::Resume | Message::Compaction(_) => {}
            }
        }
        self.clear_summary();
        cx.notify();
        Ok(())
    }

    pub fn latest_request_token_usage(&self) -> Option<language_model::TokenUsage> {
        let last_user_message = self.last_user_message()?;
        let tokens = self.request_token_usage.get(&last_user_message.id)?;
        Some(*tokens)
    }

    pub fn cumulative_token_usage(&self) -> language_model::TokenUsage {
        self.cumulative_token_usage
    }

    pub fn latest_token_usage(&self) -> Option<acp_thread::TokenUsage> {
        let usage = self.latest_request_token_usage()?;
        let model = self.model()?;
        let input_tokens = total_input_tokens(usage);

        Some(acp_thread::TokenUsage {
            max_tokens: model.max_token_count(),
            max_output_tokens: model.max_output_tokens(),
            used_tokens: usage.total_tokens(),
            input_tokens,
            output_tokens: usage.output_tokens,
        })
    }

    /// Get the total input token count as of the message before the given message.
    ///
    /// Returns `None` if:
    /// - `target_id` is the first message (no previous message)
    /// - The previous message hasn't received a response yet (no usage data)
    /// - `target_id` is not found in the messages
    pub fn tokens_before_message(&self, target_id: &ClientUserMessageId) -> Option<u64> {
        let mut previous_user_message_id: Option<&ClientUserMessageId> = None;

        for message in &self.messages {
            if let Message::User(user_msg) = &**message {
                if &user_msg.id == target_id {
                    let prev_id = previous_user_message_id?;
                    let usage = self.request_token_usage.get(prev_id)?;
                    return Some(total_input_tokens(*usage));
                }
                previous_user_message_id = Some(&user_msg.id);
            }
        }
        None
    }

    /// Look up the active profile and resolve its preferred model if one is configured.
    fn resolve_profile_model(
        profile_id: &AgentProfileId,
        cx: &mut Context<Self>,
    ) -> Option<Arc<dyn LanguageModel>> {
        let selection = AgentSettings::get_global(cx)
            .profiles
            .get(profile_id)?
            .default_model
            .clone()?;
        Self::resolve_model_from_selection(&selection, cx)
    }

    fn user_configured_model_selection(cx: &App) -> Option<SelectedModel> {
        let selection = SettingsStore::global(cx)
            .raw_user_settings()?
            .content
            .agent
            .as_ref()?
            .default_model
            .as_ref()?;
        Some(SelectedModel {
            provider: LanguageModelProviderId::from(selection.provider.0.clone()),
            model: LanguageModelId::from(selection.model.clone()),
        })
    }

    /// Translate a stored model selection into the configured model from the registry.
    fn resolve_model_from_selection(
        selection: &LanguageModelSelection,
        cx: &mut Context<Self>,
    ) -> Option<Arc<dyn LanguageModel>> {
        let selected = SelectedModel {
            provider: LanguageModelProviderId::from(selection.provider.0.clone()),
            model: LanguageModelId::from(selection.model.clone()),
        };
        LanguageModelRegistry::global(cx).update(cx, |registry, cx| {
            registry
                .select_model(&selected, cx)
                .map(|configured| configured.model)
        })
    }

    pub fn resume(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Result<mpsc::UnboundedReceiver<Result<ThreadEvent>>> {
        self.messages.push(Arc::new(Message::Resume));
        cx.notify();

        log::debug!("Total messages in thread: {}", self.messages.len());
        self.run_turn(cx)
    }

    /// Sending a message results in the model streaming a response, which could include tool calls.
    /// After calling tools, the model will stops and waits for any outstanding tool calls to be completed and their results sent.
    /// The returned channel will report all the occurrences in which the model stops before erroring or ending its turn.
    pub fn send<T>(
        &mut self,
        id: ClientUserMessageId,
        content: impl IntoIterator<Item = T>,
        cx: &mut Context<Self>,
    ) -> Result<mpsc::UnboundedReceiver<Result<ThreadEvent>>>
    where
        T: Into<UserMessageContent>,
    {
        let content = content.into_iter().map(Into::into).collect::<Arc<_>>();
        log::debug!("Thread::send content: {:?}", content);

        self.messages
            .push(Arc::new(Message::User(UserMessage { id, content })));
        cx.notify();

        self.send_existing(cx)
    }

    pub fn send_existing(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Result<mpsc::UnboundedReceiver<Result<ThreadEvent>>> {
        let model = self
            .model()
            .ok_or_else(|| anyhow!(NoModelConfiguredError))?;

        log::info!("Thread::send called with model: {}", model.name().0);
        self.advance_prompt_id();

        log::debug!("Total messages in thread: {}", self.messages.len());
        self.run_turn(cx)
    }

    /// Force a manual context compaction using the summary strategy,
    /// regardless of the current token usage or context window size.
    pub fn compact(
        &mut self,
        id: ClientUserMessageId,
        cx: &mut Context<Self>,
    ) -> Result<mpsc::UnboundedReceiver<Result<ThreadEvent>>> {
        let model = self
            .compaction_model(cx)
            .ok_or_else(|| anyhow!(NoModelConfiguredError))?;

        // Flush any pending message and cancel an in-flight turn before we
        // start, mirroring `run_turn` so a stray completion can't race with the
        // compaction we're about to perform.
        self.flush_pending_message(cx);
        self.cancel(cx).detach();

        let compaction = self.forced_compaction_target_ix().map(|request_end_ix| {
            self.advance_prompt_id();
            let request = self.build_compaction_request(request_end_ix, &model, cx);
            self.current_request_token_usage = TokenUsage::default();
            (model.clone(), request)
        });

        if compaction.is_some() {
            self.pending_compaction_telemetry =
                self.build_compaction_telemetry("manual", &model, cx);
        }

        self.clear_summary();
        cx.notify();

        let (events_tx, events_rx) = mpsc::unbounded::<Result<ThreadEvent>>();
        let event_stream = ThreadEventStream(events_tx);
        let (cancellation_tx, mut cancellation_rx) = watch::channel(false);
        let task = cx.spawn({
            let event_stream = event_stream.clone();
            async move |this, cx| {
                let result = if let Some((model, request)) = compaction {
                    Self::stream_compaction(
                        &this,
                        &event_stream,
                        cancellation_rx.clone(),
                        model,
                        request,
                        CompactionInsertion::Manual { marker_id: id },
                        cx,
                    )
                    .await
                } else {
                    Ok(ControlFlow::Continue(()))
                };

                // If we were cancelled, `cancel()` already took `running_turn`
                // (possibly for a new turn), so leave it alone.
                if *cancellation_rx.borrow() {
                    this.update(cx, |this, _| {
                        this.emit_compaction_telemetry_outcome("canceled", None)
                    })
                    .log_err();
                    return;
                }

                match result {
                    // On success, the telemetry event is deferred until the next
                    // completion reports usage (see `handle_completion_event`),
                    // so we leave `pending_compaction_telemetry` in place here.
                    Ok(_) => event_stream.send_stop(acp::StopReason::EndTurn),
                    Err(error) => {
                        log::error!("Manual compaction failed: {:?}", error);
                        this.update(cx, |this, _| {
                            this.emit_compaction_telemetry_outcome(
                                "failed",
                                Some(error.to_string()),
                            )
                        })
                        .log_err();
                        event_stream.send_error(error);
                    }
                }

                _ = this.update(cx, |this, _| this.running_turn.take());
            }
        });
        self.running_turn = Some(RunningTurn::new(
            event_stream,
            BTreeMap::default(),
            cancellation_tx,
            task,
        ));

        Ok(events_rx)
    }

    pub fn push_acp_user_block(
        &mut self,
        id: ClientUserMessageId,
        blocks: impl IntoIterator<Item = acp::ContentBlock>,
        path_style: PathStyle,
        cx: &mut Context<Self>,
    ) {
        let content = blocks
            .into_iter()
            .map(|block| UserMessageContent::from_content_block(block, path_style))
            .collect::<Arc<_>>();
        self.messages
            .push(Arc::new(Message::User(UserMessage { id, content })));
        cx.notify();
    }

    pub fn push_acp_agent_block(&mut self, block: acp::ContentBlock, cx: &mut Context<Self>) {
        let text = match block {
            acp::ContentBlock::Text(text_content) => text_content.text,
            acp::ContentBlock::Image(_) => "[image]".to_string(),
            acp::ContentBlock::Audio(_) => "[audio]".to_string(),
            acp::ContentBlock::ResourceLink(resource_link) => resource_link.uri,
            acp::ContentBlock::Resource(resource) => match resource.resource {
                acp::EmbeddedResourceResource::TextResourceContents(resource) => resource.uri,
                acp::EmbeddedResourceResource::BlobResourceContents(resource) => resource.uri,
                _ => "[resource]".to_string(),
            },
            _ => "[unknown]".to_string(),
        };

        self.messages.push(Arc::new(Message::Agent(AgentMessage {
            content: vec![AgentMessageContent::Text(text)],
            ..Default::default()
        })));
        cx.notify();
    }

    fn run_turn(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Result<mpsc::UnboundedReceiver<Result<ThreadEvent>>> {
        // Flush the old pending message synchronously before cancelling,
        // to avoid a race where the detached cancel task might flush the NEW
        // turn's pending message instead of the old one.
        self.flush_pending_message(cx);
        self.cancel(cx).detach();

        let (events_tx, events_rx) = mpsc::unbounded::<Result<ThreadEvent>>();
        let event_stream = ThreadEventStream(events_tx);
        let message_ix = self.messages.len().saturating_sub(1);
        self.clear_summary();
        let tools = self.enabled_tools(cx);
        let (cancellation_tx, mut cancellation_rx) = watch::channel(false);
        let task = cx.spawn({
            let event_stream = event_stream.clone();
            async move |this, cx| {
                log::debug!("Starting agent turn execution");

                let turn_result =
                    Self::run_turn_internal(&this, &event_stream, cancellation_rx.clone(), cx)
                        .await;

                // Check if we were cancelled - if so, cancel() already took running_turn
                // and we shouldn't touch it (it might be a NEW turn now)
                let was_cancelled = *cancellation_rx.borrow();
                if was_cancelled {
                    log::debug!("Turn was cancelled, skipping cleanup");
                    return;
                }

                _ = this.update(cx, |this, cx| this.flush_pending_message(cx));

                match turn_result {
                    Ok(()) => {
                        log::debug!("Turn execution completed");
                        event_stream.send_stop(acp::StopReason::EndTurn);
                    }
                    Err(error) => {
                        log::error!("Turn execution failed: {:?}", error);
                        match error.downcast::<CompletionError>() {
                            Ok(CompletionError::Refusal) => {
                                event_stream.send_stop(acp::StopReason::Refusal);
                                _ = this.update(cx, |this, _| this.messages.truncate(message_ix));
                            }
                            Ok(CompletionError::MaxTokens) => {
                                event_stream.send_stop(acp::StopReason::MaxTokens);
                            }
                            Ok(CompletionError::Other(error)) | Err(error) => {
                                event_stream.send_error(error);
                            }
                        }
                    }
                }

                _ = this.update(cx, |this, _| this.running_turn.take());
            }
        });
        self.running_turn = Some(RunningTurn::new(event_stream, tools, cancellation_tx, task));
        Ok(events_rx)
    }

    /// Rebuilds the system prompt's `<repo_map>` section when
    /// `agent.system_prompt_repo_map_bytes` is on and the project changed since
    /// the last build. Failures are logged and leave the previous map (or none)
    /// in place: a repository map is context, never a precondition for sending.
    async fn refresh_system_prompt_repo_map(this: &WeakEntity<Self>, cx: &mut AsyncApp) {
        let Ok(Some((project, key))) = this.update(cx, |this, cx| {
            let max_bytes = AgentSettings::get_global(cx).system_prompt_repo_map_bytes;
            if max_bytes == 0 {
                this.system_prompt_repo_map = None;
                return None;
            }
            let key = SystemPromptRepoMapKey {
                max_bytes,
                worktree_scan_ids: this
                    .project
                    .read(cx)
                    .worktrees(cx)
                    .map(|worktree| {
                        let worktree = worktree.read(cx);
                        (worktree.id(), worktree.scan_id())
                    })
                    .collect(),
            };
            if this
                .system_prompt_repo_map
                .as_ref()
                .is_some_and(|(cached_key, _)| *cached_key == key)
            {
                return None;
            }
            Some((this.project.clone(), key))
        }) else {
            return;
        };

        let map = RepoMapTool::build_map(
            project,
            RepoMapToolInput {
                path: None,
                max_bytes: Some(key.max_bytes),
            },
            cx,
        )
        .await;

        match map {
            Ok(map) => {
                this.update(cx, |this, _cx| {
                    this.system_prompt_repo_map = Some((key, map.into()));
                })
                .log_err();
            }
            Err(error) => {
                log::warn!("failed to build the system prompt repository map: {error:#}");
            }
        }
    }

    async fn run_turn_internal(
        this: &WeakEntity<Self>,
        event_stream: &ThreadEventStream,
        mut cancellation_rx: watch::Receiver<bool>,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        Self::refresh_system_prompt_repo_map(this, cx).await;

        let mut attempt = 0;
        let mut intent = CompletionIntent::UserPrompt;
        // Set when a refusal fallback occurs so subsequent iterations use the fallback model.
        let mut refusal_fallback_model: Option<Arc<dyn LanguageModel>> = None;
        loop {
            match Self::perform_compaction_if_needed(
                this,
                event_stream,
                cancellation_rx.clone(),
                cx,
            )
            .await
            {
                // On success the telemetry event is deferred until the
                // completion below reports usage, so we can record an
                // accurate post-compaction context size (see
                // `handle_completion_event`).
                Ok(ControlFlow::Continue(())) => {}
                Ok(ControlFlow::Break(())) => {
                    this.update(cx, |this, _| {
                        this.emit_compaction_telemetry_outcome("canceled", None)
                    })?;
                    return Ok(());
                }
                Err(error) => {
                    log::error!("Compaction failed: {}", error);
                    let error_message = error.to_string();
                    match error.downcast::<LanguageModelCompletionError>() {
                        Ok(error) => {
                            attempt += 1;
                            match Self::retry_completion_error(
                                this,
                                event_stream,
                                &mut cancellation_rx,
                                error,
                                attempt,
                                cx,
                            )
                            .await
                            {
                                Ok(ControlFlow::Break(())) => {
                                    this.update(cx, |this, _| {
                                        this.emit_compaction_telemetry_outcome("canceled", None)
                                    })?;
                                    return Ok(());
                                }
                                Ok(ControlFlow::Continue(())) => {
                                    this.update(cx, |this, _| {
                                        if let Some(telemetry) =
                                            this.pending_compaction_telemetry.as_mut()
                                        {
                                            telemetry.retries += 1;
                                        }
                                    })?;
                                    continue;
                                }
                                Err(retry_error) => {
                                    this.update(cx, |this, _| {
                                        this.emit_compaction_telemetry_outcome(
                                            "failed",
                                            Some(error_message),
                                        )
                                    })?;
                                    return Err(retry_error);
                                }
                            }
                        }
                        Err(error) => {
                            this.update(cx, |this, _| {
                                this.emit_compaction_telemetry_outcome(
                                    "failed",
                                    Some(error_message),
                                )
                            })?;
                            return Err(error);
                        }
                    }
                }
            }

            // Re-read the model and refresh tools on each iteration so that
            // mid-turn changes (e.g. the user switches model, toggles tools,
            // or changes profile) take effect between tool-call rounds.
            // If a refusal fallback is active, use that model instead.
            let (model, request) = this.update(cx, |this, cx| {
                let model = refusal_fallback_model
                    .clone()
                    .or_else(|| this.model().cloned())
                    .ok_or_else(|| anyhow!(NoModelConfiguredError))?;
                this.refresh_turn_tools(cx);
                let request = this.build_completion_request(intent, cx)?;
                this.current_request_token_usage = TokenUsage::default();
                anyhow::Ok((model, request))
            })??;

            telemetry::event!(
                "Agent Thread Completion",
                thread_id = this.read_with(cx, |this, _| this.id.to_string())?,
                parent_thread_id = this.read_with(cx, |this, _| this
                    .parent_thread_id()
                    .map(|id| id.to_string()))?,
                prompt_id = this.read_with(cx, |this, _| this.prompt_id.to_string())?,
                model = model.telemetry_id(),
                model_provider = model.provider_id().to_string(),
                attempt
            );

            log::debug!("Calling model.stream_completion, attempt {}", attempt);

            let (mut events, mut error) = match model.stream_completion(request, cx).await {
                Ok(events) => (events.fuse(), None),
                Err(err) => (stream::empty().boxed().fuse(), Some(err)),
            };
            let mut tool_results: FuturesUnordered<Task<(usize, LanguageModelToolResult)>> =
                FuturesUnordered::new();
            let mut early_tool_results: Vec<(usize, LanguageModelToolResult)> = Vec::new();
            let mut cancelled = false;
            let mut had_refusal = false;
            loop {
                // Race between getting the first event, tool completion, and cancellation.
                let first_event = futures::select! {
                    event = events.next().fuse() => event,
                    tool_result = futures::StreamExt::select_next_some(&mut tool_results) => {
                        let (owning_message_ix, tool_result) = tool_result;
                        let is_error = tool_result.is_error;
                        let is_still_streaming = this
                            .read_with(cx, |this, _cx| {
                                this.running_turn
                                    .as_ref()
                                    .and_then(|turn| turn.streaming_tool_inputs.get(&tool_result.tool_use_id))
                                    .map_or(false, |inputs| !inputs.has_received_final())
                            })
                            .unwrap_or(false);

                        early_tool_results.push((owning_message_ix, tool_result));

                        // Only break if the tool errored and we are still
                        // streaming the input of the tool. If the tool errored
                        // but we are no longer streaming its input (i.e. there
                        // are parallel tool calls) we want to continue
                        // processing those tool inputs.
                        if is_error && is_still_streaming {
                            break;
                        }
                        continue;
                    }
                    _ = cancellation_rx.changed().fuse() => {
                        if *cancellation_rx.borrow() {
                            cancelled = true;
                            break;
                        }
                        continue;
                    }
                };
                let Some(first_event) = first_event else {
                    break;
                };

                // Collect all immediately available events to process as a batch
                let mut batch = vec![first_event];
                while let Some(event) = events.next().now_or_never().flatten() {
                    batch.push(event);
                }

                // Process the batch in a single update
                let batch_result = this.update(cx, |this, cx| {
                    let mut batch_tool_results = Vec::new();
                    let mut batch_error = None;

                    for event in batch {
                        log::trace!("Received completion event: {:?}", event);
                        match event {
                            Ok(event) => {
                                match this.handle_completion_event(
                                    event,
                                    event_stream,
                                    cancellation_rx.clone(),
                                    cx,
                                ) {
                                    Ok(Some(task)) => batch_tool_results.push(task),
                                    Ok(None) => {}
                                    Err(err) => {
                                        batch_error = Some(err);
                                        break;
                                    }
                                }
                            }
                            Err(err) => {
                                batch_error = Some(err.into());
                                break;
                            }
                        }
                    }

                    cx.notify();
                    (batch_tool_results, batch_error)
                })?;

                tool_results.extend(batch_result.0);
                if let Some(err) = batch_result.1 {
                    let is_refusal = err
                        .downcast_ref::<CompletionError>()
                        .is_some_and(|e| matches!(e, CompletionError::Refusal));
                    if is_refusal {
                        log::info!("Model refused request; checking for fallback model");
                        had_refusal = true;
                        break;
                    }
                    error = Some(err.downcast()?);
                    break;
                }
            }

            // Drop the stream to release the rate limit permit before tool execution.
            // The stream holds a semaphore guard that limits concurrent requests.
            // Without this, the permit would be held during potentially long-running
            // tool execution, which could cause deadlocks when tools spawn subagents
            // that need their own permits.
            drop(events);

            // Drop streaming tool input senders that never received their final input.
            // This prevents deadlock when the LLM stream ends (e.g. because of an error)
            // before sending a tool use with `is_input_complete: true`.
            this.update(cx, |this, _cx| {
                if let Some(running_turn) = this.running_turn.as_mut() {
                    if running_turn.streaming_tool_inputs.is_empty() {
                        return;
                    }
                    log::warn!("Dropping partial tool inputs because the stream ended");
                    running_turn.streaming_tool_inputs.drain();
                }
            })?;

            if had_refusal {
                let maybe_fallback = this.update(cx, |this, cx| -> Option<Arc<dyn LanguageModel>> {
                    let current_model = refusal_fallback_model.as_ref().or(this.model())?;
                    let fallback_id = match current_model.refusal_fallback_model_id() {
                        Some(id) => id,
                        None => {
                            log::info!(
                                "Refusal fallback: no fallback configured for model {} (provider {})",
                                current_model.id().0,
                                current_model.provider_id()
                            );
                            return None;
                        }
                    };
                    let provider_id = current_model.provider_id();
                    let found = LanguageModelRegistry::global(cx)
                        .read(cx)
                        .available_models(cx)
                        .find(|m| {
                            m.provider_id() == provider_id && m.id().0.as_ref() == fallback_id
                        });
                    if found.is_none() {
                        log::info!(
                            "Refusal fallback: fallback model {}/{} not found in available models",
                            provider_id,
                            fallback_id
                        );
                    }
                    found
                })?;

                if let Some(fallback) = maybe_fallback {
                    log::info!("Refusal fallback: retrying with {}", fallback.id().0);
                    let fallback_name = fallback.name().0.clone();
                    this.update(cx, |this, cx| {
                        this.pending_message = None;
                        this.set_model(fallback.clone(), cx);
                    })?;
                    event_stream.send_retry(acp_thread::RetryStatus {
                        last_error: "Safety filter triggered".into(),
                        attempt: 1,
                        max_attempts: 1,
                        started_at: Instant::now(),
                        duration: Duration::MAX,
                        meta: Some(acp_thread::meta_with_refusal_fallback(&fallback_name)),
                    });
                    refusal_fallback_model = Some(fallback);
                    continue;
                }
                log::info!("Request refused with no fallback model available");
                return Err(CompletionError::Refusal.into());
            }

            let end_turn = tool_results.is_empty() && early_tool_results.is_empty();

            for (owning_message_ix, tool_result) in early_tool_results {
                Self::process_tool_result(this, event_stream, cx, owning_message_ix, tool_result)?;
            }
            while let Some((owning_message_ix, tool_result)) = tool_results.next().await {
                Self::process_tool_result(this, event_stream, cx, owning_message_ix, tool_result)?;
            }

            this.update(cx, |this, cx| {
                this.flush_pending_message(cx);
                if this.title.is_none() {
                    this.generate_title(cx);
                }
            })?;

            if cancelled {
                log::debug!("Turn cancelled by user, exiting");
                return Ok(());
            }

            if let Some(error) = error {
                attempt += 1;
                match Self::retry_completion_error(
                    this,
                    event_stream,
                    &mut cancellation_rx,
                    error,
                    attempt,
                    cx,
                )
                .await?
                {
                    ControlFlow::Break(_) => return Ok(()),
                    ControlFlow::Continue(_) => {}
                }
                this.update(cx, |this, _cx| {
                    if let Some(Message::Agent(message)) = this.last_message() {
                        if message.tool_results.is_empty() {
                            intent = CompletionIntent::UserPrompt;
                            this.messages.push(Arc::new(Message::Resume));
                        }
                    }
                })?;
            } else if end_turn {
                return Ok(());
            } else {
                let end_at_boundary =
                    this.update(cx, |this, _| this.end_turn_at_next_boundary())?;
                if end_at_boundary {
                    log::debug!("Steering message queued, ending turn at message boundary");
                    return Ok(());
                }
                intent = CompletionIntent::ToolResults;
                attempt = 0;
            }
        }
    }

    /// Computes the retry status for a failed completion, notifies listeners,
    /// and waits out the backoff delay (or returns early if the turn is
    /// cancelled while waiting). Returns an error if the completion is not
    /// retryable or retries are exhausted.
    async fn retry_completion_error(
        this: &WeakEntity<Self>,
        event_stream: &ThreadEventStream,
        cancellation_rx: &mut watch::Receiver<bool>,
        error: LanguageModelCompletionError,
        attempt: u8,
        cx: &mut AsyncApp,
    ) -> Result<ControlFlow<()>> {
        let retry = this.update(cx, |this, cx| {
            let user_store = this.user_store.read(cx);
            this.handle_completion_error(error, attempt, user_store.plan(), cx)
        })??;
        let timer = cx.background_executor().timer(retry.duration);
        event_stream.send_retry(retry);
        futures::select! {
            _ = timer.fuse() => {}
            _ = cancellation_rx.changed().fuse() => {
                if *cancellation_rx.borrow() {
                    log::debug!("Turn cancelled during retry delay, exiting");
                    return Ok(ControlFlow::Break(()));
                }
            }
        }
        Ok(ControlFlow::Continue(()))
    }

    async fn perform_compaction_if_needed(
        this: &WeakEntity<Self>,
        event_stream: &ThreadEventStream,
        cancellation_rx: watch::Receiver<bool>,
        cx: &mut AsyncApp,
    ) -> Result<ControlFlow<()>> {
        let Some((model, request, insertion_ix)) = this.update(cx, |this, cx| {
            let insertion_ix = this.compaction_message_target_ix(cx)?;
            let model = this.compaction_model(cx)?;
            let request = this.build_compaction_request(insertion_ix, &model, cx);
            this.current_request_token_usage = TokenUsage::default();
            // Preserve telemetry across retries so the retry count keeps
            // accumulating rather than resetting on each attempt.
            if this.pending_compaction_telemetry.is_none() {
                this.pending_compaction_telemetry =
                    this.build_compaction_telemetry("auto", &model, cx);
            }
            Some((model, request, insertion_ix))
        })?
        else {
            return Ok(ControlFlow::Continue(()));
        };

        Self::stream_compaction(
            this,
            event_stream,
            cancellation_rx,
            model,
            request,
            CompactionInsertion::Auto { insertion_ix },
            cx,
        )
        .await
    }

    async fn stream_compaction(
        this: &WeakEntity<Self>,
        event_stream: &ThreadEventStream,
        mut cancellation_rx: watch::Receiver<bool>,
        model: Arc<dyn LanguageModel>,
        request: LanguageModelRequest,
        insertion: CompactionInsertion,
        cx: &mut AsyncApp,
    ) -> Result<ControlFlow<()>> {
        log::debug!("Running compaction");
        let compaction_id = acp_thread::ContextCompactionId(Uuid::new_v4().to_string().into());
        event_stream.send_context_compaction(
            compaction_id.clone(),
            acp_thread::ContextCompactionStatus::InProgress,
        );
        let stream = futures::select! {
            result = model.stream_completion(request, cx).fuse() => result,
            _ = cancellation_rx.changed().fuse() => {
                if *cancellation_rx.borrow() {
                    log::debug!("Compaction cancelled before request started");
                    return Ok(ControlFlow::Break(()));
                }
                return Ok(ControlFlow::Continue(()));
            }
        };
        let mut stream = stream?;

        let mut summary = String::new();
        loop {
            let event = futures::select! {
                event = stream.next().fuse() => event,
                _ = cancellation_rx.changed().fuse() => {
                    if *cancellation_rx.borrow() {
                        log::debug!("Compaction cancelled while summarizing");
                        return Ok(ControlFlow::Break(()));
                    }
                    continue;
                }
            };

            let Some(event) = event else {
                break;
            };

            match event? {
                LanguageModelCompletionEvent::Text(text) => {
                    summary.push_str(&text);
                    event_stream.send_context_compaction_update(compaction_id.clone(), &text);
                }
                LanguageModelCompletionEvent::UsageUpdate(usage) => {
                    this.update(cx, |this, _cx| {
                        this.accumulate_token_usage(usage);
                    })?;
                }
                LanguageModelCompletionEvent::Stop(_)
                | LanguageModelCompletionEvent::Started
                | LanguageModelCompletionEvent::Queued { .. }
                | LanguageModelCompletionEvent::Thinking { .. }
                | LanguageModelCompletionEvent::RedactedThinking { .. }
                | LanguageModelCompletionEvent::ReasoningDetails(_)
                | LanguageModelCompletionEvent::ToolUse(_)
                | LanguageModelCompletionEvent::ToolUseJsonParseError { .. }
                | LanguageModelCompletionEvent::StartMessage { .. }
                | LanguageModelCompletionEvent::Compaction(_) => {}
            }
        }

        if *cancellation_rx.borrow() {
            log::debug!("Compaction cancelled after summarizing");
            return Ok(ControlFlow::Break(()));
        }

        let summary = summary.trim().to_string();
        if summary.is_empty() {
            log::warn!("Compaction produced an empty summary");
            return Err(anyhow::anyhow!("Compaction produced an empty summary"));
        }

        log::debug!("Compaction succeeded:\n{summary}");
        event_stream.update_context_compaction_status(
            compaction_id,
            acp_thread::ContextCompactionStatus::Completed,
        );

        this.update(cx, |this, cx| {
            let compaction = Arc::new(Message::Compaction(CompactionInfo::Summary(summary.into())));
            match insertion {
                CompactionInsertion::Auto { insertion_ix } => {
                    if insertion_ix <= this.messages.len() {
                        this.messages.insert(insertion_ix, compaction);
                    } else {
                        this.messages.push(compaction);
                    }
                }
                CompactionInsertion::Manual { marker_id } => {
                    this.messages.push(Arc::new(Message::User(UserMessage {
                        id: marker_id,
                        content: Arc::from([]),
                    })));
                    this.messages.push(compaction);
                }
            }
            cx.notify();
        })?;

        Ok(ControlFlow::Continue(()))
    }

    fn process_tool_result(
        this: &WeakEntity<Thread>,
        event_stream: &ThreadEventStream,
        cx: &mut AsyncApp,
        owning_message_ix: usize,
        tool_result: LanguageModelToolResult,
    ) -> Result<(), anyhow::Error> {
        log::debug!("Tool finished {:?}", tool_result);

        event_stream.update_tool_call_fields(
            &scoped_tool_call_id(owning_message_ix, &tool_result.tool_use_id),
            acp::ToolCallUpdateFields::new()
                .status(if tool_result.is_error {
                    acp::ToolCallStatus::Failed
                } else {
                    acp::ToolCallStatus::Completed
                })
                .raw_output(tool_result.output.clone()),
            None,
        );
        this.update(cx, |this, _cx| {
            this.pending_message()
                .tool_results
                .insert(tool_result.tool_use_id.clone(), tool_result)
        })?;
        Ok(())
    }

    fn handle_completion_error(
        &mut self,
        error: LanguageModelCompletionError,
        attempt: u8,
        plan: Option<Plan>,
        cx: &mut Context<Self>,
    ) -> Result<acp_thread::RetryStatus> {
        if let LanguageModelCompletionError::PromptTooLarge { tokens } = &error {
            self.mark_token_limit_exceeded(*tokens, cx);
        }

        let Some(model) = self.model() else {
            return Err(anyhow!(error));
        };

        let auto_retry = if model.provider_id() == ZED_CLOUD_PROVIDER_ID {
            plan.is_some()
        } else {
            true
        };

        if !auto_retry {
            return Err(anyhow!(error));
        }

        let Some(strategy) = Self::retry_strategy_for(&error) else {
            return Err(anyhow!(error));
        };

        let max_attempts = match &strategy {
            RetryStrategy::ExponentialBackoff { max_attempts, .. } => *max_attempts,
            RetryStrategy::Fixed { max_attempts, .. } => *max_attempts,
        };

        if attempt > max_attempts {
            return Err(anyhow!(error));
        }

        let delay = match &strategy {
            RetryStrategy::ExponentialBackoff { initial_delay, .. } => {
                let delay_secs = initial_delay.as_secs() * 2u64.pow((attempt - 1) as u32);
                Duration::from_secs(delay_secs)
            }
            RetryStrategy::Fixed { delay, .. } => *delay,
        };
        log::debug!("Retry attempt {attempt} with delay {delay:?}");

        Ok(acp_thread::RetryStatus {
            last_error: error.to_string().into(),
            attempt: attempt as usize,
            max_attempts: max_attempts as usize,
            started_at: Instant::now(),
            duration: delay,
            meta: None,
        })
    }

    /// A helper method that's called on every streamed completion event.
    /// Returns an optional tool result task, which the main agentic loop will
    /// send back to the model when it resolves.
    fn handle_completion_event(
        &mut self,
        event: LanguageModelCompletionEvent,
        event_stream: &ThreadEventStream,
        cancellation_rx: watch::Receiver<bool>,
        cx: &mut Context<Self>,
    ) -> Result<Option<Task<(usize, LanguageModelToolResult)>>> {
        log::trace!("Handling streamed completion event: {:?}", event);
        use LanguageModelCompletionEvent::*;

        match event {
            StartMessage { .. } => {
                self.flush_pending_message(cx);
                self.pending_message = Some(AgentMessage::default());
            }
            Text(new_text) => self.handle_text_event(new_text, event_stream),
            Thinking { text, signature } => {
                self.handle_thinking_event(text, signature, event_stream)
            }
            RedactedThinking { data } => self.handle_redacted_thinking_event(data),
            ReasoningDetails(details) => {
                let last_message = self.pending_message();
                // Store the last non-empty reasoning_details (overwrites earlier ones)
                // This ensures we keep the encrypted reasoning with signatures, not the early text reasoning
                if let serde_json::Value::Array(arr) = &details {
                    if !arr.is_empty() {
                        last_message.reasoning_details = Some(Arc::new(details));
                    }
                } else {
                    last_message.reasoning_details = Some(Arc::new(details));
                }
            }
            ToolUse(tool_use) => {
                return Ok(self.handle_tool_use_event(tool_use, event_stream, cancellation_rx, cx));
            }
            ToolUseJsonParseError {
                id,
                tool_name,
                raw_input,
                json_parse_error,
            } => {
                return Ok(self.handle_tool_use_json_parse_error_event(
                    id,
                    tool_name,
                    raw_input,
                    json_parse_error,
                    event_stream,
                    cancellation_rx,
                    cx,
                ));
            }
            UsageUpdate(usage) => {
                telemetry::event!(
                    "Agent Thread Completion Usage Updated",
                    thread_id = self.id.to_string(),
                    parent_thread_id = self.parent_thread_id().map(|id| id.to_string()),
                    prompt_id = self.prompt_id.to_string(),
                    model = self.model().map(|m| m.telemetry_id()),
                    model_provider = self.model().map(|m| m.provider_id().to_string()),
                    input_tokens = usage.input_tokens,
                    output_tokens = usage.output_tokens,
                    cache_creation_input_tokens = usage.cache_creation_input_tokens,
                    cache_read_input_tokens = usage.cache_read_input_tokens,
                );
                // A successful compaction defers its telemetry until the first
                // completion that follows it, so `tokens_after` reflects the
                // real post-compaction context size.
                if let Some(telemetry) = self.pending_compaction_telemetry.take() {
                    telemetry.emit("succeeded", None, Some(total_input_tokens(usage)));
                }
                self.update_token_usage(usage, cx);
            }
            Stop(StopReason::Refusal) => return Err(CompletionError::Refusal.into()),
            Stop(StopReason::MaxTokens) => return Err(CompletionError::MaxTokens.into()),
            Stop(StopReason::ToolUse | StopReason::EndTurn) => {}
            Started | Queued { .. } | Compaction(_) => {}
        }

        Ok(None)
    }

    fn handle_text_event(&mut self, new_text: String, event_stream: &ThreadEventStream) {
        event_stream.send_text(&new_text);

        let last_message = self.pending_message();
        if let Some(AgentMessageContent::Text(text)) = last_message.content.last_mut() {
            text.push_str(&new_text);
        } else {
            last_message
                .content
                .push(AgentMessageContent::Text(new_text));
        }
    }

    fn handle_thinking_event(
        &mut self,
        new_text: String,
        new_signature: Option<String>,
        event_stream: &ThreadEventStream,
    ) {
        event_stream.send_thinking(&new_text);

        let last_message = self.pending_message();
        if let Some(AgentMessageContent::Thinking { text, signature }) =
            last_message.content.last_mut()
        {
            text.push_str(&new_text);
            *signature = new_signature.or(signature.take());
        } else {
            last_message.content.push(AgentMessageContent::Thinking {
                text: new_text,
                signature: new_signature,
            });
        }
    }

    fn handle_redacted_thinking_event(&mut self, data: String) {
        let last_message = self.pending_message();
        last_message
            .content
            .push(AgentMessageContent::RedactedThinking(data));
    }

    fn handle_tool_use_event(
        &mut self,
        tool_use: LanguageModelToolUse,
        event_stream: &ThreadEventStream,
        cancellation_rx: watch::Receiver<bool>,
        cx: &mut Context<Self>,
    ) -> Option<Task<(usize, LanguageModelToolResult)>> {
        cx.notify();

        let owning_message_ix = self.messages.len();
        let tool = self.tool(tool_use.name.as_ref());
        let mut title = SharedString::from(&tool_use.name);
        let mut kind = acp::ToolKind::Other;
        if let Some(tool) = tool.as_ref() {
            if let Ok(input) = tool_use.input.clone().into_json() {
                title = tool.initial_title(input, cx);
            }
            kind = tool.kind();
        }

        self.send_or_update_tool_use(&tool_use, title, kind, owning_message_ix, event_stream);

        let Some(tool) = tool else {
            let content = format!("No tool named {} exists", tool_use.name);
            return Some(Task::ready((
                owning_message_ix,
                LanguageModelToolResult {
                    content: vec![LanguageModelToolResultContent::Text(Arc::from(content))],
                    tool_use_id: tool_use.id,
                    tool_name: tool_use.name,
                    is_error: true,
                    output: None,
                },
            )));
        };

        // Agent tools are JSON-schema tools. Custom text-tool deltas are rejected
        // before considering partial-vs-complete input for these local tools.
        let input = match tool_use.input.clone().into_json() {
            Ok(input) => input,
            Err(error) => {
                return Some(Task::ready((
                    owning_message_ix,
                    LanguageModelToolResult {
                        content: vec![LanguageModelToolResultContent::Text(Arc::from(
                            error.to_string(),
                        ))],
                        tool_use_id: tool_use.id,
                        tool_name: tool_use.name,
                        is_error: true,
                        output: None,
                    },
                )));
            }
        };

        if !tool_use.is_input_complete {
            if tool.supports_input_streaming() {
                let running_turn = self.running_turn.as_mut()?;
                if let Some(sender) = running_turn.streaming_tool_inputs.get_mut(&tool_use.id) {
                    sender.send_partial(input);
                    return None;
                }

                let (mut sender, tool_input) = ToolInputSender::channel();
                sender.send_partial(input);
                running_turn
                    .streaming_tool_inputs
                    .insert(tool_use.id.clone(), sender);

                let tool = tool.clone();
                log::debug!("Running streaming tool {}", tool_use.name);
                return Some(self.run_tool(
                    tool,
                    tool_input,
                    tool_use.id,
                    tool_use.name,
                    owning_message_ix,
                    event_stream,
                    cancellation_rx,
                    cx,
                ));
            } else {
                return None;
            }
        }

        if let Some(mut sender) = self
            .running_turn
            .as_mut()?
            .streaming_tool_inputs
            .remove(&tool_use.id)
        {
            sender.send_full(input);
            return None;
        }

        log::debug!("Running tool {}", tool_use.name);
        let tool_input = ToolInput::ready(input);
        Some(self.run_tool(
            tool,
            tool_input,
            tool_use.id,
            tool_use.name,
            owning_message_ix,
            event_stream,
            cancellation_rx,
            cx,
        ))
    }

    fn run_tool(
        &self,
        tool: Arc<dyn AnyAgentTool>,
        tool_input: ToolInput<serde_json::Value>,
        tool_use_id: LanguageModelToolUseId,
        tool_name: Arc<str>,
        owning_message_ix: usize,
        event_stream: &ThreadEventStream,
        cancellation_rx: watch::Receiver<bool>,
        cx: &mut Context<Self>,
    ) -> Task<(usize, LanguageModelToolResult)> {
        // A workspace can become restricted after a thread has already started.
        // Tools that aren't allowed in restricted workspaces must never run in
        // that state, even though they were exposed to the model earlier.
        if !tool.allow_in_restricted_mode()
            && TrustedWorktrees::has_restricted_worktrees(
                &self.project.read(cx).worktree_store(),
                cx,
            )
        {
            return Task::ready((
                owning_message_ix,
                LanguageModelToolResult {
                    tool_use_id,
                    tool_name,
                    is_error: true,
                    content: vec![LanguageModelToolResultContent::Text(Arc::from(
                        "workspace has become restricted",
                    ))],
                    output: None,
                },
            ));
        }

        let fs = self.project.read(cx).fs().clone();
        let tool_call_id = scoped_tool_call_id(owning_message_ix, &tool_use_id);
        let tool_event_stream = ToolCallEventStream::new(
            tool_use_id.clone(),
            tool_call_id,
            event_stream.clone(),
            Some(fs),
            cancellation_rx,
            self.sandbox_grants.clone(),
            self.permission_judge.clone(),
            Some(cx.weak_entity()),
        );
        tool_event_stream.update_fields(
            acp::ToolCallUpdateFields::new().status(acp::ToolCallStatus::InProgress),
        );
        let supports_images = self.model().is_some_and(|model| model.supports_images());
        let tool_result = tool.run(tool_input, tool_event_stream, cx);
        cx.foreground_executor().spawn(async move {
            let (is_error, output) = match tool_result.await {
                Ok(mut output) => {
                    let contains_image = output
                        .llm_output
                        .iter()
                        .any(|part| matches!(part, LanguageModelToolResultContent::Image(_)));
                    if contains_image && !supports_images {
                        // Replace each image part with an inline placeholder so
                        // any accompanying text is still presented to the model.
                        // If there's nothing else in the output, surface an error
                        // to match the pre-multi-part behavior for image-only
                        // tool results.
                        let placeholder = LanguageModelToolResultContent::Text(Arc::from(
                            "[Tool responded with an image, but this model doesn't support images]",
                        ));
                        let has_non_image = output
                            .llm_output
                            .iter()
                            .any(|part| !matches!(part, LanguageModelToolResultContent::Image(_)));
                        if has_non_image {
                            output.llm_output = output
                                .llm_output
                                .into_iter()
                                .map(|part| match part {
                                    LanguageModelToolResultContent::Image(_) => placeholder.clone(),
                                    other => other,
                                })
                                .collect();
                            (false, output)
                        } else {
                            let output = anyhow::anyhow!(
                                "Attempted to read an image, but this model doesn't support it.",
                            )
                            .into();
                            (true, output)
                        }
                    } else {
                        (false, output)
                    }
                }
                Err(output) => (true, output),
            };

            (
                owning_message_ix,
                LanguageModelToolResult {
                    tool_use_id,
                    tool_name,
                    is_error,
                    content: output.llm_output,
                    output: Some(output.raw_output),
                },
            )
        })
    }

    fn handle_tool_use_json_parse_error_event(
        &mut self,
        tool_use_id: LanguageModelToolUseId,
        tool_name: Arc<str>,
        raw_input: Arc<str>,
        json_parse_error: String,
        event_stream: &ThreadEventStream,
        cancellation_rx: watch::Receiver<bool>,
        cx: &mut Context<Self>,
    ) -> Option<Task<(usize, LanguageModelToolResult)>> {
        let owning_message_ix = self.messages.len();
        let tool_use = LanguageModelToolUse {
            id: tool_use_id,
            name: tool_name,
            raw_input: raw_input.to_string(),
            input: language_model::LanguageModelToolUseInput::Json(serde_json::json!({})),
            is_input_complete: true,
            thought_signature: None,
        };
        self.send_or_update_tool_use(
            &tool_use,
            SharedString::from(&tool_use.name),
            acp::ToolKind::Other,
            owning_message_ix,
            event_stream,
        );

        let tool = self.tool(tool_use.name.as_ref());

        let Some(tool) = tool else {
            let content = format!("No tool named {} exists", tool_use.name);
            return Some(Task::ready((
                owning_message_ix,
                LanguageModelToolResult {
                    content: vec![LanguageModelToolResultContent::Text(Arc::from(content))],
                    tool_use_id: tool_use.id,
                    tool_name: tool_use.name,
                    is_error: true,
                    output: None,
                },
            )));
        };

        let error_message = format!("Error parsing input JSON: {json_parse_error}");

        if tool.supports_input_streaming()
            && let Some(mut sender) = self
                .running_turn
                .as_mut()?
                .streaming_tool_inputs
                .remove(&tool_use.id)
        {
            sender.send_invalid_json(error_message);
            return None;
        }

        log::debug!("Running tool {}. Received invalid JSON", tool_use.name);
        let tool_input = ToolInput::invalid_json(error_message);
        Some(self.run_tool(
            tool,
            tool_input,
            tool_use.id,
            tool_use.name,
            owning_message_ix,
            event_stream,
            cancellation_rx,
            cx,
        ))
    }

    fn send_or_update_tool_use(
        &mut self,
        tool_use: &LanguageModelToolUse,
        title: SharedString,
        kind: acp::ToolKind,
        owning_message_ix: usize,
        event_stream: &ThreadEventStream,
    ) {
        let tool_call_id = scoped_tool_call_id(owning_message_ix, &tool_use.id);

        // Ensure the last message ends in the current tool use
        let last_message = self.pending_message();

        let has_tool_use = last_message.content.iter_mut().rev().any(|content| {
            if let AgentMessageContent::ToolUse(last_tool_use) = content {
                if last_tool_use.id == tool_use.id {
                    *last_tool_use = tool_use.clone();
                    return true;
                }
            }
            false
        });

        if !has_tool_use {
            event_stream.send_tool_call(
                &tool_call_id,
                &tool_use.name,
                title,
                kind,
                tool_use.input.to_display_json(),
            );
            last_message
                .content
                .push(AgentMessageContent::ToolUse(tool_use.clone()));
        } else {
            event_stream.update_tool_call_fields(
                &tool_call_id,
                acp::ToolCallUpdateFields::new()
                    .title(title.as_str())
                    .kind(kind)
                    .raw_input(tool_use.input.to_display_json()),
                None,
            );
        }
    }

    pub fn title(&self) -> Option<SharedString> {
        self.title.clone()
    }

    pub fn is_generating_summary(&self) -> bool {
        self.pending_summary_generation.is_some()
    }

    pub fn is_generating_title(&self) -> bool {
        self.pending_title_generation.is_some()
    }

    pub fn has_failed_title_generation(&self) -> bool {
        self.title_generation_error.is_some()
    }

    pub fn title_generation_error(&self) -> Option<SharedString> {
        self.title_generation_error.clone()
    }

    pub fn can_generate_title(&self) -> bool {
        self.pending_title_generation.is_none() && self.summarization_model.is_some()
    }

    pub fn summary(&mut self, cx: &mut Context<Self>) -> Shared<Task<Option<SharedString>>> {
        if let Some(summary) = self.summary.as_ref() {
            return Task::ready(Some(summary.clone())).shared();
        }
        if let Some(task) = self.pending_summary_generation.clone() {
            return task;
        }
        let Some(model) = self.summarization_model.clone() else {
            log::error!("No summarization model available");
            return Task::ready(None).shared();
        };
        let mut request = LanguageModelRequest {
            intent: Some(CompletionIntent::ThreadContextSummarization),
            temperature: AgentSettings::temperature_for_model(&model, cx),
            ..Default::default()
        };

        self.extend_request_history_until(&mut request.messages, self.messages.len());

        request.messages.push(LanguageModelRequestMessage {
            role: Role::User,
            content: vec![SUMMARIZE_THREAD_DETAILED_PROMPT.into()],
            cache: false,
            reasoning_details: None,
        });

        let task = cx
            .spawn(async move |this, cx| {
                let mut summary = String::new();
                let mut messages = model.stream_completion(request, cx).await.log_err()?;
                while let Some(event) = messages.next().await {
                    let event = event.log_err()?;
                    let text = match event {
                        LanguageModelCompletionEvent::Text(text) => text,
                        _ => continue,
                    };

                    let mut lines = text.lines();
                    summary.extend(lines.next());
                }

                log::debug!("Setting summary: {}", summary);
                let summary = SharedString::from(summary);

                this.update(cx, |this, cx| {
                    this.summary = Some(summary.clone());
                    this.pending_summary_generation = None;
                    cx.notify()
                })
                .ok()?;

                Some(summary)
            })
            .shared();
        self.pending_summary_generation = Some(task.clone());
        task
    }

    pub fn generate_title(&mut self, cx: &mut Context<Self>) {
        if !self.can_generate_title() {
            return;
        }
        let Some(model) = self.summarization_model.clone() else {
            return;
        };
        self.spawn_title_generation(model, None, cx);
    }

    pub fn regenerate_title(&mut self, cx: &mut Context<Self>) -> bool {
        self.regenerate_title_with_callback(cx, |_title, _cx| {})
    }

    pub fn regenerate_title_with_callback(
        &mut self,
        cx: &mut Context<Self>,
        on_generated_title: impl FnOnce(SharedString, &mut Context<Self>) + 'static,
    ) -> bool {
        if self.pending_title_generation.is_some() {
            return false;
        }

        let Some(model) = self.summarization_model.clone() else {
            return false;
        };

        self.spawn_title_generation(model, Some(Box::new(on_generated_title)), cx);

        true
    }

    fn spawn_title_generation(
        &mut self,
        model: Arc<dyn LanguageModel>,
        on_generated_title: Option<Box<dyn FnOnce(SharedString, &mut Context<Self>)>>,
        cx: &mut Context<Self>,
    ) {
        self.title_generation_error = None;
        log::debug!("Generating title with model: {:?}", model.name());

        let temperature = AgentSettings::temperature_for_model(&model, cx);
        let request = build_thread_title_request(&self.messages, temperature);

        let title_generation = cx.spawn(async move |_this, cx| {
            stream_thread_title(model, request, cx)
                .await
                .context("failed to generate thread title")
                .map(SharedString::from)
        });

        self.pending_title_generation = Some(cx.spawn(async move |this, cx| {
            let title = title_generation.await;
            _ = this.update(cx, |this, cx| {
                this.pending_title_generation = None;
                match title {
                    Ok(title) => {
                        this.set_title(title.clone(), cx);
                        if let Some(on_generated_title) = on_generated_title {
                            on_generated_title(title, cx);
                        }
                    }
                    Err(error) => {
                        let error = format!("{error:#}");
                        log::error!("{error}");
                        this.title_generation_error = Some(error.into());
                        cx.emit(TitleUpdated);
                        cx.notify();
                    }
                }
            });
        }));
        cx.notify();
    }

    pub fn set_title(&mut self, title: SharedString, cx: &mut Context<Self>) {
        self.pending_title_generation = None;
        self.title_generation_error = None;
        if Some(&title) != self.title.as_ref() {
            self.title = Some(title);
            cx.emit(TitleUpdated);
            cx.notify();
        }
    }

    fn clear_summary(&mut self) {
        self.summary = None;
        self.pending_summary_generation = None;
    }

    fn last_user_message(&self) -> Option<&UserMessage> {
        self.messages
            .iter()
            .rev()
            .find_map(|message| match &**message {
                Message::User(user_message) => Some(user_message),
                Message::Agent(_) | Message::Resume | Message::Compaction(_) => None,
            })
    }

    fn pending_message(&mut self) -> &mut AgentMessage {
        self.pending_message.get_or_insert_default()
    }

    fn flush_pending_message(&mut self, cx: &mut Context<Self>) {
        let Some(mut message) = self.pending_message.take() else {
            return;
        };

        if message.content.is_empty() {
            return;
        }

        for content in &message.content {
            let AgentMessageContent::ToolUse(tool_use) = content else {
                continue;
            };

            if !message.tool_results.contains_key(&tool_use.id) {
                message.tool_results.insert(
                    tool_use.id.clone(),
                    LanguageModelToolResult {
                        tool_use_id: tool_use.id.clone(),
                        tool_name: tool_use.name.clone(),
                        is_error: true,
                        content: vec![LanguageModelToolResultContent::Text(
                            TOOL_CANCELED_MESSAGE.into(),
                        )],
                        output: None,
                    },
                );
            }
        }

        self.messages.push(Arc::new(Message::Agent(message)));
        self.updated_at = Utc::now();
        self.clear_summary();
        cx.notify()
    }

    pub(crate) fn build_completion_request(
        &self,
        completion_intent: CompletionIntent,
        cx: &App,
    ) -> Result<LanguageModelRequest> {
        let completion_intent =
            if self.is_subagent() && completion_intent == CompletionIntent::UserPrompt {
                CompletionIntent::Subagent
            } else {
                completion_intent
            };

        let model = self
            .model()
            .ok_or_else(|| anyhow!(NoModelConfiguredError))?;
        let sandboxing_enabled = crate::sandboxing::sandboxing_enabled(cx);
        let tools = if let Some(turn) = self.running_turn.as_ref() {
            turn.tools
                .iter()
                .filter_map(|(tool_name, tool)| {
                    log::trace!("Including tool: {}", tool_name);
                    let mut description = tool.description().to_string();
                    let mut schema = tool.input_schema(model.tool_input_format()).log_err()?;
                    // TEMPORARY (sandboxing feature flag): with the flag off,
                    // the fetch and create_directory descriptions/schemas must
                    // not advertise sandbox-dependent behavior (host grants,
                    // out-of-project creation via the `reason` field), since
                    // the corresponding runtime paths are disabled. Restore
                    // the pre-sandboxing model-facing surface here rather than
                    // forking the tools; delete this when the flag is removed
                    // again.
                    if !sandboxing_enabled {
                        if tool_name.as_ref() == FetchTool::NAME {
                            description =
                                "Fetches a URL and returns the content as Markdown.".to_string();
                        } else if tool_name.as_ref() == CreateDirectoryTool::NAME {
                            description = "Creates a new directory at the specified path within \
                                the project. Returns confirmation that the directory was \
                                created.\n\nThis tool creates a directory and all necessary \
                                parent directories. It should be used whenever you need to \
                                create new directories within the project.\nThe only supported \
                                path outside the project is `~/.agents/skills` or a descendant, \
                                for global agent skills."
                                .to_string();
                            if let Some(properties) = schema
                                .get_mut("properties")
                                .and_then(|value| value.as_object_mut())
                            {
                                properties.remove("reason");
                            }
                        }
                    }
                    Some(LanguageModelRequestTool::function(
                        tool_name.to_string(),
                        description,
                        schema,
                        tool.supports_input_streaming(),
                    ))
                })
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };

        log::debug!("Building completion request");
        log::debug!("Completion intent: {:?}", completion_intent);

        let available_tools: Vec<_> = self
            .running_turn
            .as_ref()
            .map(|turn| turn.tools.keys().cloned().collect())
            .unwrap_or_default();

        log::debug!("Request includes {} tools", available_tools.len());
        let messages = self.build_request_messages(available_tools, cx);
        log::debug!("Request will include {} messages", messages.len());

        let request = LanguageModelRequest {
            thread_id: Some(self.id.to_string()),
            prompt_id: Some(self.prompt_id.to_string()),
            intent: Some(completion_intent),
            messages,
            tools,
            tool_choice: None,
            stop: Vec::new(),
            temperature: AgentSettings::temperature_for_model(model, cx),
            // Models that can't run with thinking disabled ignore the
            // toggle state, which may be stale from a previously selected
            // model that could.
            thinking_allowed: self.thinking_enabled || !model.supports_disabling_thinking(),
            thinking_effort: self.thinking_effort.clone(),
            speed: self.speed(),
            compact_at_tokens: None,
        };

        log::debug!("Completion request built successfully");
        Ok(request)
    }

    fn enabled_tools(&self, cx: &App) -> BTreeMap<SharedString, Arc<dyn AnyAgentTool>> {
        let Some(model) = self.model() else {
            return BTreeMap::new();
        };
        let Some(profile) = AgentSettings::get_global(cx)
            .profiles
            .get(&self.profile(cx))
        else {
            return BTreeMap::new();
        };
        // Terminal variants are configured by users under the canonical
        // `terminal` name. Expose the one matching the current sandbox state
        // to the model under that name.
        let use_sandboxed_terminal = sandboxing_enabled_for_project(self.project.read(cx), cx);

        // Tools that aren't allowed in restricted workspaces must never be
        // provided to the model while the workspace is restricted, regardless
        // of what the active profile enables.
        let is_restricted =
            TrustedWorktrees::has_restricted_worktrees(&self.project.read(cx).worktree_store(), cx);

        let mut tools = self
            .tools
            .iter()
            .filter(|(_, tool)| !is_restricted || tool.allow_in_restricted_mode())
            .filter_map(|(tool_name, tool)| {
                let terminal_variant = matches!(
                    tool_name.as_ref(),
                    TerminalTool::NAME | SandboxedTerminalTool::NAME
                );
                let profile_tool_name = if terminal_variant {
                    TerminalTool::NAME
                } else {
                    tool_name.as_ref()
                };

                if tool.supports_provider(&model.provider_id())
                    && profile.is_tool_enabled(profile_tool_name)
                {
                    match (tool_name.as_ref(), use_sandboxed_terminal) {
                        (TerminalTool::NAME, false) | (SandboxedTerminalTool::NAME, true) => {
                            Some((SharedString::from(TerminalTool::NAME), tool.clone()))
                        }
                        (TerminalTool::NAME | SandboxedTerminalTool::NAME, _) => None,
                        _ => Some((
                            provider_compatible_tool_name(tool_name.as_ref()).into(),
                            tool.clone(),
                        )),
                    }
                } else {
                    None
                }
            })
            .filter(|(tool_name, _)| crate::tools::tool_feature_flag_enabled(tool_name, cx))
            .collect::<BTreeMap<_, _>>();

        let mut context_server_tools = Vec::new();
        let mut seen_tools = tools.keys().cloned().collect::<HashSet<_>>();
        let mut duplicate_tool_names = HashSet::default();
        for (server_id, server_tools) in self.context_server_registry.read(cx).servers() {
            for (tool_name, tool) in server_tools {
                if profile.is_context_server_tool_enabled(&server_id.0, &tool_name) {
                    let tool_name: SharedString =
                        provider_compatible_tool_name(tool_name.as_ref()).into();
                    if !seen_tools.insert(tool_name.clone()) {
                        duplicate_tool_names.insert(tool_name.clone());
                    }
                    context_server_tools.push((server_id.clone(), tool_name, tool.clone()));
                }
            }
        }

        // When there are duplicate tool names, disambiguate by prefixing them
        // with the server ID (converted to snake_case for API compatibility).
        // In the rare case there isn't enough space for the disambiguated tool
        // name, keep only the last tool with this name.
        for (server_id, tool_name, tool) in context_server_tools {
            if duplicate_tool_names.contains(&tool_name) {
                let available = MAX_TOOL_NAME_LENGTH.saturating_sub(tool_name.len());
                if available >= 2 {
                    let mut disambiguated =
                        provider_compatible_tool_name(&server_id.0.to_snake_case()).to_string();
                    disambiguated.truncate(available - 1);
                    disambiguated.push('_');
                    disambiguated.push_str(&tool_name);
                    tools.insert(disambiguated.into(), tool.clone());
                } else {
                    tools.insert(tool_name, tool.clone());
                }
            } else {
                tools.insert(tool_name, tool.clone());
            }
        }

        tools
    }

    fn refresh_turn_tools(&mut self, cx: &App) {
        let tools = self.enabled_tools(cx);
        if let Some(turn) = self.running_turn.as_mut() {
            turn.tools = tools;
        }
    }

    fn tool(&self, name: &str) -> Option<Arc<dyn AnyAgentTool>> {
        self.running_turn.as_ref()?.tools.get(name).cloned()
    }

    pub fn has_tool(&self, name: &str) -> bool {
        self.running_turn
            .as_ref()
            .is_some_and(|turn| turn.tools.contains_key(name))
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn has_registered_tool(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn registered_tool_kinds(&self) -> Vec<(SharedString, acp::ToolKind)> {
        self.tools
            .iter()
            .map(|(name, tool)| (name.clone(), tool.kind()))
            .collect()
    }

    pub(crate) fn register_running_subagent(&mut self, subagent: WeakEntity<Thread>) {
        self.running_subagents.push(subagent);
    }

    pub(crate) fn unregister_running_subagent(
        &mut self,
        subagent_session_id: &acp::SessionId,
        cx: &App,
    ) {
        self.running_subagents.retain(|s| {
            s.upgrade()
                .map_or(false, |s| s.read(cx).id() != subagent_session_id)
        });
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn running_subagent_ids(&self, cx: &App) -> Vec<acp::SessionId> {
        self.running_subagents
            .iter()
            .filter_map(|s| s.upgrade().map(|s| s.read(cx).id().clone()))
            .collect()
    }

    pub fn is_subagent(&self) -> bool {
        self.subagent_context.is_some()
    }

    pub fn parent_thread_id(&self) -> Option<acp::SessionId> {
        self.subagent_context
            .as_ref()
            .map(|c| c.parent_thread_id.clone())
    }

    pub fn depth(&self) -> u8 {
        self.subagent_context.as_ref().map(|c| c.depth).unwrap_or(0)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn set_subagent_context(&mut self, context: SubagentContext) {
        self.subagent_context = Some(context);
    }

    pub fn is_turn_complete(&self) -> bool {
        self.running_turn.is_none()
    }

    fn build_request_messages(
        &self,
        available_tools: Vec<SharedString>,
        cx: &App,
    ) -> Vec<LanguageModelRequestMessage> {
        let mut messages =
            self.build_request_messages_until(available_tools, self.messages.len(), cx);

        if let Some(message) = self.pending_message.as_ref() {
            messages.extend(message.to_request());
        }

        messages
    }

    fn build_request_messages_until(
        &self,
        available_tools: Vec<SharedString>,
        end_ix: usize,
        cx: &App,
    ) -> Vec<LanguageModelRequestMessage> {
        let end_ix = end_ix.min(self.messages.len());
        log::trace!("Building request messages from {} thread messages", end_ix);

        let user_agents_md = UserAgentsMd::global(cx).and_then(|s| s.content().cloned());
        let system_prompt = SystemPromptTemplate {
            project: self.project_context.read(cx),
            available_tools,
            model_name: self.model().map(|m| m.name().0.to_string()),
            date: Local::now().format("%Y-%m-%d").to_string(),
            user_agents_md,
            sandboxing: crate::sandboxing::sandboxing_enabled_for_project(
                self.project.read(cx),
                cx,
            ),
            is_linux: cfg!(target_os = "linux"),
            is_windows: cfg!(target_os = "windows"),
            restricted_workspace: TrustedWorktrees::has_restricted_worktrees(
                &self.project.read(cx).worktree_store(),
                cx,
            ),
            repo_map: self
                .system_prompt_repo_map
                .as_ref()
                .map(|(_key, map)| map.clone()),
        }
        .render(&self.templates)
        .context("failed to build system prompt")
        .expect("Invalid template");
        let mut messages = vec![LanguageModelRequestMessage {
            role: Role::System,
            content: vec![system_prompt.into()],
            cache: false,
            reasoning_details: None,
        }];
        self.extend_request_history_until(&mut messages, end_ix);

        if let Some(last_message) = messages.last_mut() {
            last_message.cache = true;
        }

        messages
    }

    fn extend_request_history_until(
        &self,
        request_messages: &mut Vec<LanguageModelRequestMessage>,
        end_ix: usize,
    ) {
        extend_request_history_until(&self.messages, request_messages, end_ix);
    }

    fn build_compaction_telemetry(
        &self,
        trigger: &'static str,
        compaction_model: &Arc<dyn LanguageModel>,
        cx: &App,
    ) -> Option<CompactionTelemetry> {
        let model = self.model()?;
        let auto_compact = AgentSettings::get_global(cx).auto_compact;
        let max_tokens = model.max_token_count();
        let max_input_tokens = max_tokens.saturating_sub(model.max_output_tokens().unwrap_or(0));
        let tokens_before = self
            .latest_request_token_usage()
            .map(|usage| total_input_tokens(usage).saturating_add(usage.output_tokens));
        Some(CompactionTelemetry {
            trigger,
            thread_id: self.id.to_string(),
            parent_thread_id: self.parent_thread_id().map(|id| id.to_string()),
            prompt_id: self.prompt_id.to_string(),
            model: model.telemetry_id(),
            compaction_model: compaction_model.telemetry_id(),
            thinking_effort: self.thinking_effort.clone(),
            max_tokens,
            tokens_before,
            auto_compact_enabled: auto_compact.enabled,
            auto_compact_threshold: auto_compact.threshold.to_string(),
            auto_compact_threshold_tokens: auto_compact_threshold_token_count(
                auto_compact.threshold,
                max_input_tokens,
            ),
            retries: 0,
        })
    }

    /// Emits a pending compaction telemetry event for a non-success outcome
    /// (`"failed"` or `"canceled"`), with no post-compaction token count. A
    /// no-op if no compaction telemetry is pending.
    fn emit_compaction_telemetry_outcome(&mut self, status: &'static str, error: Option<String>) {
        if let Some(telemetry) = self.pending_compaction_telemetry.take() {
            telemetry.emit(status, error, None);
        }
    }

    fn compaction_message_target_ix(&self, cx: &App) -> Option<usize> {
        let auto_compact = AgentSettings::get_global(cx).auto_compact;
        if !auto_compact.enabled {
            return None;
        }

        let model = self.model()?;
        let max_token_count = model.max_token_count();
        let max_input_tokens =
            max_token_count.saturating_sub(model.max_output_tokens().unwrap_or(0));
        // Models with a small context window don't leave enough headroom for a
        // compaction pass; the UI warns the user about the token limit instead.
        if max_input_tokens < min_compaction_context_window(cx) {
            return None;
        }
        let (usage_ix, usage) = {
            let this = &self;
            this.messages
                .iter()
                .enumerate()
                .rev()
                .find_map(|(ix, message)| {
                    let Message::User(user_message) = &**message else {
                        return None;
                    };
                    this.request_token_usage
                        .get(&user_message.id)
                        .copied()
                        .map(|usage| (ix, usage))
                })
        }?;
        if latest_compaction_message_ix_before(&self.messages, self.messages.len())
            .is_some_and(|compaction_ix| compaction_ix > usage_ix)
        {
            return None;
        }

        let active_tokens = total_input_tokens(usage).saturating_add(usage.output_tokens);
        let compaction_threshold =
            auto_compact_threshold_token_count(auto_compact.threshold, max_input_tokens);
        if active_tokens < compaction_threshold {
            return None;
        }

        let insertion_ix = match self.messages.last() {
            Some(message)
                if matches!(
                    &**message,
                    Message::User(UserMessage { id, .. }) if !self.request_token_usage.contains_key(id)
                ) =>
            {
                self.messages.len().saturating_sub(1)
            }
            _ => self.messages.len(),
        };
        Some(insertion_ix)
    }

    /// Insertion point for a manually-triggered compaction.
    /// Returns `None` only when there is nothing to summarize (no messages, or the thread already ends in a compaction).
    fn forced_compaction_target_ix(&self) -> Option<usize> {
        if matches!(
            self.messages.last().map(|message| &**message),
            None | Some(Message::Compaction(_))
        ) {
            return None;
        }
        Some(self.messages.len())
    }

    fn compaction_model(&self, cx: &App) -> Option<Arc<dyn LanguageModel>> {
        LanguageModelRegistry::read_global(cx)
            .compaction_model()
            .map(|m| m.model)
            .or_else(|| self.model().cloned())
    }

    fn build_compaction_request(
        &self,
        insertion_ix: usize,
        model: &Arc<dyn LanguageModel>,
        cx: &App,
    ) -> LanguageModelRequest {
        let mut request = LanguageModelRequest {
            thread_id: Some(self.id.to_string()),
            prompt_id: Some(self.prompt_id.to_string()),
            intent: Some(CompletionIntent::ThreadContextSummarization),
            temperature: AgentSettings::temperature_for_model(model, cx),
            messages: self.build_request_messages_until(Vec::new(), insertion_ix, cx),
            ..Default::default()
        };

        request.messages.push(LanguageModelRequestMessage {
            role: Role::User,
            content: vec![COMPACTION_PROMPT.into()],
            cache: false,
            reasoning_details: None,
        });

        request
    }

    pub fn to_markdown(&self) -> String {
        let mut markdown = messages_to_markdown(&self.messages);

        if let Some(message) = self.pending_message.as_ref() {
            markdown.push_str("\n## Assistant\n\n");
            markdown.push_str(&message.to_markdown());
        }

        markdown
    }

    fn advance_prompt_id(&mut self) {
        self.prompt_id = PromptId::new();
    }

    fn retry_strategy_for(error: &LanguageModelCompletionError) -> Option<RetryStrategy> {
        use LanguageModelCompletionError::*;
        use http_client::StatusCode;

        // General strategy here:
        // - If retrying won't help (e.g. invalid API key or payload too large), return None so we don't retry at all.
        // - If it's a time-based issue (e.g. server overloaded, rate limit exceeded), retry up to 4 times with exponential backoff.
        // - If it's an issue that *might* be fixed by retrying (e.g. internal server error), retry up to 3 times.
        match error {
            HttpResponseError {
                status_code: StatusCode::TOO_MANY_REQUESTS,
                ..
            } => Some(RetryStrategy::ExponentialBackoff {
                initial_delay: BASE_RETRY_DELAY,
                max_attempts: MAX_RETRY_ATTEMPTS,
            }),
            ServerOverloaded { retry_after, .. } | RateLimitExceeded { retry_after, .. } => {
                Some(RetryStrategy::Fixed {
                    delay: retry_after.unwrap_or(BASE_RETRY_DELAY),
                    max_attempts: MAX_RETRY_ATTEMPTS,
                })
            }
            UpstreamProviderError {
                status,
                retry_after,
                ..
            } => match *status {
                StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE => {
                    Some(RetryStrategy::Fixed {
                        delay: retry_after.unwrap_or(BASE_RETRY_DELAY),
                        max_attempts: MAX_RETRY_ATTEMPTS,
                    })
                }
                StatusCode::INTERNAL_SERVER_ERROR => Some(RetryStrategy::Fixed {
                    delay: retry_after.unwrap_or(BASE_RETRY_DELAY),
                    // Internal Server Error could be anything, retry up to 3 times.
                    max_attempts: 3,
                }),
                status => {
                    // There is no StatusCode variant for the unofficial HTTP 529 ("The service is overloaded"),
                    // but we frequently get them in practice. See https://http.dev/529
                    if status.as_u16() == 529 {
                        Some(RetryStrategy::Fixed {
                            delay: retry_after.unwrap_or(BASE_RETRY_DELAY),
                            max_attempts: MAX_RETRY_ATTEMPTS,
                        })
                    } else {
                        Some(RetryStrategy::Fixed {
                            delay: retry_after.unwrap_or(BASE_RETRY_DELAY),
                            max_attempts: 2,
                        })
                    }
                }
            },
            ApiInternalServerError { .. } => Some(RetryStrategy::Fixed {
                delay: BASE_RETRY_DELAY,
                max_attempts: 3,
            }),
            ApiReadResponseError { .. }
            | HttpSend { .. }
            | DeserializeResponse { .. }
            | BadRequestFormat { .. } => Some(RetryStrategy::Fixed {
                delay: BASE_RETRY_DELAY,
                max_attempts: 3,
            }),
            // Retrying these errors definitely shouldn't help.
            HttpResponseError {
                status_code:
                    StatusCode::PAYLOAD_TOO_LARGE | StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED,
                ..
            }
            | AuthenticationError { .. }
            | PermissionError { .. }
            | NoApiKey { .. }
            | ApiEndpointNotFound { .. }
            | PromptTooLarge { .. }
            | InvalidEncryptedContent { .. } => None,
            // These errors might be transient, so retry them
            SerializeRequest { .. } | BuildRequestBody { .. } | StreamEndedUnexpectedly { .. } => {
                Some(RetryStrategy::Fixed {
                    delay: BASE_RETRY_DELAY,
                    max_attempts: 1,
                })
            }
            // Retry all other 4xx and 5xx errors once.
            HttpResponseError { status_code, .. }
                if status_code.is_client_error() || status_code.is_server_error() =>
            {
                Some(RetryStrategy::Fixed {
                    delay: BASE_RETRY_DELAY,
                    max_attempts: 3,
                })
            }
            // Retrying won't help for Payment Required errors.
            PaymentRequired => None,
            // Retrying won't help until the user consents to data retention
            // or switches models.
            DataRetentionConsentRequired { .. } => None,
            // Conservatively assume that any other errors are non-retryable
            HttpResponseError { .. } | Other(..) => Some(RetryStrategy::Fixed {
                delay: BASE_RETRY_DELAY,
                max_attempts: 2,
            }),
        }
    }
}

fn total_input_tokens(usage: language_model::TokenUsage) -> u64 {
    usage
        .input_tokens
        .saturating_add(usage.cache_creation_input_tokens)
        .saturating_add(usage.cache_read_input_tokens)
}

fn auto_compact_threshold_token_count(
    threshold: AutoCompactThreshold,
    max_token_count: u64,
) -> u64 {
    match threshold {
        AutoCompactThreshold::Percentage(percent) => {
            ((max_token_count as f64) * percent).ceil() as u64
        }
        AutoCompactThreshold::TokensUsed(tokens) => tokens,
        AutoCompactThreshold::TokensRemaining(tokens) => {
            max_token_count.saturating_sub(tokens).saturating_add(1)
        }
    }
}

/// Snapshot of the data needed to report an `"Agent Compaction Completed"`
/// telemetry event, captured when a compaction starts.
struct CompactionTelemetry {
    /// `"auto"` for threshold-triggered compaction, `"manual"` for `/compact`.
    trigger: &'static str,
    thread_id: String,
    parent_thread_id: Option<String>,
    prompt_id: String,
    model: String,
    compaction_model: String,
    thinking_effort: Option<String>,
    max_tokens: u64,
    /// Tokens in the context window immediately before compaction.
    tokens_before: Option<u64>,
    auto_compact_enabled: bool,
    auto_compact_threshold: String,
    auto_compact_threshold_tokens: u64,
    /// Number of times the compaction request was retried before the final
    /// outcome.
    retries: u32,
}

impl CompactionTelemetry {
    fn emit(self, status: &'static str, error: Option<String>, tokens_after: Option<u64>) {
        telemetry::event!(
            "Agent Compaction Completed",
            trigger = self.trigger,
            status = status,
            error = error,
            thread_id = self.thread_id,
            parent_thread_id = self.parent_thread_id,
            prompt_id = self.prompt_id,
            model = self.model,
            compaction_model = self.compaction_model,
            thinking_effort = self.thinking_effort,
            max_tokens = self.max_tokens,
            tokens_before = self.tokens_before,
            tokens_after = tokens_after,
            auto_compact_enabled = self.auto_compact_enabled,
            auto_compact_threshold = self.auto_compact_threshold,
            auto_compact_threshold_tokens = self.auto_compact_threshold_tokens,
            retries = self.retries,
        );
    }
}

fn user_message_byte_len(message: &LanguageModelRequestMessage) -> usize {
    message
        .content
        .iter()
        .map(|content| match content {
            MessageContent::Text(text) => text.len(),
            MessageContent::Image(image) => image.len(),
            // These can never occur in a user message
            MessageContent::Thinking { .. }
            | MessageContent::RedactedThinking(_)
            | MessageContent::ToolResult(_)
            | MessageContent::ToolUse(_)
            | MessageContent::Compaction(_) => 0,
        })
        .sum()
}

fn truncate_user_message_to_byte_budget(
    mut message: LanguageModelRequestMessage,
    byte_budget: usize,
) -> Option<LanguageModelRequestMessage> {
    let mut remaining_bytes = byte_budget;
    let mut content = Vec::with_capacity(message.content.len());

    for item in message.content {
        match item {
            MessageContent::Text(text) => {
                let fits = text.len() <= remaining_bytes;
                if let Some(text) = take_text_within_byte_budget(text, &mut remaining_bytes) {
                    content.push(MessageContent::Text(text));
                }
                if !fits {
                    break;
                }
            }
            MessageContent::Image(image) => {
                let byte_len = image.len();
                if let Some(bytes) = remaining_bytes.checked_sub(byte_len) {
                    remaining_bytes = bytes;
                    content.push(MessageContent::Image(image));
                } else {
                    break;
                }
            }
            // These can never occur in a user message
            MessageContent::Thinking { .. }
            | MessageContent::RedactedThinking(_)
            | MessageContent::ToolResult(_)
            | MessageContent::ToolUse(_)
            | MessageContent::Compaction(_) => {}
        }
    }

    if content.is_empty() {
        None
    } else {
        message.content = content;
        Some(message)
    }
}

fn take_text_within_byte_budget(text: String, remaining_bytes: &mut usize) -> Option<String> {
    if text.is_empty() || *remaining_bytes == 0 {
        return None;
    }

    if let Some(bytes) = remaining_bytes.checked_sub(text.len()) {
        *remaining_bytes = bytes;
        return Some(text);
    }

    let end = text.floor_char_boundary((*remaining_bytes).min(text.len()));
    *remaining_bytes = 0;

    let text = text[..end].to_string();

    if text.is_empty() { None } else { Some(text) }
}

/// Describes where a streamed compaction summary should land in the thread
/// once it completes successfully.
enum CompactionInsertion {
    /// Automatic compaction inserts the summary at an index computed up front
    /// (which may be before a trailing not-yet-answered user message).
    Auto { insertion_ix: usize },
    /// Manual `/compact` appends a zero-content user message followed by the summary.
    Manual { marker_id: ClientUserMessageId },
}

struct RunningTurn {
    /// Holds the task that handles agent interaction until the end of the turn.
    /// Survives across multiple requests as the model performs tool calls and
    /// we run tools, report their results.
    _task: Task<()>,
    /// The current event stream for the running turn. Used to report a final
    /// cancellation event if we cancel the turn.
    event_stream: ThreadEventStream,
    /// The tools that are enabled for the current iteration of the turn.
    /// Refreshed at the start of each iteration via `refresh_turn_tools`.
    tools: BTreeMap<SharedString, Arc<dyn AnyAgentTool>>,
    /// Sender to signal tool cancellation. When cancel is called, this is
    /// set to true so all tools can detect user-initiated cancellation.
    cancellation_tx: watch::Sender<bool>,
    /// Senders for tools that support input streaming and have already been
    /// started but are still receiving input from the LLM.
    streaming_tool_inputs: HashMap<LanguageModelToolUseId, ToolInputSender>,
}

impl RunningTurn {
    fn new(
        event_stream: ThreadEventStream,
        tools: BTreeMap<SharedString, Arc<dyn AnyAgentTool>>,
        cancellation_tx: watch::Sender<bool>,
        task: Task<()>,
    ) -> Self {
        Self {
            _task: task,
            event_stream,
            tools,
            cancellation_tx,
            streaming_tool_inputs: HashMap::default(),
        }
    }

    fn cancel(mut self) -> Task<()> {
        log::debug!("Cancelling in progress turn");
        self.cancellation_tx.send(true).ok();
        self.event_stream.send_canceled();
        self._task
    }
}

pub(crate) fn messages_to_markdown(messages: &[Arc<Message>]) -> String {
    let mut markdown = String::new();
    for (ix, message) in messages.iter().enumerate() {
        if ix > 0 {
            markdown.push('\n');
        }
        match &**message {
            Message::User(_) => markdown.push_str("## User\n\n"),
            Message::Agent(_) => markdown.push_str("## Assistant\n\n"),
            Message::Resume | Message::Compaction(_) => {}
        }
        markdown.push_str(&message.to_markdown());
    }
    markdown
}

fn extend_request_history_until(
    messages: &[Arc<Message>],
    request_messages: &mut Vec<LanguageModelRequestMessage>,
    end_ix: usize,
) {
    let end_ix = end_ix.min(messages.len());
    let Some(compaction_ix) = latest_compaction_message_ix_before(messages, end_ix) else {
        for message in &messages[..end_ix] {
            request_messages.extend(message.to_request());
        }
        return;
    };

    if matches!(
        &*messages[compaction_ix],
        Message::Compaction(CompactionInfo::Summary(_))
    ) {
        request_messages.extend(retained_user_request_messages_before(
            messages,
            compaction_ix,
        ));
    }

    for message in &messages[compaction_ix..end_ix] {
        request_messages.extend(message.to_request());
    }
}

fn latest_compaction_message_ix_before(messages: &[Arc<Message>], end_ix: usize) -> Option<usize> {
    messages[..end_ix]
        .iter()
        .rposition(|message| matches!(&**message, Message::Compaction(_)))
}

fn retained_user_request_messages_before(
    messages: &[Arc<Message>],
    compaction_ix: usize,
) -> Vec<LanguageModelRequestMessage> {
    let mut remaining_bytes = COMPACTION_RETAINED_USER_MESSAGES_BYTE_BUDGET;
    let mut retained_messages = Vec::new();

    for message in messages[..compaction_ix].iter().rev() {
        let Message::User(user_message) = &**message else {
            continue;
        };
        if user_message.content.is_empty() {
            continue;
        }

        let request_message = user_message.to_request();
        let byte_count = user_message_byte_len(&request_message);
        if let Some(bytes) = remaining_bytes.checked_sub(byte_count) {
            remaining_bytes = bytes;
            retained_messages.push(request_message);
        } else {
            if remaining_bytes > 0
                && let Some(request_message) =
                    truncate_user_message_to_byte_budget(request_message, remaining_bytes)
            {
                retained_messages.push(request_message);
            }
            break;
        }
    }

    retained_messages.reverse();
    retained_messages
}

pub fn build_thread_title_request(
    messages: &[Arc<Message>],
    temperature: Option<f32>,
) -> LanguageModelRequest {
    let mut request = LanguageModelRequest {
        intent: Some(CompletionIntent::ThreadSummarization),
        temperature,
        ..Default::default()
    };
    extend_request_history_until(messages, &mut request.messages, messages.len());
    request.messages.push(LanguageModelRequestMessage {
        role: Role::User,
        content: vec![SUMMARIZE_THREAD_PROMPT.into()],
        cache: false,
        reasoning_details: None,
    });
    request
}

pub async fn stream_thread_title(
    model: Arc<dyn LanguageModel>,
    request: LanguageModelRequest,
    cx: &AsyncApp,
) -> Result<String> {
    let mut title = String::new();
    let mut events = model.stream_completion(request, cx).await?;
    while let Some(event) = events.next().await {
        let LanguageModelCompletionEvent::Text(text) = event? else {
            continue;
        };
        if let Some(newline_ix) = text.find(|ch| ch == '\n' || ch == '\r') {
            title.push_str(&text[..newline_ix]);
            break;
        }
        title.push_str(&text);
    }
    Ok(title)
}

/// Collects the permission judge model's answer (ADR-F52). Only the first two
/// non-empty lines matter, so the response is capped well below anything a
/// judge that follows the format would produce.
async fn stream_permission_judge_response(
    model: Arc<dyn LanguageModel>,
    request: LanguageModelRequest,
    cx: &AsyncApp,
) -> Result<String> {
    const RESPONSE_BYTE_LIMIT: usize = 2048;

    let mut response = String::new();
    let mut events = model.stream_completion(request, cx).await?;
    while let Some(event) = events.next().await {
        if let LanguageModelCompletionEvent::Text(text) = event? {
            response.push_str(&text);
            if response.len() >= RESPONSE_BYTE_LIMIT {
                break;
            }
        }
    }
    Ok(response)
}

pub struct TokenUsageUpdated(pub Option<acp_thread::TokenUsage>);

impl EventEmitter<TokenUsageUpdated> for Thread {}

pub struct TitleUpdated;

impl EventEmitter<TitleUpdated> for Thread {}

pub struct ModelChanged;

impl EventEmitter<ModelChanged> for Thread {}

/// A channel-based wrapper that delivers tool input to a running tool.
///
/// For non-streaming tools, created via `ToolInput::ready()` so `.recv()` resolves immediately.
/// For streaming tools, partial JSON snapshots arrive via `.recv_partial()` as the LLM streams
/// them, followed by the final complete input available through `.recv()`.
pub struct ToolInput<T> {
    rx: mpsc::UnboundedReceiver<ToolInputPayload<serde_json::Value>>,
    _phantom: PhantomData<T>,
}

impl<T: DeserializeOwned> ToolInput<T> {
    #[cfg(any(test, feature = "test-support"))]
    pub fn resolved(input: impl Serialize) -> Self {
        let value = serde_json::to_value(input).expect("failed to serialize tool input");
        Self::ready(value)
    }

    pub fn ready(value: serde_json::Value) -> Self {
        let (tx, rx) = mpsc::unbounded();
        tx.unbounded_send(ToolInputPayload::Full(value)).ok();
        Self {
            rx,
            _phantom: PhantomData,
        }
    }

    pub fn invalid_json(error_message: String) -> Self {
        let (tx, rx) = mpsc::unbounded();
        tx.unbounded_send(ToolInputPayload::InvalidJson { error_message })
            .ok();
        Self {
            rx,
            _phantom: PhantomData,
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn test() -> (ToolInputSender, Self) {
        let (sender, input) = ToolInputSender::channel();
        (sender, input.cast())
    }

    /// Wait for the final deserialized input, ignoring all partial updates.
    /// Non-streaming tools can use this to wait until the whole input is available.
    pub async fn recv(mut self) -> Result<T> {
        while let Ok(value) = self.next().await {
            match value {
                ToolInputPayload::Full(value) => return Ok(value),
                ToolInputPayload::Partial(_) => {}
                ToolInputPayload::InvalidJson { error_message } => {
                    return Err(anyhow!(error_message));
                }
            }
        }
        Err(anyhow!("tool input was not fully received"))
    }

    pub async fn next(&mut self) -> Result<ToolInputPayload<T>> {
        let value = self
            .rx
            .next()
            .await
            .ok_or_else(|| anyhow!("tool input was not fully received"))?;

        Ok(match value {
            ToolInputPayload::Partial(payload) => ToolInputPayload::Partial(payload),
            ToolInputPayload::Full(payload) => {
                ToolInputPayload::Full(serde_json::from_value(payload)?)
            }
            ToolInputPayload::InvalidJson { error_message } => {
                ToolInputPayload::InvalidJson { error_message }
            }
        })
    }

    fn cast<U: DeserializeOwned>(self) -> ToolInput<U> {
        ToolInput {
            rx: self.rx,
            _phantom: PhantomData,
        }
    }
}

pub enum ToolInputPayload<T> {
    Partial(serde_json::Value),
    Full(T),
    InvalidJson { error_message: String },
}

pub struct ToolInputSender {
    has_received_final: bool,
    tx: mpsc::UnboundedSender<ToolInputPayload<serde_json::Value>>,
}

impl ToolInputSender {
    pub(crate) fn channel() -> (Self, ToolInput<serde_json::Value>) {
        let (tx, rx) = mpsc::unbounded();
        let sender = Self {
            tx,
            has_received_final: false,
        };
        let input = ToolInput {
            rx,
            _phantom: PhantomData,
        };
        (sender, input)
    }

    pub(crate) fn has_received_final(&self) -> bool {
        self.has_received_final
    }

    pub fn send_partial(&mut self, payload: serde_json::Value) {
        self.tx
            .unbounded_send(ToolInputPayload::Partial(payload))
            .ok();
    }

    pub fn send_full(&mut self, payload: serde_json::Value) {
        self.has_received_final = true;
        self.tx.unbounded_send(ToolInputPayload::Full(payload)).ok();
    }

    pub fn send_invalid_json(&mut self, error_message: String) {
        self.has_received_final = true;
        self.tx
            .unbounded_send(ToolInputPayload::InvalidJson { error_message })
            .ok();
    }
}

pub trait AgentTool
where
    Self: 'static + Sized,
{
    type Input: for<'de> Deserialize<'de> + Serialize + JsonSchema;
    type Output: for<'de> Deserialize<'de> + Serialize + Into<LanguageModelToolResultContent>;

    const NAME: &'static str;

    fn description() -> SharedString {
        let schema = schemars::schema_for!(Self::Input);
        SharedString::new(
            schema
                .get("description")
                .and_then(|description| description.as_str())
                .unwrap_or_default(),
        )
    }

    fn kind() -> acp::ToolKind;

    /// The initial tool title to display. Can be updated during the tool run.
    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        cx: &mut App,
    ) -> SharedString;

    /// Returns the JSON schema that describes the tool's input.
    fn input_schema(format: LanguageModelToolSchemaFormat) -> Schema {
        language_model::tool_schema::root_schema_for::<Self::Input>(format)
    }

    /// Returns whether the tool supports streaming of tool use parameters.
    fn supports_input_streaming() -> bool {
        false
    }

    /// Some tools rely on a provider for the underlying billing or other reasons.
    /// Allow the tool to check if they are compatible, or should be filtered out.
    fn supports_provider(_provider: &LanguageModelProviderId) -> bool {
        true
    }

    /// Whether this tool may be provided to an agent in a restricted workspace.
    ///
    /// Tools that return `false` are never exposed to the model while the
    /// workspace is restricted, and will fail if invoked in that state.
    fn allow_in_restricted_mode() -> bool {
        true
    }

    /// Runs the tool with the provided input.
    ///
    /// Returns `Result<Self::Output, Self::Output>` rather than `Result<Self::Output, anyhow::Error>`
    /// because tool errors are sent back to the model as tool results. This means error output must
    /// be structured and readable by the agent — not an arbitrary `anyhow::Error`. Returning the
    /// same `Output` type for both success and failure lets tools provide structured data while
    /// still signaling whether the invocation succeeded or failed.
    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>>;

    /// Emits events for a previous execution of the tool.
    fn replay(
        &self,
        _input: Self::Input,
        _output: Self::Output,
        _event_stream: ToolCallEventStream,
        _cx: &mut App,
    ) -> Result<()> {
        Ok(())
    }

    fn erase(self) -> Arc<dyn AnyAgentTool> {
        Arc::new(Erased(Arc::new(self)))
    }
}

pub struct Erased<T>(T);

pub struct AgentToolOutput {
    pub llm_output: Vec<LanguageModelToolResultContent>,
    pub raw_output: serde_json::Value,
}

impl From<anyhow::Error> for AgentToolOutput {
    fn from(error: anyhow::Error) -> Self {
        let llm_output = vec![error.into()];
        let raw_output = serde_json::to_value(&llm_output).unwrap_or_else(|e| {
            log::error!("Failed to serialize tool output: {e}");
            serde_json::Value::Null
        });
        Self {
            raw_output,
            llm_output,
        }
    }
}

pub trait AnyAgentTool {
    fn name(&self) -> SharedString;
    fn description(&self) -> SharedString;
    fn kind(&self) -> acp::ToolKind;
    fn initial_title(&self, input: serde_json::Value, _cx: &mut App) -> SharedString;
    fn input_schema(&self, format: LanguageModelToolSchemaFormat) -> Result<serde_json::Value>;
    fn supports_input_streaming(&self) -> bool {
        false
    }
    fn supports_provider(&self, _provider: &LanguageModelProviderId) -> bool {
        true
    }
    fn allow_in_restricted_mode(&self) -> bool {
        true
    }
    /// See [`AgentTool::run`] for why this returns `Result<AgentToolOutput, AgentToolOutput>`.
    fn run(
        self: Arc<Self>,
        input: ToolInput<serde_json::Value>,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<AgentToolOutput, AgentToolOutput>>;
    fn replay(
        &self,
        input: serde_json::Value,
        output: serde_json::Value,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Result<()>;
}

impl<T> AnyAgentTool for Erased<Arc<T>>
where
    T: AgentTool,
{
    fn name(&self) -> SharedString {
        T::NAME.into()
    }

    fn description(&self) -> SharedString {
        T::description()
    }

    fn kind(&self) -> acp::ToolKind {
        T::kind()
    }

    fn supports_input_streaming(&self) -> bool {
        T::supports_input_streaming()
    }

    fn initial_title(&self, input: serde_json::Value, _cx: &mut App) -> SharedString {
        let parsed_input = serde_json::from_value(input.clone()).map_err(|_| input);
        self.0.initial_title(parsed_input, _cx)
    }

    fn input_schema(&self, format: LanguageModelToolSchemaFormat) -> Result<serde_json::Value> {
        let mut json = serde_json::to_value(T::input_schema(format))?;
        language_model::tool_schema::adapt_schema_to_format(&mut json, format)?;
        Ok(json)
    }

    fn supports_provider(&self, provider: &LanguageModelProviderId) -> bool {
        T::supports_provider(provider)
    }

    fn allow_in_restricted_mode(&self) -> bool {
        T::allow_in_restricted_mode()
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<serde_json::Value>,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<AgentToolOutput, AgentToolOutput>> {
        let tool_input: ToolInput<T::Input> = input.cast();
        let task = self.0.clone().run(tool_input, event_stream, cx);
        cx.spawn(async move |_cx| match task.await {
            Ok(output) => {
                let raw_output = serde_json::to_value(&output).unwrap_or_else(|e| {
                    log::error!("Failed to serialize tool output: {e}");
                    serde_json::Value::Null
                });
                Ok(AgentToolOutput {
                    raw_output,
                    llm_output: vec![output.into()],
                })
            }
            Err(error_output) => {
                let raw_output = serde_json::to_value(&error_output).unwrap_or_else(|e| {
                    log::error!("Failed to serialize tool error output: {e}");
                    serde_json::Value::Null
                });
                Err(AgentToolOutput {
                    llm_output: vec![error_output.into()],
                    raw_output,
                })
            }
        })
    }

    fn replay(
        &self,
        input: serde_json::Value,
        output: serde_json::Value,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Result<()> {
        let input = serde_json::from_value(input)?;
        let output = serde_json::from_value(output)?;
        self.0.replay(input, output, event_stream, cx)
    }
}

/// Builds the ACP-facing tool call id for a tool use in the message at
/// `message_ix`.
///
/// Provider-issued `tool_use` ids aren't guaranteed unique across separate
/// request/response cycles in the same turn -- some providers reset a
/// per-request counter (e.g. `call_1`, `call_2`, ...), so the same raw id
/// can recur. Scoping by message index keeps the id stable for one tool
/// call's lifetime while preventing it from colliding with an unrelated one.
pub(crate) fn scoped_tool_call_id(
    message_ix: usize,
    tool_use_id: &LanguageModelToolUseId,
) -> acp::ToolCallId {
    // `message_ix` is non-zero-padded decimal, so the `:` delimiter is always
    // unambiguous -- this would break if the index were zero-padded.
    acp::ToolCallId::new(format!("{message_ix}:{tool_use_id}"))
}

#[derive(Clone)]
struct ThreadEventStream(mpsc::UnboundedSender<Result<ThreadEvent>>);

impl ThreadEventStream {
    fn send_user_message(&self, message: &UserMessage) {
        self.0
            .unbounded_send(Ok(ThreadEvent::UserMessage(message.clone())))
            .ok();
    }

    fn send_text(&self, text: &str) {
        self.0
            .unbounded_send(Ok(ThreadEvent::AgentText(text.to_string())))
            .ok();
    }

    fn send_thinking(&self, text: &str) {
        self.0
            .unbounded_send(Ok(ThreadEvent::AgentThinking(text.to_string())))
            .ok();
    }

    fn send_tool_call(
        &self,
        id: &acp::ToolCallId,
        tool_name: &str,
        title: SharedString,
        kind: acp::ToolKind,
        input: serde_json::Value,
    ) {
        self.0
            .unbounded_send(Ok(ThreadEvent::ToolCall(Self::initial_tool_call(
                id,
                tool_name,
                title.to_string(),
                kind,
                input,
            ))))
            .ok();
    }

    fn initial_tool_call(
        id: &acp::ToolCallId,
        tool_name: &str,
        title: String,
        kind: acp::ToolKind,
        input: serde_json::Value,
    ) -> acp::ToolCall {
        acp::ToolCall::new(id.clone(), title)
            .kind(kind)
            .raw_input(input)
            .meta(acp_thread::meta_with_tool_name(tool_name))
    }

    fn update_tool_call_fields(
        &self,
        tool_call_id: &acp::ToolCallId,
        fields: acp::ToolCallUpdateFields,
        meta: Option<acp::Meta>,
    ) {
        self.0
            .unbounded_send(Ok(ThreadEvent::ToolCallUpdate(
                acp::ToolCallUpdate::new(tool_call_id.clone(), fields)
                    .meta(meta)
                    .into(),
            )))
            .ok();
    }

    fn resolve_tool_call_authorization(
        &self,
        tool_call_id: &acp::ToolCallId,
        outcome: acp_thread::SelectedPermissionOutcome,
    ) {
        self.0
            .unbounded_send(Ok(ThreadEvent::ToolCallAuthorizationResolved {
                tool_call_id: tool_call_id.clone(),
                outcome,
            }))
            .ok();
    }

    fn send_retry(&self, status: acp_thread::RetryStatus) {
        self.0.unbounded_send(Ok(ThreadEvent::Retry(status))).ok();
    }

    fn send_context_compaction(
        &self,
        id: acp_thread::ContextCompactionId,
        status: acp_thread::ContextCompactionStatus,
    ) {
        self.0
            .unbounded_send(Ok(ThreadEvent::ContextCompaction(
                acp_thread::ContextCompaction {
                    id,
                    status,
                    summary: None,
                },
            )))
            .ok();
    }

    fn send_context_compaction_update(
        &self,
        id: acp_thread::ContextCompactionId,
        summary_delta: &str,
    ) {
        self.0
            .unbounded_send(Ok(ThreadEvent::ContextCompactionUpdate(
                acp_thread::ContextCompactionUpdate {
                    id,
                    summary_delta: summary_delta.to_string(),
                    status: None,
                },
            )))
            .ok();
    }

    fn update_context_compaction_status(
        &self,
        id: acp_thread::ContextCompactionId,
        status: acp_thread::ContextCompactionStatus,
    ) {
        self.0
            .unbounded_send(Ok(ThreadEvent::ContextCompactionUpdate(
                acp_thread::ContextCompactionUpdate {
                    id,
                    summary_delta: String::new(),
                    status: Some(status),
                },
            )))
            .ok();
    }

    fn send_stop(&self, reason: acp::StopReason) {
        self.0.unbounded_send(Ok(ThreadEvent::Stop(reason))).ok();
    }

    fn send_canceled(&self) {
        self.0
            .unbounded_send(Ok(ThreadEvent::Stop(acp::StopReason::Cancelled)))
            .ok();
    }

    fn send_error(&self, error: impl Into<anyhow::Error>) {
        self.0.unbounded_send(Err(error.into())).ok();
    }
}

/// The user's choice on the card shown when the OS sandbox could not be
/// created for a command (see
/// [`ToolCallEventStream::ask_to_retry_sandbox_creation`]). Only the Bubblewrap
/// sandboxes (Linux directly, Windows via WSL) can fail to be created, so this
/// is gated to those platforms.
#[cfg(any(target_os = "linux", target_os = "windows"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SandboxRetryDecision {
    /// Probe the environment again (e.g. after the user installed `bwrap`).
    Retry,
    /// Don't run the command.
    Cancel,
}

/// The choices on the card shown when the OS sandbox could not be created
/// (ADR-F46): try again after fixing the environment, or cancel. Running the
/// command without a sandbox is deliberately not one of them.
///
/// `retries` is how many times the user already pressed Retry for this
/// command; the label numbers the next attempt so repeated presses visibly
/// advance.
pub fn sandbox_retry_permission_options(retries: usize) -> acp_thread::PermissionOptions {
    let retry_label = if retries == 0 {
        "再試行".to_string()
    } else {
        format!("再試行 ({} 回目)", retries + 1)
    };
    acp_thread::PermissionOptions::Flat(vec![
        // Retry isn't an allow/deny choice and the UI dispatches on its id.
        // `RejectAlways` has no keyboard shortcut, so neither allow shortcut
        // can press it.
        acp::PermissionOption::new(
            acp::PermissionOptionId::new(acp_thread::SANDBOX_FALLBACK_RETRY_OPTION_ID),
            retry_label,
            acp::PermissionOptionKind::RejectAlways,
        ),
        acp::PermissionOption::new(
            acp::PermissionOptionId::new(acp_thread::SandboxPermission::Deny.as_id()),
            "キャンセル",
            acp::PermissionOptionKind::RejectOnce,
        ),
    ])
}

// Shared with the cards Zed shows for external agents' commands and writes
// (ADR-F49), so both offer exactly the same two choices.
use acp_thread::UNSANDBOXED_COMMAND_ALLOW_OPTION_ID;
pub use acp_thread::unsandboxed_command_permission_options;

/// Writes the permission judge's state (ADR-F52) to both places that need it:
/// the tool card's meta, and the thread's own record, which is what gets saved.
///
/// Deliberately usable without a `cx`, because
/// [`PermissionJudgeInProgress`] has to write from `Drop`.
struct PermissionJudgeRecorder {
    stream: ThreadEventStream,
    tool_call_id: acp::ToolCallId,
    tool_use_id: LanguageModelToolUseId,
    permission_judge: Rc<RefCell<ThreadPermissionJudge>>,
    /// Filled in once the judge model has been resolved; stays empty when
    /// there was nothing to judge with.
    judge_model_id: RefCell<String>,
}

impl PermissionJudgeRecorder {
    fn set_judge_model_id(&self, model_id: String) {
        *self.judge_model_id.borrow_mut() = model_id;
    }

    fn record(&self, state: acp_thread::PermissionJudgeState, reason: String, reused: bool) {
        let details = acp_thread::PermissionJudgeDetails {
            state,
            reason,
            reused,
        };
        // Only a finished judgement is worth saving: a thread saved while one
        // was still running would come back saying 「AI が判定中…」 about a
        // call that ended long ago.
        let judgement = (state != acp_thread::PermissionJudgeState::Judging).then(|| {
            DbPermissionJudgement::new(
                &details,
                self.judge_model_id.borrow().clone(),
                chrono::Utc::now(),
            )
        });
        self.permission_judge
            .borrow_mut()
            .record_judgement(self.tool_use_id.clone(), judgement);
        self.stream.update_tool_call_fields(
            &self.tool_call_id,
            acp::ToolCallUpdateFields::new(),
            Some(acp_thread::meta_with_permission_judge(details)),
        );
    }
}

/// Marks a tool call as being judged, and — if the judgement is dropped before
/// it produces a verdict — says so on the card instead of leaving 「AI が判定
/// 中…」 there forever (ADR-F52).
///
/// Cancelling a turn drops the whole authorization task, so this is the only
/// place the abandoned judgement can be noticed from the agent's side.
struct PermissionJudgeInProgress<'a> {
    recorder: &'a PermissionJudgeRecorder,
    armed: bool,
}

impl<'a> PermissionJudgeInProgress<'a> {
    fn arm(recorder: &'a PermissionJudgeRecorder) -> Self {
        recorder.record(
            acp_thread::PermissionJudgeState::Judging,
            String::new(),
            false,
        );
        Self {
            recorder,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PermissionJudgeInProgress<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.recorder.record(
                acp_thread::PermissionJudgeState::Superseded,
                acp_thread::PERMISSION_JUDGE_CANCELED_REASON.to_string(),
                false,
            );
        }
    }
}

/// Whether a tool's arguments carry anything the permission judge could weigh
/// (ADR-F52). An absent, empty or argument-less input leaves only the tool's
/// own name, which is not something the judge may approve a call on.
fn tool_input_says_something(input: &serde_json::Value) -> bool {
    match input {
        serde_json::Value::Null => false,
        serde_json::Value::Object(fields) => !fields.is_empty(),
        serde_json::Value::Array(items) => !items.is_empty(),
        _ => true,
    }
}

#[derive(Clone)]
pub struct ToolCallEventStream {
    tool_use_id: LanguageModelToolUseId,
    /// The ACP-facing id for this tool call (see [`scoped_tool_call_id`]).
    /// Distinct from `tool_use_id`, which is the raw, provider-issued id.
    tool_call_id: acp::ToolCallId,
    stream: ThreadEventStream,
    fs: Option<Arc<dyn Fs>>,
    cancellation_rx: watch::Receiver<bool>,
    /// Shared, thread-scoped sandbox grants (see [`Thread::sandbox_grants`]).
    sandbox_grants: Rc<RefCell<ThreadSandboxGrants>>,
    /// Shared, thread-scoped permission judge memory (see
    /// [`Thread::permission_judge`]).
    permission_judge: Rc<RefCell<ThreadPermissionJudge>>,
    /// The owning thread, used to trigger a save when a "for this thread"
    /// sandbox grant is recorded so it survives reopening. `None` in tests and
    /// for streams not tied to a live thread.
    thread: Option<WeakEntity<Thread>>,
}

impl ToolCallEventStream {
    #[cfg(any(test, feature = "test-support"))]
    pub fn test() -> (Self, ToolCallEventStreamReceiver) {
        let (stream, receiver, _cancellation_tx) = Self::test_with_cancellation();
        (stream, receiver)
    }

    /// Like [`Self::test`], but the returned stream shares the provided
    /// thread-scoped sandbox grants. This mirrors how a real [`Thread`] builds a
    /// distinct event stream per tool call while sharing one set of grants, so
    /// tests can exercise sequences of tool calls within the same conversation.
    #[cfg(test)]
    pub(crate) fn test_with_grants(
        sandbox_grants: Rc<RefCell<ThreadSandboxGrants>>,
    ) -> (Self, ToolCallEventStreamReceiver) {
        let (events_tx, events_rx) = mpsc::unbounded::<Result<ThreadEvent>>();
        let (_cancellation_tx, cancellation_rx) = watch::channel(false);

        // The raw and scoped ids deliberately differ (mirroring
        // `scoped_tool_call_id`) so that conflating them fails tests instead
        // of silently passing.
        let stream = ToolCallEventStream::new(
            "test_id".into(),
            acp::ToolCallId::new("0:test_id"),
            ThreadEventStream(events_tx),
            None,
            cancellation_rx,
            sandbox_grants,
            Rc::new(RefCell::new(ThreadPermissionJudge::default())),
            None,
        );

        (stream, ToolCallEventStreamReceiver(events_rx))
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn test_with_cancellation() -> (Self, ToolCallEventStreamReceiver, watch::Sender<bool>) {
        let (events_tx, events_rx) = mpsc::unbounded::<Result<ThreadEvent>>();
        let (cancellation_tx, cancellation_rx) = watch::channel(false);

        // The raw and scoped ids deliberately differ (mirroring
        // `scoped_tool_call_id`) so that conflating them fails tests instead
        // of silently passing.
        let stream = ToolCallEventStream::new(
            "test_id".into(),
            acp::ToolCallId::new("0:test_id"),
            ThreadEventStream(events_tx),
            None,
            cancellation_rx,
            Rc::new(RefCell::new(ThreadSandboxGrants::default())),
            Rc::new(RefCell::new(ThreadPermissionJudge::default())),
            None,
        );

        (
            stream,
            ToolCallEventStreamReceiver(events_rx),
            cancellation_tx,
        )
    }

    /// Signal cancellation for this event stream. Only available in tests.
    #[cfg(any(test, feature = "test-support"))]
    pub fn signal_cancellation_with_sender(cancellation_tx: &mut watch::Sender<bool>) {
        cancellation_tx.send(true).ok();
    }

    fn new(
        tool_use_id: LanguageModelToolUseId,
        tool_call_id: acp::ToolCallId,
        stream: ThreadEventStream,
        fs: Option<Arc<dyn Fs>>,
        cancellation_rx: watch::Receiver<bool>,
        sandbox_grants: Rc<RefCell<ThreadSandboxGrants>>,
        permission_judge: Rc<RefCell<ThreadPermissionJudge>>,
        thread: Option<WeakEntity<Thread>>,
    ) -> Self {
        Self {
            tool_use_id,
            tool_call_id,
            stream,
            fs,
            cancellation_rx,
            sandbox_grants,
            permission_judge,
            thread,
        }
    }

    /// Whether the owning thread is a subagent, so prompts can say "for this
    /// subagent" instead of "for this thread".
    fn is_subagent(&self, cx: &App) -> bool {
        self.thread
            .as_ref()
            .and_then(|thread| thread.upgrade())
            .is_some_and(|thread| thread.read(cx).is_subagent())
    }

    /// Persist the thread so a freshly recorded "for this thread" sandbox grant
    /// survives a reopen. Saving is driven by the agent's `observe` on the
    /// thread entity, so a no-op `notify` is enough to schedule it.
    fn persist_thread_grants(thread: &Option<WeakEntity<Thread>>, cx: &AsyncApp) {
        let Some(thread) = thread else { return };
        cx.update(|cx| {
            thread.update(cx, |_thread, cx| cx.notify()).ok();
        });
    }

    /// Returns a future that resolves when the user cancels the tool call.
    /// Tools should select on this alongside their main work to detect user cancellation.
    pub fn cancelled_by_user(&self) -> impl std::future::Future<Output = ()> + '_ {
        let mut rx = self.cancellation_rx.clone();
        async move {
            loop {
                if *rx.borrow() {
                    return;
                }
                if rx.changed().await.is_err() {
                    // Sender dropped, will never be cancelled
                    std::future::pending::<()>().await;
                }
            }
        }
    }

    /// Returns true if the user has cancelled this tool call.
    /// This is useful for checking cancellation state after an operation completes,
    /// to determine if the completion was due to user cancellation.
    pub fn was_cancelled_by_user(&self) -> bool {
        *self.cancellation_rx.clone().borrow()
    }

    pub fn tool_use_id(&self) -> &LanguageModelToolUseId {
        &self.tool_use_id
    }

    /// The ACP-facing id for this tool call (see [`scoped_tool_call_id`]).
    pub fn tool_call_id(&self) -> &acp::ToolCallId {
        &self.tool_call_id
    }

    pub fn update_fields(&self, fields: acp::ToolCallUpdateFields) {
        self.stream
            .update_tool_call_fields(&self.tool_call_id, fields, None);
    }

    pub fn update_fields_with_meta(
        &self,
        fields: acp::ToolCallUpdateFields,
        meta: Option<acp::Meta>,
    ) {
        self.stream
            .update_tool_call_fields(&self.tool_call_id, fields, meta);
    }

    pub fn resolve_authorization(&self, outcome: acp_thread::SelectedPermissionOutcome) {
        self.stream
            .resolve_tool_call_authorization(&self.tool_call_id, outcome);
    }

    pub fn update_diff(&self, diff: Entity<acp_thread::Diff>) {
        self.stream
            .0
            .unbounded_send(Ok(ThreadEvent::ToolCallUpdate(
                acp_thread::ToolCallUpdateDiff {
                    id: self.tool_call_id.clone(),
                    diff,
                }
                .into(),
            )))
            .ok();
    }

    pub fn subagent_spawned(&self, id: acp::SessionId) {
        self.stream
            .0
            .unbounded_send(Ok(ThreadEvent::SubagentSpawned(id)))
            .ok();
    }

    /// Authorize a third-party tool (e.g., MCP tool from a context server).
    ///
    /// Unlike built-in tools, third-party tools don't support pattern-based permissions.
    /// They only support `default` (allow/deny/confirm) per tool.
    ///
    /// Uses the dropdown authorization flow with two granularities:
    /// - 「MCP ツール <display_name> は常に」 → sets `tools.<tool_id>.default = "allow"` or "deny"
    /// - 「今回のみ」 → allow/deny once
    ///
    /// Each label names only the scope: the dropdown shows it next to both the
    /// 許可 and 拒否 buttons.
    ///
    /// `judge_input` is the tool's actual arguments, for the "AI が判定" mode
    /// (ADR-F52). Callers pass `None` when the arguments never arrived, and the
    /// judge then refuses to answer rather than approving a call nobody — not
    /// it, not the user — has seen. Arguments that say nothing (`null`, `{}`,
    /// `[]`) are treated the same way.
    pub fn authorize_third_party_tool(
        &self,
        title: impl Into<String>,
        tool_id: String,
        display_name: String,
        judge_input: Option<&serde_json::Value>,
        cx: &mut App,
    ) -> Task<Result<()>> {
        let title = title.into();
        // The judge is told the tool's full id (`mcp:<server>:<tool>`), which
        // says which server would run it, and the arguments as the server will
        // receive them. Without arguments only the name is left to go on, and
        // approving a name is exactly how an MCP server got past this gate
        // before, so the judge is not asked and the user decides.
        let judge_context = judge_input
            .filter(|input| tool_input_says_something(input))
            .map(|input| PermissionJudgeContext::new(tool_id.clone(), vec![input.to_string()]));
        let always_label = format!("MCP ツール {display_name} は常に");
        let options = acp_thread::PermissionOptions::Dropdown(vec![
            acp_thread::PermissionOptionChoice {
                allow: acp::PermissionOption::new(
                    acp::PermissionOptionId::new(format!("always_allow_mcp:{tool_id}")),
                    always_label.clone(),
                    acp::PermissionOptionKind::AllowAlways,
                ),
                deny: acp::PermissionOption::new(
                    acp::PermissionOptionId::new(format!("always_deny_mcp:{tool_id}")),
                    always_label,
                    acp::PermissionOptionKind::RejectAlways,
                ),
                sub_patterns: vec![],
            },
            acp_thread::PermissionOptionChoice {
                allow: acp::PermissionOption::new(
                    acp::PermissionOptionId::new("allow"),
                    "今回のみ",
                    acp::PermissionOptionKind::AllowOnce,
                ),
                deny: acp::PermissionOption::new(
                    acp::PermissionOptionId::new("deny"),
                    "今回のみ",
                    acp::PermissionOptionKind::RejectOnce,
                ),
                sub_patterns: vec![],
            },
        ]);

        // MCP tools are gated only by tool id (no per-input pattern
        // matching), so we pass a single empty input value just to satisfy
        // `decide_permission_from_settings`' signature.
        let check_settings: Box<dyn Fn(&App) -> ToolPermissionDecision> =
            Box::new(move |cx: &App| {
                let settings = agent_settings::AgentSettings::get_global(cx);
                decide_permission_from_settings(&tool_id, &[String::new()], settings)
            });

        self.run_authorization_loop(
            title,
            options,
            None,
            Some(check_settings),
            judge_context,
            cx,
        )
    }

    /// Gate a tool call on user permission, driven by the agent's
    /// tool-permission settings.
    ///
    /// Evaluates the current settings up-front: returns `Ok(())` immediately
    /// if the tool is already allowed, an error if it is denied, and
    /// otherwise prompts the user for a decision. While a prompt is pending,
    /// a subscription to `SettingsStore` watches for changes (for example,
    /// when the user clicks "Always for …" on a sibling tool call and the
    /// new rule becomes globally visible). When settings change, the current
    /// prompt is dismissed and the decision is re-evaluated. This closes the
    /// gap where an "Always for …" decision on one pending tool call would
    /// not propagate to other pending tool calls in the same turn or in
    /// subagent turns.
    ///
    /// For authorizations that must always prompt regardless of settings
    /// (e.g. symlink-escape confirmations, sensitive settings-file edits),
    /// use [`Self::prompt`] instead.
    pub fn authorize(
        &self,
        title: impl Into<String>,
        context: ToolPermissionContext,
        cx: &mut App,
    ) -> Task<Result<()>> {
        let title = title.into();
        let options = context.build_permission_options();

        let judge_context =
            PermissionJudgeContext::new(context.tool_name.clone(), context.input_values.clone());
        let tool_name = context.tool_name.clone();
        let input_values = context.input_values.clone();
        let terminal_execution = context.terminal_execution;
        let check_settings: Box<dyn Fn(&App) -> ToolPermissionDecision> =
            Box::new(move |cx: &App| {
                let settings = agent_settings::AgentSettings::get_global(cx);
                match terminal_execution {
                    Some(execution) => decide_terminal_permission(
                        &input_values,
                        &settings.tool_permissions,
                        util::shell::ShellKind::system(),
                        execution,
                    ),
                    None => decide_permission_from_settings(&tool_name, &input_values, settings),
                }
            });

        self.run_authorization_loop(
            title,
            options,
            Some(context),
            Some(check_settings),
            Some(judge_context),
            cx,
        )
    }

    /// Like [`Self::authorize`], but always prompts the user without
    /// consulting settings. Use this for authorizations that must be
    /// confirmed even when the user has configured `always_allow` rules —
    /// for example, symlink-escape confirmations or edits that target
    /// sensitive settings files.
    pub fn authorize_always_prompt(
        &self,
        title: impl Into<String>,
        context: ToolPermissionContext,
        cx: &mut App,
    ) -> Task<Result<()>> {
        let title = title.into();
        let options = context.build_permission_options();
        self.run_authorization_loop(title, options, Some(context), None, None, cx)
    }

    /// Like [`Self::authorize`], but for a path that lies outside every
    /// worktree root of the project (決定 3).
    ///
    /// Blanket approval reaches as far as the project, not the whole disk, so
    /// "allow everything" mode cannot approve this call on its own: only the
    /// user, or a pattern they wrote that names this path, can
    /// (see [`decide_permission_for_paths_outside_project`]). A deny default
    /// or a deny pattern still refuses it without asking, and a standing allow
    /// the user adds while the prompt is open still resolves it.
    ///
    /// Its choices are narrowed to the ones that mean what the card says
    /// (see [`Self::drop_choices_binding_more_than_this_path`]).
    pub fn authorize_outside_project(
        &self,
        title: impl Into<String>,
        context: ToolPermissionContext,
        cx: &mut App,
    ) -> Task<Result<()>> {
        // The card is the only place the user learns why a mode that approves
        // everything is asking, so the reason is part of the title rather than
        // left to the path for them to work out.
        self.authorize_with_note(title, "プロジェクトの外", context, cx)
    }

    /// Like [`Self::authorize_outside_project`], but with the card's own
    /// reason for asking in place of "outside the project" — for a relative
    /// path that would land inside the project but names no worktree in
    /// particular (決定 1). The permission rules are the same, because the
    /// reason blanket approval can't reach it is the same: we would have to
    /// pick a worktree on the user's behalf.
    pub fn authorize_with_note(
        &self,
        title: impl Into<String>,
        note: &str,
        context: ToolPermissionContext,
        cx: &mut App,
    ) -> Task<Result<()>> {
        let title = format!("{} ({note})", title.into());
        let mut options = context.build_permission_options();
        Self::drop_choices_binding_more_than_this_path(&mut options);
        let tool_name = context.tool_name.clone();
        let input_values = context.input_values.clone();
        let check_settings: Box<dyn Fn(&App) -> ToolPermissionDecision> =
            Box::new(move |cx: &App| {
                decide_permission_for_paths_outside_project(
                    &tool_name,
                    &input_values,
                    agent_settings::AgentSettings::get_global(cx),
                )
            });

        // No judge context: out here the settings never hand the decision to
        // the judge, which is told to refuse writes outside the worktree
        // roots anyway.
        self.run_authorization_loop(
            title,
            options,
            Some(context),
            Some(check_settings),
            None,
            cx,
        )
    }

    /// Removes the "always" choice that writes the tool's own default rather
    /// than a pattern (決定 3).
    ///
    /// On a card about one path outside the project that choice means
    /// something else than it says: allowing sets the tool's default to
    /// "allow", which out here is read back as "ask" — so it neither runs this
    /// call unprompted nor stops the next one — while quietly approving every
    /// call of that tool inside the project; denying likewise turns the tool
    /// off everywhere. What is left is the choice that names this path, which
    /// writes a pattern and so binds exactly what the card is about, and the
    /// once-only pair (which the auto-resolution path in
    /// [`Self::run_authorization_loop`] also needs).
    fn drop_choices_binding_more_than_this_path(options: &mut acp_thread::PermissionOptions) {
        // A path card is always a plain dropdown: the pattern-picking variant
        // is built for the terminal tool only.
        if let acp_thread::PermissionOptions::Dropdown(choices) = options {
            choices.retain(|choice| {
                let writes_the_tools_default = choice.sub_patterns.is_empty()
                    && (matches!(choice.allow.kind, acp::PermissionOptionKind::AllowAlways)
                        || matches!(choice.deny.kind, acp::PermissionOptionKind::RejectAlways));
                !writes_the_tools_default
            });
        }
    }

    /// Gate an agent terminal command that will run outside any sandbox, with
    /// Zed's own permissions, or inside one that may write anywhere but the
    /// protected paths (`writes_anywhere`), which is nearly the same (ADR-F46).
    ///
    /// Auto-approval only applies inside the sandbox (see
    /// [`decide_terminal_permission`]): deny rules and the built-in safety rules
    /// still refuse the command without asking, and anything else, including
    /// what the tool permissions would have approved, shows a card that says
    /// the command runs outside the sandbox and why (`outside_sandbox_reason`). The card
    /// offers only a one-time allow or a denial. Nothing chosen here is
    /// persisted, and unlike [`Self::authorize`] a later settings change never
    /// resolves the card.
    pub(crate) fn authorize_unsandboxed_command(
        &self,
        command: String,
        outside_sandbox_reason: String,
        writes_anywhere: bool,
        cx: &mut App,
    ) -> Task<Result<()>> {
        let decision = decide_terminal_permission(
            std::slice::from_ref(&command),
            &AgentSettings::get_global(cx).tool_permissions,
            util::shell::ShellKind::system(),
            TerminalExecution::Unsandboxed,
        );
        if let ToolPermissionDecision::Deny(reason) = decision {
            return Task::ready(Err(anyhow!(reason)));
        }

        let details = acp_thread::SandboxAuthorizationDetails {
            command: None,
            network_hosts: Vec::new(),
            network_all_hosts: false,
            allow_fs_write_all: writes_anywhere,
            write_paths: Vec::new(),
            warn_windows_fs: false,
            reason: String::new(),
            outside_sandbox_reason: Some(outside_sandbox_reason),
        };
        let stream = self.stream.clone();
        let tool_call_id = self.tool_call_id.clone();
        cx.spawn(async move |_cx| {
            let (response_tx, response_rx) = oneshot::channel();
            if let Err(error) = stream
                .0
                .unbounded_send(Ok(ThreadEvent::ToolCallAuthorization(
                    ToolCallAuthorization {
                        tool_call: acp::ToolCallUpdate::new(
                            tool_call_id,
                            acp::ToolCallUpdateFields::new().title(command),
                        )
                        .meta(acp_thread::meta_with_sandbox_authorization(details)),
                        options: unsandboxed_command_permission_options(),
                        response: response_tx,
                        context: None,
                        kind: acp_thread::AuthorizationKind::PermissionGrant,
                    },
                )))
            {
                log::error!("Failed to send unsandboxed command authorization: {error}");
                return Err(anyhow!(
                    "Failed to send unsandboxed command authorization: {error}"
                ));
            }

            let outcome = response_rx
                .await
                .map_err(|_| anyhow!("authorization channel closed"))?;
            ensure_tool_call_authorization_not_interrupted(&outcome)?;
            // Anything but the one-time allow, including an "always" choice some
            // other surface might send, is a denial and is never persisted: no
            // allow rule may cover commands that run outside the sandbox.
            if outcome.option_id.0.as_ref() == UNSANDBOXED_COMMAND_ALLOW_OPTION_ID {
                Ok(())
            } else {
                Err(anyhow!("Permission to run tool denied by user"))
            }
        })
    }

    /// Gate a sandbox *escalation* (network access, per-path writes, or full
    /// filesystem write access) on user approval.
    ///
    /// Offers the user three grant lifetimes — "once", "for the rest of this
    /// thread", and "always". Thread grants live in the shared, in-memory
    /// [`ThreadSandboxGrants`]. Always grants are persisted in agent settings
    /// and are also observed while a prompt is pending, matching the
    /// settings-driven authorization flow for regular tools.
    pub(crate) fn authorize_sandbox(
        &self,
        request: SandboxRequest,
        reason: String,
        cx: &mut App,
    ) -> Task<Result<()>> {
        if Self::sandbox_request_covered_by_grants(&request, &self.sandbox_grants, cx) {
            return Task::ready(Ok(()));
        }

        let (network_hosts, network_all_hosts) = match &request.network {
            crate::sandboxing::NetworkRequest::None => (Vec::new(), false),
            crate::sandboxing::NetworkRequest::AnyHost => (Vec::new(), true),
            crate::sandboxing::NetworkRequest::Hosts(hosts) => {
                (hosts.iter().map(|host| host.to_string()).collect(), false)
            }
        };
        let sandbox_authorization_details = acp_thread::SandboxAuthorizationDetails {
            // The command stays in the tool-call title (set by the terminal
            // tool), so the approval card keeps showing it; the details only
            // describe the requested access and the agent's reason.
            command: None,
            network_hosts,
            network_all_hosts,
            allow_fs_write_all: request.allow_fs_write_all,
            write_paths: request.write_paths.clone(),
            // The Windows-drive warning is a separate pre-prompt
            // (`authorize_windows_fs_warning`), never part of the escalation
            // prompt, so this stays off here.
            warn_windows_fs: false,
            reason,
            outside_sandbox_reason: None,
        };
        let options = if request.allow_fs_write_all {
            // Writing anywhere but the protected paths is nearly the same as
            // running outside the sandbox, so like an unsandboxed command it can
            // only be allowed once (ADR-F46).
            unsandboxed_command_permission_options()
        } else {
            let allow_thread_label = if self.is_subagent(cx) {
                "許可 (このサブエージェント)"
            } else {
                "許可 (このスレッド)"
            };
            acp_thread::PermissionOptions::Flat(vec![
                acp::PermissionOption::new(
                    acp::PermissionOptionId::new(acp_thread::SandboxPermission::AllowOnce.as_id()),
                    "許可 (今回のみ)",
                    acp::PermissionOptionKind::AllowOnce,
                ),
                acp::PermissionOption::new(
                    acp::PermissionOptionId::new(
                        acp_thread::SandboxPermission::AllowThread.as_id(),
                    ),
                    allow_thread_label,
                    acp::PermissionOptionKind::AllowAlways,
                ),
                acp::PermissionOption::new(
                    acp::PermissionOptionId::new(
                        acp_thread::SandboxPermission::AllowAlways.as_id(),
                    ),
                    "常に許可",
                    acp::PermissionOptionKind::AllowAlways,
                ),
                acp::PermissionOption::new(
                    acp::PermissionOptionId::new(acp_thread::SandboxPermission::Deny.as_id()),
                    "拒否",
                    acp::PermissionOptionKind::RejectOnce,
                ),
            ])
        };

        let fs = self.fs.clone();
        let stream = self.stream.clone();
        let tool_call_id = self.tool_call_id.clone();
        let sandbox_grants = self.sandbox_grants.clone();
        let thread = self.thread.clone();
        let auto_allow_outcome = match auto_resolve_permission_outcome(&options, true) {
            Ok(outcome) => outcome,
            Err(error) => return Task::ready(Err(error)),
        };
        cx.spawn(async move |cx| {
            let (response_tx, mut response_rx) = oneshot::channel();
            if let Err(error) = stream
                .0
                .unbounded_send(Ok(ThreadEvent::ToolCallAuthorization(
                    ToolCallAuthorization {
                        tool_call: acp::ToolCallUpdate::new(
                            tool_call_id.clone(),
                            // Leave the title untouched so the card keeps
                            // showing the command (matching the retry card).
                            acp::ToolCallUpdateFields::new(),
                        )
                        .meta(acp_thread::meta_with_sandbox_authorization(
                            sandbox_authorization_details,
                        )),
                        options,
                        response: response_tx,
                        context: None,
                        kind: acp_thread::AuthorizationKind::PermissionGrant,
                    },
                )))
            {
                log::error!("Failed to send sandbox authorization: {error}");
                return Err(anyhow!("Failed to send sandbox authorization: {error}"));
            }

            let (mut settings_tx, mut settings_rx) = watch::channel(());
            let _settings_subscription = cx.update(|cx| {
                cx.observe_global::<SettingsStore>(move |_cx| {
                    settings_tx.send(()).ok();
                })
            });

            loop {
                let settings_changed = async {
                    if settings_rx.changed().await.is_err() {
                        std::future::pending::<()>().await;
                    }
                };
                futures::select_biased! {
                    outcome = (&mut response_rx).fuse() => {
                        let outcome = outcome.map_err(|_| anyhow!("authorization channel closed"))?;
                        ensure_tool_call_authorization_not_interrupted(&outcome)?;
                        return Self::handle_sandbox_permission_outcome(
                            &outcome,
                            &request,
                            sandbox_grants.clone(),
                            thread.clone(),
                            fs.clone(),
                            cx,
                        );
                    }
                    _ = settings_changed.fuse() => {
                        // Unrestricted writes are only ever allowed once
                        // (ADR-F46), so a settings change never approves them.
                        if !request.allow_fs_write_all && cx.update(|cx| Self::sandbox_request_covered_by_grants(
                            &request,
                            &sandbox_grants,
                            cx,
                        )) {
                            drop(response_rx);
                            stream.resolve_tool_call_authorization(
                                &tool_call_id,
                                auto_allow_outcome.clone(),
                            );
                            return Ok(());
                        }
                    }
                }
            }
        })
    }

    /// Confirm, before running a command whose sandbox will contain a Windows
    /// drive (DrvFs) path, that the user accepts the weaker integrity
    /// guarantees. This is a transient gate *in front of* the normal sandbox
    /// flow — it is never persisted or recorded as a grant. 「続行」 answers it
    /// for the rest of the thread (in memory only, 決定 1), because the caveat
    /// is a property of the project's location rather than of the command:
    /// asked per command it fired on every single one. 「中止」 is not
    /// remembered, so the next command asks again. Returns `Ok(())` on 「続行」
    /// (after which the caller proceeds to any escalation prompt) and `Err` on
    /// 「中止」.
    pub(crate) fn authorize_windows_fs_warning(&self, cx: &mut App) -> Task<Result<()>> {
        // If the warning is already disabled, don't prompt.
        if !AgentSettings::get_global(cx)
            .sandbox_permissions
            .warn_ntfs_grants
        {
            return Task::ready(Ok(()));
        }

        if self.sandbox_grants.borrow().windows_fs_warning_accepted() {
            return Task::ready(Ok(()));
        }

        let details = acp_thread::SandboxAuthorizationDetails {
            command: None,
            network_hosts: Vec::new(),
            network_all_hosts: false,
            allow_fs_write_all: false,
            write_paths: Vec::new(),
            warn_windows_fs: true,
            reason: String::new(),
            outside_sandbox_reason: None,
        };
        let options = acp_thread::PermissionOptions::Flat(vec![
            acp::PermissionOption::new(
                acp::PermissionOptionId::new(acp_thread::SandboxPermission::AllowOnce.as_id()),
                "続行",
                acp::PermissionOptionKind::AllowOnce,
            ),
            acp::PermissionOption::new(
                acp::PermissionOptionId::new(acp_thread::SandboxPermission::Deny.as_id()),
                "中止",
                acp::PermissionOptionKind::RejectOnce,
            ),
        ]);

        let stream = self.stream.clone();
        // The update must target the ACP-facing (scoped) id: an update keyed
        // by the raw provider id matches no existing tool call, and creating
        // one from empty fields is rejected downstream ("title is required"),
        // which tears down the stream and turns the prompt into a phantom
        // decline.
        let tool_call_id = self.tool_call_id.clone();
        let sandbox_grants = self.sandbox_grants.clone();
        cx.spawn(async move |_cx| {
            let (response_tx, response_rx) = oneshot::channel();
            if let Err(error) = stream
                .0
                .unbounded_send(Ok(ThreadEvent::ToolCallAuthorization(
                    ToolCallAuthorization {
                        tool_call: acp::ToolCallUpdate::new(
                            tool_call_id,
                            // Leave the title untouched so the card keeps
                            // showing the command (matching the escalation
                            // flow).
                            acp::ToolCallUpdateFields::new(),
                        )
                        .meta(acp_thread::meta_with_sandbox_authorization(details)),
                        options,
                        response: response_tx,
                        context: None,
                        kind: acp_thread::AuthorizationKind::PermissionGrant,
                    },
                )))
            {
                log::error!("Failed to send Windows-drive sandbox warning: {error}");
                return Err(anyhow!(
                    "Failed to send Windows-drive sandbox warning: {error}"
                ));
            }

            let outcome = response_rx
                .await
                .map_err(|_| anyhow!("authorization channel closed"))?;
            ensure_tool_call_authorization_not_interrupted(&outcome)?;
            match acp_thread::SandboxPermission::from_id(outcome.option_id.0.as_ref()) {
                Some(acp_thread::SandboxPermission::AllowOnce) => {
                    sandbox_grants.borrow_mut().accept_windows_fs_warning();
                    Ok(())
                }
                _ => Err(anyhow!("Windows-drive write aborted by user")),
            }
        })
    }

    fn sandbox_request_covered_by_grants(
        request: &SandboxRequest,
        sandbox_grants: &Rc<RefCell<ThreadSandboxGrants>>,
        cx: &App,
    ) -> bool {
        let settings = AgentSettings::get_global(cx);
        sandbox_grants
            .borrow()
            .covers_with_persistent(request, &settings.sandbox_permissions)
    }

    fn handle_sandbox_permission_outcome(
        outcome: &acp_thread::SelectedPermissionOutcome,
        request: &SandboxRequest,
        sandbox_grants: Rc<RefCell<ThreadSandboxGrants>>,
        thread: Option<WeakEntity<Thread>>,
        fs: Option<Arc<dyn Fs>>,
        cx: &AsyncApp,
    ) -> Result<()> {
        debug_assert!(
            outcome.params.is_none(),
            "unexpected params for sandbox permission"
        );

        let permission = acp_thread::SandboxPermission::from_id(outcome.option_id.0.as_ref());
        // The card for unrestricted writes only offers a one-time allow
        // (ADR-F46); a standing choice some other surface might send is a
        // denial, so it is never recorded or persisted.
        if request.allow_fs_write_all
            && matches!(
                permission,
                Some(
                    acp_thread::SandboxPermission::AllowThread
                        | acp_thread::SandboxPermission::AllowAlways
                )
            )
        {
            return Err(anyhow!("Permission to run tool denied by user"));
        }
        match permission {
            Some(acp_thread::SandboxPermission::AllowOnce) => Ok(()),
            Some(acp_thread::SandboxPermission::AllowThread) => {
                sandbox_grants.borrow_mut().record(request);
                Self::persist_thread_grants(&thread, cx);
                Ok(())
            }
            Some(acp_thread::SandboxPermission::AllowAlways) => {
                Self::persist_sandbox_always_permission(request, fs, cx);
                Ok(())
            }
            Some(acp_thread::SandboxPermission::Deny) => {
                Err(anyhow!("Permission to run tool denied by user"))
            }
            None => {
                let other = outcome.option_id.0.as_ref();
                debug_assert!(false, "unexpected sandbox permission option_id: {other}");
                Err(anyhow!("Permission to run tool denied by user"))
            }
        }
    }

    fn persist_sandbox_always_permission(
        request: &SandboxRequest,
        fs: Option<Arc<dyn Fs>>,
        cx: &AsyncApp,
    ) {
        let Some(fs) = fs else {
            log::error!(
                "Cannot persist \"allow always\" sandbox permission: no filesystem available"
            );
            return;
        };

        let request = request.clone();
        cx.update(|cx| {
            update_settings_file(fs, cx, move |settings, _| {
                let agent = settings.agent.get_or_insert_default();
                match &request.network {
                    crate::sandboxing::NetworkRequest::None => {}
                    crate::sandboxing::NetworkRequest::AnyHost => {
                        agent.allow_sandbox_all_hosts();
                    }
                    crate::sandboxing::NetworkRequest::Hosts(hosts) => {
                        // Rebuild the persisted list with subsumption pruning
                        // so granting `*.github.com` retires a previously
                        // persisted `api.github.com` instead of accumulating
                        // redundant entries. Unparsable hand-edited entries
                        // are preserved untouched.
                        let mut patterns = Vec::new();
                        let mut unparsable = Vec::new();
                        for raw in agent.sandbox_network_hosts() {
                            match http_proxy::HostPattern::parse(raw) {
                                Ok(pattern) => {
                                    crate::sandboxing::insert_host_pattern(&mut patterns, pattern)
                                }
                                Err(_) => unparsable.push(raw.clone()),
                            }
                        }
                        for host in hosts {
                            crate::sandboxing::insert_host_pattern(&mut patterns, host.clone());
                        }
                        let mut host_strings = unparsable;
                        host_strings.extend(patterns.iter().map(|pattern| pattern.to_string()));
                        agent.set_sandbox_network_hosts(host_strings);
                    }
                }

                for granted in request.write_paths {
                    // Persist the full (requested, resolved-canonical) pair so a
                    // persistent grant is rebuilt from the vetted canonical
                    // across restarts, not re-resolved by path string.
                    agent.add_sandbox_write_path(settings::GrantedWritePathContent {
                        requested: granted.requested,
                        resolved: granted.resolved,
                        on_windows_fs: granted.on_windows_fs,
                    });
                }
            });
        });
    }

    /// The sandbox permissions to actually enforce for a command: the union
    /// of this command's `request`, everything granted "for the rest of the
    /// conversation", and persistent "allow always" sandbox grants.
    ///
    /// Callers must apply this to the enforced sandbox policy (rather than
    /// the raw `request`) so standing grants keep working for later commands
    /// that write to a previously approved path without re-requesting it.
    pub(crate) fn effective_sandbox_request(
        &self,
        request: &SandboxRequest,
        persistent: &agent_settings::SandboxPermissions,
    ) -> SandboxRequest {
        self.sandbox_grants
            .borrow()
            .effective_with_persistent(request, persistent)
    }

    /// Ask the user to fix the environment and retry when the OS sandbox could
    /// not be created for a command (for example, `bwrap` is missing or user
    /// namespaces are disabled).
    ///
    /// Unlike [`Self::authorize_sandbox`], which gates a model-requested
    /// *escalation*, this surfaces a *system limitation*: the card explains why
    /// the sandbox failed (`reason`) and offers only to try again or to cancel.
    /// There is no way to run the command without a sandbox (ADR-F46), and
    /// nothing is recorded or persisted. Only the Bubblewrap sandboxes (Linux
    /// directly, Windows via WSL) can fail to be created, so this is gated to
    /// those platforms.
    ///
    /// `retries` is how many times the user has already pressed Retry for this
    /// command (see [`sandbox_retry_permission_options`]).
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    pub(crate) fn ask_to_retry_sandbox_creation(
        &self,
        command: Option<String>,
        reason: String,
        docs_section: Option<String>,
        retries: usize,
        cx: &mut App,
    ) -> Task<Result<SandboxRetryDecision>> {
        let details = acp_thread::SandboxFallbackAuthorizationDetails {
            command,
            reason,
            docs_section,
        };
        let options = sandbox_retry_permission_options(retries);
        let stream = self.stream.clone();
        let tool_call_id = self.tool_call_id.clone();
        cx.spawn(async move |_cx| {
            let (response_tx, response_rx) = oneshot::channel();
            if let Err(error) = stream
                .0
                .unbounded_send(Ok(ThreadEvent::ToolCallAuthorization(
                    ToolCallAuthorization {
                        // Leave the title untouched so the card keeps showing
                        // the command that is waiting for the sandbox; the
                        // failure reason comes with the details.
                        tool_call: acp::ToolCallUpdate::new(
                            tool_call_id,
                            acp::ToolCallUpdateFields::new(),
                        )
                        .meta(
                            acp_thread::meta_with_sandbox_fallback_authorization(details),
                        ),
                        options,
                        response: response_tx,
                        context: None,
                        kind: acp_thread::AuthorizationKind::ActionChoice,
                    },
                )))
            {
                log::error!("Failed to send sandbox retry prompt: {error}");
                return Err(anyhow!("Failed to send sandbox retry prompt: {error}"));
            }

            let outcome = response_rx
                .await
                .map_err(|_| anyhow!("authorization channel closed"))?;
            ensure_tool_call_authorization_not_interrupted(&outcome)?;

            if outcome.option_id.0.as_ref() == acp_thread::SANDBOX_FALLBACK_RETRY_OPTION_ID {
                Ok(SandboxRetryDecision::Retry)
            } else {
                Ok(SandboxRetryDecision::Cancel)
            }
        })
    }

    /// Prompts the user to choose between an explicit set of actions and
    /// returns the chosen `option_id`.
    ///
    /// Unlike [`Self::authorize`] / [`Self::authorize_always_prompt`], this
    /// does not interpret the user's choice as a permission grant — callers
    /// are responsible for handling each `option_id` explicitly. Use this
    /// when a tool needs the user to pick between several side-effecting
    /// actions (for example, "Save" vs "Discard" for a dirty buffer).
    pub fn prompt_for_decision(
        &self,
        title: Option<String>,
        message: Option<String>,
        options: Vec<acp::PermissionOption>,
        cx: &mut App,
    ) -> Task<Result<acp::PermissionOptionId>> {
        let options = acp_thread::PermissionOptions::Flat(options);
        let stream = self.stream.clone();
        let tool_call_id = self.tool_call_id.clone();
        cx.spawn(async move |_cx| {
            let mut fields = acp::ToolCallUpdateFields::new();
            if let Some(title) = title {
                fields = fields.title(title);
            }
            if let Some(message) = message {
                fields = fields.content(vec![acp::ToolCallContent::from(message)]);
            }

            let (response_tx, response_rx) = oneshot::channel();
            if let Err(error) = stream
                .0
                .unbounded_send(Ok(ThreadEvent::ToolCallAuthorization(
                    ToolCallAuthorization {
                        tool_call: acp::ToolCallUpdate::new(tool_call_id.clone(), fields),
                        options,
                        response: response_tx,
                        context: None,
                        kind: acp_thread::AuthorizationKind::ActionChoice,
                    },
                )))
            {
                log::error!("Failed to send tool call decision prompt: {error}");
                return Err(anyhow!("Failed to send tool call decision prompt: {error}"));
            }

            let outcome = response_rx
                .await
                .map_err(|_| anyhow!("authorization channel closed"))?;
            ensure_tool_call_authorization_not_interrupted(&outcome)?;
            Ok(outcome.option_id)
        })
    }

    /// Requests structured input from the user via an elicitation form and
    /// returns the raw response (accept with content, decline, or cancel).
    ///
    /// Unlike [`Self::prompt_for_decision`], which presents a fixed set of
    /// buttons, this renders a form from the provided JSON schema — so callers
    /// can offer free-text fields, single-select enums, and typed inputs. The
    /// session scope is attached by the connection layer.
    pub fn request_elicitation(
        &self,
        message: String,
        schema: acp::ElicitationSchema,
        cx: &mut App,
    ) -> Task<Result<acp::CreateElicitationResponse>> {
        let stream = self.stream.clone();
        let tool_use_id = self.tool_use_id.clone();
        cx.spawn(async move |_cx| {
            let (response_tx, response_rx) = oneshot::channel();
            if let Err(error) =
                stream
                    .0
                    .unbounded_send(Ok(ThreadEvent::Elicitation(ElicitationRequest {
                        tool_call_id: acp::ToolCallId::new(tool_use_id.to_string()),
                        message,
                        schema,
                        response: response_tx,
                    })))
            {
                log::error!("Failed to send elicitation request: {error}");
                return Err(anyhow!("Failed to send elicitation request: {error}"));
            }

            response_rx
                .await
                .map_err(|_| anyhow!("elicitation channel closed"))
        })
    }

    /// What the permission judge model is told about the project the tool
    /// action targets (ADR-F52): the worktree roots, so it can tell "inside the
    /// project" from "outside" it, and whether the user trusts the project.
    ///
    /// Read from the owning thread only once the authorization task is running:
    /// an MCP tool call authorizes itself from inside a `Thread` update, and
    /// reading the thread again there would panic. A stream with no thread
    /// (tests, streams not tied to a live thread) reports no roots and an
    /// untrusted project, which is the safe direction.
    fn permission_judge_project_context(
        thread: &Option<WeakEntity<Thread>>,
        cx: &App,
    ) -> (Vec<String>, bool) {
        let Some(thread) = thread.as_ref().and_then(|thread| thread.upgrade()) else {
            return (Vec::new(), false);
        };
        let project = thread.read(cx).project().clone();
        let project = project.read(cx);
        let worktree_roots = project
            .visible_worktrees(cx)
            .map(|worktree| worktree.read(cx).abs_path().to_string_lossy().into_owned())
            .collect();
        let project_trusted =
            !TrustedWorktrees::has_restricted_worktrees(&project.worktree_store(), cx);
        (worktree_roots, project_trusted)
    }

    /// Builds the recorder the judge writes its state through, for one tool
    /// call.
    fn permission_judge_recorder(&self) -> PermissionJudgeRecorder {
        PermissionJudgeRecorder {
            stream: self.stream.clone(),
            tool_call_id: self.tool_call_id.clone(),
            tool_use_id: self.tool_use_id.clone(),
            permission_judge: self.permission_judge.clone(),
            judge_model_id: RefCell::new(String::new()),
        }
    }

    /// Asks the permission judge model whether this tool action is safe
    /// (ADR-F52).
    ///
    /// Returns `Some(result)` when the call was decided without asking the
    /// user — either the judge approved it, or the user changed the settings
    /// mid-judgement into a definitive allow/deny — and `None` when the user
    /// has to be prompted after all. Anything short of an explicit `SAFE`
    /// (no model, an error, the 10-second timeout, an unparsable answer)
    /// returns `None`, so a judge that cannot answer costs a prompt rather
    /// than an unreviewed tool call.
    async fn run_permission_judge(
        recorder: &PermissionJudgeRecorder,
        judge_context: Option<PermissionJudgeContext>,
        thread: &Option<WeakEntity<Thread>>,
        check_settings: &dyn Fn(&App) -> ToolPermissionDecision,
        settings_rx: &mut watch::Receiver<()>,
        cx: &mut AsyncApp,
    ) -> Option<Result<()>> {
        let Some(mut judge_context) = judge_context else {
            recorder.record(
                acp_thread::PermissionJudgeState::Unsafe,
                acp_thread::PERMISSION_JUDGE_NO_ARGUMENTS_REASON.to_string(),
                false,
            );
            return None;
        };

        // Both the worktree roots and the judge model are read here, inside the
        // task: an MCP tool call authorizes itself from inside a `Thread`
        // update, and reading the thread at that point would panic.
        let (worktree_roots, project_trusted) =
            cx.update(|cx| Self::permission_judge_project_context(thread, cx));
        judge_context.worktree_roots = worktree_roots;
        judge_context.project_trusted = project_trusted;

        let configured_model =
            cx.update(|cx| LanguageModelRegistry::read_global(cx).permission_judge_model(cx));
        if let Some(configured) = configured_model.as_ref() {
            recorder.set_judge_model_id(configured.model.id().0.to_string());
        }

        // A verdict this thread already gave for exactly the same question is
        // reused rather than paid for again (ADR-F52). With no model there is
        // nothing to remember or to reuse, and an input too large for the judge
        // to have been shown in full is deliberately not remembered either
        // (see [`PermissionJudgeCacheKey::new`]); both fall through to a fresh
        // judgement.
        let cache_key = configured_model.as_ref().and_then(|configured| {
            PermissionJudgeCacheKey::new(&judge_context, configured.model.id().0.to_string())
        });
        if let Some(cache_key) = cache_key.as_ref() {
            let remembered = recorder
                .permission_judge
                .borrow()
                .remembered_safe_reason(cache_key);
            if let Some(reason) = remembered {
                recorder.record(acp_thread::PermissionJudgeState::Safe, reason, true);
                return Some(Ok(()));
            }
        }

        // Armed only once the verdict can no longer come from memory: dropping
        // the task from here on (the user cancels the turn) must leave the card
        // saying the judgement was abandoned rather than still running.
        let mut in_progress = PermissionJudgeInProgress::arm(recorder);

        let mut judgement = std::pin::pin!(
            Self::permission_judge_verdict(configured_model, judge_context, cx.clone()).fuse()
        );
        let verdict = loop {
            let settings_changed = async {
                if settings_rx.changed().await.is_err() {
                    std::future::pending::<()>().await;
                }
            };
            futures::select_biased! {
                verdict = judgement => break verdict,
                _ = settings_changed.fuse() => {
                    // The user changed the permission settings while the judge
                    // was thinking; the settings win and its answer is dropped.
                    let decision = cx.update(|cx| check_settings(cx));
                    let mut superseded = |reason: &str| {
                        in_progress.disarm();
                        recorder.record(
                            acp_thread::PermissionJudgeState::Superseded,
                            reason.to_string(),
                            false,
                        );
                    };
                    match decision {
                        ToolPermissionDecision::Allow => {
                            superseded("設定が変わったため判定を中止しました");
                            return Some(Ok(()));
                        }
                        ToolPermissionDecision::Deny(reason) => {
                            superseded("設定が変わったため判定を中止しました");
                            return Some(Err(anyhow!(reason)));
                        }
                        ToolPermissionDecision::Confirm => {
                            superseded("設定が変わったため判定を中止しました");
                            return None;
                        }
                        ToolPermissionDecision::Judge => continue,
                    }
                }
            }
        };
        in_progress.disarm();

        match verdict {
            PermissionJudgeVerdict::Safe { reason } => {
                if let Some(cache_key) = cache_key {
                    let permission_judge = recorder.permission_judge.clone();
                    let reason = reason.clone();
                    cx.update(|cx| {
                        ThreadPermissionJudge::remember_safe(
                            &permission_judge,
                            cache_key,
                            reason,
                            cx,
                        )
                    });
                }
                recorder.record(acp_thread::PermissionJudgeState::Safe, reason, false);
                Some(Ok(()))
            }
            PermissionJudgeVerdict::Unsafe { reason } => {
                recorder.record(acp_thread::PermissionJudgeState::Unsafe, reason, false);
                None
            }
        }
    }

    /// One non-conversational request to the judge model, bounded by
    /// [`PERMISSION_JUDGE_TIMEOUT`]. The audit line names the tool, the verdict,
    /// the model and how long it took, but never the input, which can carry
    /// secrets.
    async fn permission_judge_verdict(
        configured_model: Option<language_model::ConfiguredModel>,
        judge_context: PermissionJudgeContext,
        cx: AsyncApp,
    ) -> PermissionJudgeVerdict {
        let Some(configured_model) = configured_model else {
            log::info!(
                "permission judge: tool={} verdict=UNSAFE model=<none> elapsed=0ms",
                judge_context.tool_name
            );
            return PermissionJudgeVerdict::Unsafe {
                reason: PERMISSION_JUDGE_NO_MODEL_REASON.to_string(),
            };
        };
        let model = configured_model.model;
        let model_id = model.id().0.to_string();

        let request = LanguageModelRequest {
            intent: Some(CompletionIntent::PermissionJudge),
            messages: vec![
                LanguageModelRequestMessage {
                    role: Role::System,
                    content: vec![PERMISSION_JUDGE_SYSTEM_PROMPT.into()],
                    cache: false,
                    reasoning_details: None,
                },
                LanguageModelRequestMessage {
                    role: Role::User,
                    content: vec![build_permission_judge_prompt(&judge_context).into()],
                    cache: false,
                    reasoning_details: None,
                },
            ],
            temperature: Some(0.0),
            ..Default::default()
        };

        let started_at = std::time::Instant::now();
        let mut completion =
            std::pin::pin!(stream_permission_judge_response(model, request, &cx).fuse());
        let mut timeout = std::pin::pin!(
            cx.background_executor()
                .timer(PERMISSION_JUDGE_TIMEOUT)
                .fuse()
        );
        let verdict = futures::select_biased! {
            response = completion => match response {
                Ok(response) => parse_permission_judge_response(&response),
                Err(error) => {
                    log::warn!("permission judge request failed: {error:#}");
                    PermissionJudgeVerdict::Unsafe {
                        reason: PERMISSION_JUDGE_ERROR_REASON.to_string(),
                    }
                }
            },
            _ = timeout => PermissionJudgeVerdict::Unsafe {
                reason: PERMISSION_JUDGE_TIMEOUT_REASON.to_string(),
            },
        };

        log::info!(
            "permission judge: tool={} verdict={} model={} elapsed={}ms",
            judge_context.tool_name,
            verdict.log_word(),
            model_id,
            started_at.elapsed().as_millis()
        );
        verdict
    }

    /// Prompts the user for authorization.
    ///
    /// When `check_settings` is `Some`, this gate is settings-driven: the
    /// settings are evaluated up-front (an Allow or Deny result resolves the
    /// task immediately without prompting), and while a prompt is pending a
    /// `SettingsStore` subscription watches for changes. A subsequent Allow
    /// or Deny dismisses the prompt UI and resolves the task without user
    /// interaction.
    ///
    /// When `check_settings` is `None`, the user is always prompted and
    /// settings changes are ignored. This suits prompts that aren't
    /// settings-driven (e.g. symlink-escape confirmations).
    fn run_authorization_loop(
        &self,
        title: String,
        options: acp_thread::PermissionOptions,
        context: Option<ToolPermissionContext>,
        check_settings: Option<Box<dyn Fn(&App) -> ToolPermissionDecision>>,
        judge_context: Option<PermissionJudgeContext>,
        cx: &mut App,
    ) -> Task<Result<()>> {
        // Short-circuit when current settings yield a definitive answer.
        let mut needs_judge = false;
        if let Some(check) = check_settings.as_ref() {
            match check(cx) {
                ToolPermissionDecision::Allow => return Task::ready(Ok(())),
                ToolPermissionDecision::Deny(reason) => {
                    return Task::ready(Err(anyhow!(reason)));
                }
                ToolPermissionDecision::Confirm => {}
                ToolPermissionDecision::Judge => needs_judge = true,
            }
        }

        let fs = self.fs.clone();
        let stream = self.stream.clone();
        let tool_call_id = self.tool_call_id.clone();
        let thread = self.thread.clone();
        let judge_recorder = needs_judge.then(|| self.permission_judge_recorder());
        let auto_resolution_outcomes = if check_settings.is_some() {
            match (
                auto_resolve_permission_outcome(&options, true),
                auto_resolve_permission_outcome(&options, false),
            ) {
                (Ok(allow), Ok(deny)) => Some((allow, deny)),
                (Err(error), _) | (_, Err(error)) => return Task::ready(Err(error)),
            }
        } else {
            None
        };
        cx.spawn(async move |cx| {
            // The settings watch is armed before the judge runs, so a settings
            // change during judgement is seen by the same `check_settings` path
            // that resolves a pending prompt.
            let (mut settings_tx, mut settings_rx) = watch::channel(());
            let _settings_subscription = check_settings.as_ref().map(|_| {
                cx.update(|cx| {
                    cx.observe_global::<SettingsStore>(move |_cx| {
                        settings_tx.send(()).ok();
                    })
                })
            });

            if let Some(judge_recorder) = judge_recorder.as_ref() {
                let Some(check) = check_settings.as_ref() else {
                    return Err(anyhow!(
                        "the permission judge can only run for a settings-driven authorization"
                    ));
                };
                if let Some(result) = Self::run_permission_judge(
                    judge_recorder,
                    judge_context,
                    &thread,
                    check.as_ref(),
                    &mut settings_rx,
                    cx,
                )
                .await
                {
                    return result;
                }
            }

            let (response_tx, mut response_rx) = oneshot::channel();
            if let Err(error) = stream
                .0
                .unbounded_send(Ok(ThreadEvent::ToolCallAuthorization(
                    ToolCallAuthorization {
                        tool_call: acp::ToolCallUpdate::new(
                            tool_call_id.clone(),
                            acp::ToolCallUpdateFields::new().title(title),
                        ),
                        options,
                        response: response_tx,
                        context,
                        kind: acp_thread::AuthorizationKind::PermissionGrant,
                    },
                )))
            {
                log::error!("Failed to send tool call authorization: {error}");
                return Err(anyhow!("Failed to send tool call authorization: {error}"));
            }

            let Some(check_settings) = check_settings else {
                let outcome = response_rx
                    .await
                    .map_err(|_| anyhow!("authorization channel closed"))?;
                ensure_tool_call_authorization_not_interrupted(&outcome)?;

                return Self::persist_permission_outcome(&outcome, fs, cx);
            };
            let Some((auto_allow_outcome, auto_deny_outcome)) = auto_resolution_outcomes else {
                return Err(anyhow!("missing auto-resolution outcomes"));
            };

            // Race the user's response against settings changes. On each
            // settings change, re-evaluate `check_settings`: if it now
            // yields a definitive Allow or Deny, resolve the prompt
            // without user interaction. Otherwise keep waiting on the
            // same prompt.
            loop {
                let settings_changed = async {
                    if settings_rx.changed().await.is_err() {
                        std::future::pending::<()>().await;
                    }
                };
                futures::select_biased! {
                    outcome = (&mut response_rx).fuse() => {
                        let outcome = outcome.map_err(|_| anyhow!("authorization channel closed"))?;
                        ensure_tool_call_authorization_not_interrupted(&outcome)?;
                        return Self::persist_permission_outcome(&outcome, fs.clone(), cx);
                    }
                    _ = settings_changed.fuse() => {
                        // On auto-resolve, we dismiss the prompt UI by
                        // resolving the tool call's `WaitingForConfirmation`
                        // status with an internal selected outcome. Dropping
                        // `response_rx` prevents the synthetic response from
                        // being delivered back into this loop.
                        match cx.update(|cx| check_settings(cx)) {
                            ToolPermissionDecision::Allow => {
                                drop(response_rx);
                                stream.resolve_tool_call_authorization(
                                    &tool_call_id,
                                    auto_allow_outcome.clone(),
                                );
                                return Ok(());
                            }
                            ToolPermissionDecision::Deny(reason) => {
                                drop(response_rx);
                                stream.resolve_tool_call_authorization(
                                    &tool_call_id,
                                    auto_deny_outcome.clone(),
                                );
                                return Err(anyhow!(reason));
                            }
                            // Switching into "AI が判定" while a prompt is up
                            // leaves the prompt up: re-judging on every edit of
                            // settings.json would be a burst of model calls, and
                            // the user is already looking at the decision.
                            ToolPermissionDecision::Confirm
                            | ToolPermissionDecision::Judge => continue,
                        }
                    }
                }
            }
        })
    }

    /// Interprets a `SelectedPermissionOutcome` and persists any settings changes.
    /// Returns `true` if the tool call should be allowed, `false` if denied.
    fn persist_permission_outcome(
        outcome: &acp_thread::SelectedPermissionOutcome,
        fs: Option<Arc<dyn Fs>>,
        cx: &AsyncApp,
    ) -> Result<()> {
        let option_id = outcome.option_id.0.as_ref();
        let err = || Err(anyhow!("Permission to run tool denied by user"));

        let always_permission = option_id
            .strip_prefix("always_allow:")
            .map(|tool| (tool, ToolPermissionMode::Allow))
            .or_else(|| {
                option_id
                    .strip_prefix("always_deny:")
                    .map(|tool| (tool, ToolPermissionMode::Deny))
            })
            .or_else(|| {
                option_id
                    .strip_prefix("always_allow_mcp:")
                    .map(|tool| (tool, ToolPermissionMode::Allow))
            })
            .or_else(|| {
                option_id
                    .strip_prefix("always_deny_mcp:")
                    .map(|tool| (tool, ToolPermissionMode::Deny))
            });

        if let Some((tool, mode)) = always_permission {
            let params = outcome.params.as_ref();
            Self::persist_always_permission(tool, mode, params, fs, cx);
            return if mode == ToolPermissionMode::Allow {
                Ok(())
            } else {
                err()
            };
        }

        // Handle simple "allow" / "deny" (once, no persistence)
        if option_id == "allow" || option_id == "deny" {
            debug_assert!(
                outcome.params.is_none(),
                "unexpected params for once-only permission"
            );
            return if option_id == "allow" { Ok(()) } else { err() };
        }

        debug_assert!(false, "unexpected permission option_id: {option_id}");

        err()
    }

    /// Persists an "always allow" or "always deny" permission, using sub_patterns
    /// from params when present.
    fn persist_always_permission(
        tool: &str,
        mode: ToolPermissionMode,
        params: Option<&acp_thread::SelectedPermissionParams>,
        fs: Option<Arc<dyn Fs>>,
        cx: &AsyncApp,
    ) {
        let Some(fs) = fs else {
            return;
        };

        match params {
            Some(acp_thread::SelectedPermissionParams::Terminal {
                patterns: sub_patterns,
            }) => {
                debug_assert!(
                    !sub_patterns.is_empty(),
                    "empty sub_patterns for tool {tool} — callers should pass None instead"
                );
                let tool = tool.to_string();
                let sub_patterns = sub_patterns.clone();
                cx.update(|cx| {
                    update_settings_file(fs, cx, move |settings, _| {
                        let agent = settings.agent.get_or_insert_default();
                        for pattern in sub_patterns {
                            match mode {
                                ToolPermissionMode::Allow => {
                                    agent.add_tool_allow_pattern(&tool, pattern);
                                }
                                ToolPermissionMode::Deny => {
                                    agent.add_tool_deny_pattern(&tool, pattern);
                                }
                                // If there's no matching pattern this will
                                // default to confirm (or to the judge), so
                                // falling through is fine here. An "always"
                                // choice never writes a judged default: the
                                // user already made the call themselves.
                                ToolPermissionMode::Confirm | ToolPermissionMode::Auto => (),
                            }
                        }
                    });
                });
            }
            None => {
                let tool = tool.to_string();
                cx.update(|cx| {
                    update_settings_file(fs, cx, move |settings, _| {
                        settings
                            .agent
                            .get_or_insert_default()
                            .set_tool_default_permission(&tool, mode);
                    });
                });
            }
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
pub struct ToolCallEventStreamReceiver(mpsc::UnboundedReceiver<Result<ThreadEvent>>);

#[cfg(any(test, feature = "test-support"))]
impl ToolCallEventStreamReceiver {
    /// Panics if any event delivered so far is a permission card. The other
    /// events (field updates, diffs) are expected and skipped, so a test can
    /// assert that a tool ran without asking.
    #[track_caller]
    pub fn expect_no_authorization(&mut self) {
        while let Ok(event) = self.0.try_recv() {
            if let Ok(ThreadEvent::ToolCallAuthorization(authorization)) = event {
                panic!(
                    "確認カードが出た: {:?}",
                    authorization.tool_call.fields.title
                );
            }
        }
    }

    pub async fn expect_authorization(&mut self) -> ToolCallAuthorization {
        let event = self.0.next().await;
        if let Some(Ok(ThreadEvent::ToolCallAuthorization(auth))) = event {
            auth
        } else {
            panic!("Expected ToolCallAuthorization but got: {:?}", event);
        }
    }

    pub async fn expect_elicitation(&mut self) -> ElicitationRequest {
        let event = self.0.next().await;
        if let Some(Ok(ThreadEvent::Elicitation(request))) = event {
            request
        } else {
            panic!("Expected Elicitation but got: {:?}", event);
        }
    }

    pub async fn expect_update_fields(&mut self) -> acp::ToolCallUpdateFields {
        let event = self.0.next().await;
        if let Some(Ok(ThreadEvent::ToolCallUpdate(acp_thread::ToolCallUpdate::UpdateFields(
            update,
        )))) = event
        {
            update.fields
        } else {
            panic!("Expected update fields but got: {:?}", event);
        }
    }

    /// The permission judge state recorded on the tool call (ADR-F52).
    pub async fn expect_permission_judge(&mut self) -> acp_thread::PermissionJudgeDetails {
        let event = self.0.next().await;
        if let Some(Ok(ThreadEvent::ToolCallUpdate(acp_thread::ToolCallUpdate::UpdateFields(
            update,
        )))) = &event
            && let Some(details) = acp_thread::permission_judge_from_meta(&update.meta)
        {
            return details;
        }
        panic!("Expected a permission judge update but got: {:?}", event);
    }

    /// The next permission judge update, if one is already queued. Used to
    /// assert that no *further* update follows a verdict.
    pub fn try_next_permission_judge(&mut self) -> Option<acp_thread::PermissionJudgeDetails> {
        while let Ok(event) = self.0.try_recv() {
            if let Ok(ThreadEvent::ToolCallUpdate(acp_thread::ToolCallUpdate::UpdateFields(update))) =
                &event
                && let Some(details) = acp_thread::permission_judge_from_meta(&update.meta)
            {
                return Some(details);
            }
        }
        None
    }

    pub async fn expect_authorization_resolved(
        &mut self,
    ) -> (acp::ToolCallId, acp_thread::SelectedPermissionOutcome) {
        let event = self.0.next().await;
        if let Some(Ok(ThreadEvent::ToolCallAuthorizationResolved {
            tool_call_id,
            outcome,
        })) = event
        {
            (tool_call_id, outcome)
        } else {
            panic!("Expected authorization resolved but got: {:?}", event);
        }
    }

    pub async fn expect_diff(&mut self) -> Entity<acp_thread::Diff> {
        let event = self.0.next().await;
        if let Some(Ok(ThreadEvent::ToolCallUpdate(acp_thread::ToolCallUpdate::UpdateDiff(
            update,
        )))) = event
        {
            update.diff
        } else {
            panic!("Expected diff but got: {:?}", event);
        }
    }

    pub async fn expect_terminal(&mut self) -> Entity<acp_thread::Terminal> {
        let event = self.0.next().await;
        if let Some(Ok(ThreadEvent::ToolCallUpdate(acp_thread::ToolCallUpdate::UpdateTerminal(
            update,
        )))) = event
        {
            update.terminal
        } else {
            panic!("Expected terminal but got: {:?}", event);
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
impl std::ops::Deref for ToolCallEventStreamReceiver {
    type Target = mpsc::UnboundedReceiver<Result<ThreadEvent>>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[cfg(any(test, feature = "test-support"))]
impl std::ops::DerefMut for ToolCallEventStreamReceiver {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl From<&str> for UserMessageContent {
    fn from(text: &str) -> Self {
        Self::Text(text.into())
    }
}

impl From<String> for UserMessageContent {
    fn from(text: String) -> Self {
        Self::Text(text)
    }
}

impl UserMessageContent {
    pub fn from_content_block(value: acp::ContentBlock, path_style: PathStyle) -> Self {
        match value {
            acp::ContentBlock::Text(text_content) => Self::Text(text_content.text),
            acp::ContentBlock::Image(image_content) => Self::Image(convert_image(image_content)),
            acp::ContentBlock::Audio(_) => {
                // TODO
                Self::Text("[audio]".to_string())
            }
            acp::ContentBlock::ResourceLink(resource_link) => {
                match MentionUri::parse(&resource_link.uri, path_style) {
                    Ok(uri) => Self::Mention {
                        uri,
                        content: SharedString::default(),
                    },
                    Err(err) => {
                        log::error!("Failed to parse mention link: {}", err);
                        Self::Text(format!("[{}]({})", resource_link.name, resource_link.uri))
                    }
                }
            }
            acp::ContentBlock::Resource(resource) => match resource.resource {
                acp::EmbeddedResourceResource::TextResourceContents(resource) => {
                    match MentionUri::parse(&resource.uri, path_style) {
                        Ok(uri) => Self::Mention {
                            uri,
                            content: resource.text.into(),
                        },
                        Err(err) => {
                            log::error!("Failed to parse mention link: {}", err);
                            Self::Text(
                                MarkdownCodeBlock {
                                    tag: &resource.uri,
                                    text: &resource.text,
                                }
                                .to_string(),
                            )
                        }
                    }
                }
                acp::EmbeddedResourceResource::BlobResourceContents(_) => {
                    // TODO
                    Self::Text("[blob]".to_string())
                }
                other => {
                    log::warn!("Unexpected content type: {:?}", other);
                    Self::Text("[unknown]".to_string())
                }
            },
            other => {
                log::warn!("Unexpected content type: {:?}", other);
                Self::Text("[unknown]".to_string())
            }
        }
    }
}

impl From<UserMessageContent> for acp::ContentBlock {
    fn from(content: UserMessageContent) -> Self {
        match content {
            UserMessageContent::Text(text) => text.into(),
            UserMessageContent::Image(image) => {
                acp::ContentBlock::Image(acp::ImageContent::new(image.source, "image/png"))
            }
            UserMessageContent::Mention { uri, content } => acp::ContentBlock::Resource(
                acp::EmbeddedResource::new(acp::EmbeddedResourceResource::TextResourceContents(
                    acp::TextResourceContents::new(content, uri.to_uri().to_string()),
                )),
            ),
        }
    }
}

fn convert_image(image_content: acp::ImageContent) -> LanguageModelImage {
    LanguageModelImage {
        source: image_content.data.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use language_model::LanguageModelToolUseId;
    use language_model::fake_provider::FakeLanguageModel;
    use serde_json::json;
    use settings::LanguageModelProviderSetting;
    use std::sync::Arc;

    async fn setup_thread_for_test(cx: &mut TestAppContext) -> (Entity<Thread>, ThreadEventStream) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);

            LanguageModelRegistry::test(cx);
        });

        let fs = fs::FakeFs::new(cx.background_executor.clone());
        let templates = Templates::new();
        let project = Project::test(fs.clone(), [], cx).await;

        cx.update(|cx| {
            let project_context = cx.new(|_cx| prompt_store::ProjectContext::default());
            let context_server_store = project.read(cx).context_server_store();
            let context_server_registry =
                cx.new(|cx| ContextServerRegistry::new(context_server_store, cx));

            let thread = cx.new(|cx| {
                Thread::new(
                    project,
                    project_context,
                    context_server_registry,
                    templates,
                    None,
                    cx,
                )
            });

            let (event_tx, _event_rx) = mpsc::unbounded();
            let event_stream = ThreadEventStream(event_tx);

            (thread, event_stream)
        })
    }

    fn set_auto_compact_settings(cx: &mut App, auto_compact: agent_settings::AutoCompactSettings) {
        let mut settings = AgentSettings::get_global(cx).clone();
        settings.auto_compact = auto_compact;
        AgentSettings::override_global(settings, cx);
    }

    fn set_registry_compaction_model(cx: &mut App, model: Option<Arc<dyn LanguageModel>>) {
        use language_model::fake_provider::FakeLanguageModelProvider;
        use language_model::{ConfiguredModel, LanguageModelProvider};
        LanguageModelRegistry::global(cx).update(cx, |registry, cx| {
            let configured = model.map(|m| ConfiguredModel {
                provider: Arc::new(FakeLanguageModelProvider::default())
                    as Arc<dyn LanguageModelProvider>,
                model: m,
            });
            registry.set_compaction_model(configured, cx);
        });
    }

    #[test]
    fn test_reduced_attachments_keep_the_context_tags() {
        let file_path = if cfg!(windows) {
            "C:\\project\\big.rs"
        } else {
            "/project/big.rs"
        };
        let file_uri = MentionUri::File {
            abs_path: file_path.into(),
        };
        let outline = "# `big.rs` (900 lines, 40.0 KB). Its full content was not attached.\n\
                       fn main [L1-20]\n";
        let diff_uri = MentionUri::GitDiff {
            base_ref: "main".into(),
        };
        let diff_summary = "Changes since the merge base with `main`: 1 file changed, +1 -0.\n\n\
                            | File | + | - |\n| --- | ---: | ---: |\n| `big.rs` | 1 | 0 |\n";
        let message = UserMessage {
            id: ClientUserMessageId::new(),
            content: vec![
                UserMessageContent::Text("Look at ".into()),
                UserMessageContent::from_content_block(
                    acp::ContentBlock::Resource(acp::EmbeddedResource::new(
                        acp::EmbeddedResourceResource::TextResourceContents(
                            acp::TextResourceContents::new(outline, file_uri.to_uri().to_string()),
                        ),
                    )),
                    PathStyle::local(),
                ),
                UserMessageContent::from_content_block(
                    acp::ContentBlock::Resource(acp::EmbeddedResource::new(
                        acp::EmbeddedResourceResource::TextResourceContents(
                            acp::TextResourceContents::new(
                                diff_summary,
                                diff_uri.to_uri().to_string(),
                            ),
                        ),
                    )),
                    PathStyle::local(),
                ),
            ]
            .into(),
        };

        let request = message.to_request();
        let context = request
            .content
            .iter()
            .filter_map(|content| match content {
                language_model::MessageContent::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        let files_start = context.find("<files>").expect("a <files> section");
        let files_end = context
            .find("</files>")
            .expect("the <files> section is closed");
        assert!(
            context[files_start..files_end].contains("Its full content was not attached."),
            "{context}"
        );
        let diffs_start = context.find("<diffs>").expect("a <diffs> section");
        let diffs_end = context
            .find("</diffs>")
            .expect("the <diffs> section is closed");
        assert!(
            context[diffs_start..diffs_end].contains("Branch diff against main:"),
            "{context}"
        );
        assert!(
            context[diffs_start..diffs_end].contains("| `big.rs` | 1 | 0 |"),
            "{context}"
        );
        assert!(context.contains("<context>\n"), "{context}");
        assert!(context.ends_with("</context>"), "{context}");
    }

    #[test]
    fn test_summary_compaction_renders_for_request_and_markdown() {
        let message = Message::Compaction(CompactionInfo::Summary("Older context".into()));

        assert_eq!(message.role(), Role::User);
        assert_eq!(message.to_markdown(), "--- Context Compacted ---\n");

        let request_messages = message.to_request();
        assert_eq!(request_messages.len(), 1);
        assert_eq!(request_messages[0].role, Role::User);
        assert!(!request_messages[0].cache);
        assert_eq!(request_messages[0].reasoning_details, None);
        assert_eq!(request_messages[0].content.len(), 1);
        let language_model::MessageContent::Text(text) = &request_messages[0].content[0] else {
            panic!("expected text summary context");
        };
        assert_eq!(
            text.as_str(),
            "The previous conversation was compacted. Use this summary as context:\n\nOlder context"
        );
    }

    fn user_text_message(id: ClientUserMessageId, text: &str) -> Arc<Message> {
        Arc::new(Message::User(UserMessage {
            id,
            content: vec![UserMessageContent::Text(text.to_string())].into(),
        }))
    }

    fn agent_text_message(text: &str) -> Arc<Message> {
        Arc::new(Message::Agent(AgentMessage {
            content: vec![AgentMessageContent::Text(text.to_string())],
            ..Default::default()
        }))
    }

    fn summary_compaction(summary: &str) -> Arc<Message> {
        Arc::new(Message::Compaction(CompactionInfo::Summary(summary.into())))
    }

    fn summary_request_text(summary: &str) -> String {
        format!(
            "The previous conversation was compacted. Use this summary as context:\n\n{summary}"
        )
    }

    fn request_texts_after_system(messages: &[LanguageModelRequestMessage]) -> Vec<String> {
        messages
            .iter()
            .skip(1)
            .map(LanguageModelRequestMessage::string_contents)
            .collect()
    }

    fn request_texts(messages: &[LanguageModelRequestMessage]) -> Vec<String> {
        messages
            .iter()
            .map(LanguageModelRequestMessage::string_contents)
            .collect()
    }

    #[gpui::test]
    async fn test_thread_summary_request_uses_compacted_history(cx: &mut TestAppContext) {
        let (thread, _event_stream) = setup_thread_for_test(cx).await;
        let summary_model = Arc::new(FakeLanguageModel::default());

        let summary_task = cx.update(|cx| {
            thread.update(cx, |thread, cx| {
                thread.set_summarization_model(Some(summary_model.clone()), cx);
                thread
                    .messages
                    .push(user_text_message(ClientUserMessageId::new(), "old user"));
                thread.messages.push(agent_text_message("old assistant"));
                thread.messages.push(summary_compaction("first summary"));
                thread.messages.push(user_text_message(
                    ClientUserMessageId::new(),
                    "between user",
                ));
                thread
                    .messages
                    .push(agent_text_message("between assistant"));
                thread.messages.push(summary_compaction("latest summary"));
                thread
                    .messages
                    .push(user_text_message(ClientUserMessageId::new(), "after user"));
                thread.messages.push(agent_text_message("after assistant"));

                thread.summary(cx)
            })
        });
        cx.run_until_parked();

        let summary_request = summary_model.pending_completions().pop().unwrap();
        assert_eq!(
            summary_request.intent,
            Some(CompletionIntent::ThreadContextSummarization)
        );
        assert_eq!(
            request_texts(&summary_request.messages),
            vec![
                "old user".to_string(),
                "between user".to_string(),
                summary_request_text("latest summary"),
                "after user".to_string(),
                "after assistant".to_string(),
                SUMMARIZE_THREAD_DETAILED_PROMPT.to_string(),
            ]
        );

        summary_model.send_completion_stream_text_chunk(&summary_request, "thread summary");
        summary_model.end_completion_stream(&summary_request);
        assert_eq!(summary_task.await.as_deref(), Some("thread summary"));
    }

    #[test]
    fn test_thread_title_request_uses_compacted_history() {
        let messages = vec![
            user_text_message(ClientUserMessageId::new(), "old user"),
            agent_text_message("old assistant"),
            summary_compaction("first summary"),
            user_text_message(ClientUserMessageId::new(), "between user"),
            agent_text_message("between assistant"),
            summary_compaction("latest summary"),
            user_text_message(ClientUserMessageId::new(), "after user"),
            agent_text_message("after assistant"),
        ];

        let request = build_thread_title_request(&messages, Some(0.2));

        assert_eq!(request.intent, Some(CompletionIntent::ThreadSummarization));
        assert_eq!(request.temperature, Some(0.2));
        assert_eq!(
            request_texts(&request.messages),
            vec![
                "old user".to_string(),
                "between user".to_string(),
                summary_request_text("latest summary"),
                "after user".to_string(),
                "after assistant".to_string(),
                SUMMARIZE_THREAD_PROMPT.to_string(),
            ]
        );
    }

    #[gpui::test]
    async fn test_compaction_threshold_uses_percentage_setting(cx: &mut TestAppContext) {
        let (thread, _event_stream) = setup_thread_for_test(cx).await;
        let model = Arc::new(FakeLanguageModel::default());
        let user_message_id = ClientUserMessageId::new();

        cx.update(|cx| {
            thread.update(cx, |thread, cx| {
                thread.set_model(model, cx);
                thread
                    .messages
                    .push(user_text_message(user_message_id.clone(), "below limit"));
                thread.request_token_usage.insert(
                    user_message_id.clone(),
                    language_model::TokenUsage {
                        input_tokens: 899_999,
                        ..Default::default()
                    },
                );

                assert_eq!(thread.compaction_message_target_ix(cx), None);

                thread.request_token_usage.insert(
                    user_message_id.clone(),
                    language_model::TokenUsage {
                        input_tokens: 900_000,
                        ..Default::default()
                    },
                );

                assert_eq!(thread.compaction_message_target_ix(cx), Some(1));
            });
        });
    }

    #[gpui::test]
    async fn test_compaction_threshold_accounts_for_max_output_tokens(cx: &mut TestAppContext) {
        let (thread, _event_stream) = setup_thread_for_test(cx).await;
        let model = Arc::new(FakeLanguageModel::default());
        model.set_max_output_tokens(Some(32_000));
        let user_message_id = ClientUserMessageId::new();

        cx.update(|cx| {
            thread.update(cx, |thread, cx| {
                thread.set_model(model, cx);
                thread.messages.push(user_text_message(
                    user_message_id.clone(),
                    "near input limit",
                ));
                thread.request_token_usage.insert(
                    user_message_id.clone(),
                    language_model::TokenUsage {
                        input_tokens: 871_199,
                        ..Default::default()
                    },
                );

                assert_eq!(thread.compaction_message_target_ix(cx), None);

                thread.request_token_usage.insert(
                    user_message_id.clone(),
                    language_model::TokenUsage {
                        input_tokens: 871_200,
                        ..Default::default()
                    },
                );

                assert_eq!(thread.compaction_message_target_ix(cx), Some(1));

                set_auto_compact_settings(
                    cx,
                    agent_settings::AutoCompactSettings {
                        enabled: true,
                        threshold: AutoCompactThreshold::TokensRemaining(20_000),
                    },
                );
                thread.request_token_usage.insert(
                    user_message_id.clone(),
                    language_model::TokenUsage {
                        input_tokens: 948_000,
                        ..Default::default()
                    },
                );

                assert_eq!(thread.compaction_message_target_ix(cx), None);

                thread.request_token_usage.insert(
                    user_message_id.clone(),
                    language_model::TokenUsage {
                        input_tokens: 948_001,
                        ..Default::default()
                    },
                );

                assert_eq!(thread.compaction_message_target_ix(cx), Some(1));
            });
        });
    }

    #[gpui::test]
    async fn test_compaction_threshold_respects_enabled_setting(cx: &mut TestAppContext) {
        let (thread, _event_stream) = setup_thread_for_test(cx).await;
        let model = Arc::new(FakeLanguageModel::default());
        let user_message_id = ClientUserMessageId::new();

        cx.update(|cx| {
            set_auto_compact_settings(
                cx,
                agent_settings::AutoCompactSettings {
                    enabled: false,
                    threshold: AutoCompactThreshold::Percentage(0.9),
                },
            );
            thread.update(cx, |thread, cx| {
                thread.set_model(model, cx);
                thread
                    .messages
                    .push(user_text_message(user_message_id.clone(), "near limit"));
                thread.request_token_usage.insert(
                    user_message_id.clone(),
                    language_model::TokenUsage {
                        input_tokens: 960_000,
                        ..Default::default()
                    },
                );

                assert_eq!(thread.compaction_message_target_ix(cx), None);
            });
        });
    }

    #[gpui::test]
    async fn test_compaction_threshold_respects_token_settings(cx: &mut TestAppContext) {
        let (thread, _event_stream) = setup_thread_for_test(cx).await;
        let model = Arc::new(FakeLanguageModel::default());
        let user_message_id = ClientUserMessageId::new();

        cx.update(|cx| {
            set_auto_compact_settings(
                cx,
                agent_settings::AutoCompactSettings {
                    enabled: true,
                    threshold: AutoCompactThreshold::TokensUsed(100_000),
                },
            );
            thread.update(cx, |thread, cx| {
                thread.set_model(model, cx);
                thread.messages.push(user_text_message(
                    user_message_id.clone(),
                    "fixed token limit",
                ));
                thread.request_token_usage.insert(
                    user_message_id.clone(),
                    language_model::TokenUsage {
                        input_tokens: 99_999,
                        ..Default::default()
                    },
                );

                assert_eq!(thread.compaction_message_target_ix(cx), None);

                thread.request_token_usage.insert(
                    user_message_id.clone(),
                    language_model::TokenUsage {
                        input_tokens: 100_000,
                        ..Default::default()
                    },
                );

                assert_eq!(thread.compaction_message_target_ix(cx), Some(1));

                set_auto_compact_settings(
                    cx,
                    agent_settings::AutoCompactSettings {
                        enabled: true,
                        threshold: AutoCompactThreshold::TokensRemaining(20_000),
                    },
                );
                thread.request_token_usage.insert(
                    user_message_id.clone(),
                    language_model::TokenUsage {
                        input_tokens: 980_000,
                        ..Default::default()
                    },
                );

                assert_eq!(thread.compaction_message_target_ix(cx), None);

                thread.request_token_usage.insert(
                    user_message_id.clone(),
                    language_model::TokenUsage {
                        input_tokens: 980_001,
                        ..Default::default()
                    },
                );

                assert_eq!(thread.compaction_message_target_ix(cx), Some(1));
            });
        });
    }

    #[gpui::test]
    async fn test_compaction_unavailable_for_small_context_window(cx: &mut TestAppContext) {
        let (thread, _event_stream) = setup_thread_for_test(cx).await;
        let model = Arc::new(FakeLanguageModel::default());
        // A context window below the minimum disables auto-compaction.
        model.set_max_token_count(agent_settings::DEFAULT_AUTO_COMPACT_MIN_CONTEXT_WINDOW - 1);
        let user_message_id = ClientUserMessageId::new();

        cx.update(|cx| {
            thread.update(cx, |thread, cx| {
                thread.set_model(model, cx);
                thread
                    .messages
                    .push(user_text_message(user_message_id.clone(), "near limit"));
                thread.request_token_usage.insert(
                    user_message_id.clone(),
                    language_model::TokenUsage {
                        input_tokens: u64::MAX,
                        ..Default::default()
                    },
                );

                assert_eq!(thread.compaction_message_target_ix(cx), None);
            });
        });
    }

    #[gpui::test]
    async fn test_compaction_inserts_before_new_user_and_requests_compacted_window(
        cx: &mut TestAppContext,
    ) {
        let (thread, _event_stream) = setup_thread_for_test(cx).await;
        let model = Arc::new(FakeLanguageModel::default());
        let old_user_message_id = ClientUserMessageId::new();
        let new_user_message_id = ClientUserMessageId::new();

        cx.update(|cx| {
            thread.update(cx, |thread, cx| {
                thread.set_model(model.clone(), cx);
                thread
                    .messages
                    .push(user_text_message(old_user_message_id.clone(), "old user"));
                thread.messages.push(agent_text_message("old assistant"));
                thread.request_token_usage.insert(
                    old_user_message_id.clone(),
                    language_model::TokenUsage {
                        input_tokens: 960_000,
                        ..Default::default()
                    },
                );
            });
        });

        let _events = cx
            .update(|cx| {
                thread.update(cx, |thread, cx| {
                    thread.send(new_user_message_id, vec!["new prompt"], cx)
                })
            })
            .unwrap();
        cx.run_until_parked();

        let compaction_request = model.pending_completions().pop().unwrap();
        assert_eq!(
            compaction_request.intent,
            Some(CompletionIntent::ThreadContextSummarization)
        );
        let compaction_texts = request_texts_after_system(&compaction_request.messages);
        assert_eq!(compaction_texts.len(), 3);
        assert_eq!(compaction_texts[0], "old user");
        assert_eq!(compaction_texts[1], "old assistant");
        assert_eq!(compaction_texts[2], COMPACTION_PROMPT);

        model.send_completion_stream_text_chunk(&compaction_request, "compacted old context");
        model.end_completion_stream(&compaction_request);
        cx.run_until_parked();

        let final_request = model.pending_completions().pop().unwrap();
        assert_eq!(final_request.intent, Some(CompletionIntent::UserPrompt));
        assert_eq!(
            request_texts_after_system(&final_request.messages),
            vec![
                "old user".to_string(),
                summary_request_text("compacted old context"),
                "new prompt".to_string(),
            ]
        );

        model.send_completion_stream_text_chunk(&final_request, "answer");
        model.end_completion_stream(&final_request);
        cx.run_until_parked();

        cx.update(|cx| {
            thread.read_with(cx, |thread, _cx| {
                assert!(matches!(&*thread.messages[0], Message::User(_)));
                assert!(matches!(&*thread.messages[1], Message::Agent(_)));
                assert!(matches!(
                    &*thread.messages[2],
                    Message::Compaction(CompactionInfo::Summary(summary)) if summary.as_ref() == "compacted old context"
                ));
                assert!(matches!(&*thread.messages[3], Message::User(_)));
            });
        });
    }

    #[gpui::test]
    async fn test_manual_compact_forces_summary(cx: &mut TestAppContext) {
        let (thread, _event_stream) = setup_thread_for_test(cx).await;
        let model = Arc::new(FakeLanguageModel::default());
        // A context window below the minimum and no recorded token usage would
        // both disable *automatic* compaction. Manual compaction forces it anyway.
        model.set_max_token_count(agent_settings::DEFAULT_AUTO_COMPACT_MIN_CONTEXT_WINDOW - 1);
        let user_message_id = ClientUserMessageId::new();
        let compact_message_id = ClientUserMessageId::new();

        cx.update(|cx| {
            thread.update(cx, |thread, cx| {
                thread.set_model(model.clone(), cx);
                thread
                    .messages
                    .push(user_text_message(user_message_id.clone(), "old user"));
                thread.messages.push(agent_text_message("old assistant"));
                // Auto-compaction would be a no-op here.
                assert_eq!(thread.compaction_message_target_ix(cx), None);
            });
        });

        let _events = cx
            .update(|cx| {
                thread.update(cx, |thread, cx| {
                    thread.compact(compact_message_id.clone(), cx)
                })
            })
            .unwrap();
        cx.run_until_parked();

        let compaction_request = model.pending_completions().pop().unwrap();
        assert_eq!(
            compaction_request.intent,
            Some(CompletionIntent::ThreadContextSummarization)
        );
        let compaction_texts = request_texts_after_system(&compaction_request.messages);
        assert_eq!(compaction_texts.len(), 3);
        assert_eq!(compaction_texts[0], "old user");
        assert_eq!(compaction_texts[1], "old assistant");
        assert_eq!(compaction_texts[2], COMPACTION_PROMPT);

        model.send_completion_stream_text_chunk(&compaction_request, "summary of old context");
        model.end_completion_stream(&compaction_request);
        cx.run_until_parked();

        // The compaction summary is appended after a zero-content user message
        // marker, and no follow-up model turn is requested — `/compact` only
        // compacts.
        assert!(model.pending_completions().is_empty());
        cx.update(|cx| {
            thread.read_with(cx, |thread, _cx| {
                assert!(matches!(&*thread.messages[0], Message::User(_)));
                assert!(matches!(&*thread.messages[1], Message::Agent(_)));
                assert!(matches!(
                    &*thread.messages[2],
                    Message::User(UserMessage { id, content }) if id == &compact_message_id && content.is_empty()
                ));
                assert!(matches!(
                    &*thread.messages[3],
                    Message::Compaction(CompactionInfo::Summary(summary)) if summary.as_ref() == "summary of old context"
                ));
                // Re-running `/compact` with nothing new to summarize is a
                // no-op: the thread already ends in a compaction.
                assert_eq!(thread.forced_compaction_target_ix(), None);
            });

            thread
                .update(cx, |thread, cx| thread.truncate(compact_message_id.clone(), cx))
                .unwrap();

            thread.read_with(cx, |thread, _cx| {
                assert_eq!(thread.messages.len(), 2);
                assert!(matches!(&*thread.messages[0], Message::User(_)));
                assert!(matches!(&*thread.messages[1], Message::Agent(_)));
            });
        });
    }

    /// Cancelling an in-flight manual compaction must not leave the zero-content
    /// rewind marker (or a partial summary) dangling at the end of the thread.
    #[gpui::test]
    async fn test_manual_compact_cancelled_leaves_no_marker(cx: &mut TestAppContext) {
        let (thread, _event_stream) = setup_thread_for_test(cx).await;
        let model = Arc::new(FakeLanguageModel::default());

        cx.update(|cx| {
            thread.update(cx, |thread, cx| {
                thread.set_model(model.clone(), cx);
                thread
                    .messages
                    .push(user_text_message(ClientUserMessageId::new(), "old user"));
                thread.messages.push(agent_text_message("old assistant"));
            });
        });

        let _events = cx
            .update(|cx| {
                thread.update(cx, |thread, cx| {
                    thread.compact(ClientUserMessageId::new(), cx)
                })
            })
            .unwrap();
        cx.run_until_parked();
        // The compaction request is in flight but hasn't streamed a summary.
        assert_eq!(model.pending_completions().len(), 1);

        cx.update(|cx| thread.update(cx, |thread, cx| thread.cancel(cx)))
            .await;
        cx.run_until_parked();

        thread.read_with(cx, |thread, _cx| {
            assert_eq!(thread.messages.len(), 2);
            assert!(matches!(&*thread.messages[0], Message::User(_)));
            assert!(matches!(&*thread.messages[1], Message::Agent(_)));
        });
    }

    /// A failed compaction (here, an empty summary) reports an error and leaves
    /// the thread untouched — no marker, no compaction.
    #[gpui::test]
    async fn test_manual_compact_empty_summary_leaves_no_marker(cx: &mut TestAppContext) {
        let (thread, _event_stream) = setup_thread_for_test(cx).await;
        let model = Arc::new(FakeLanguageModel::default());

        cx.update(|cx| {
            thread.update(cx, |thread, cx| {
                thread.set_model(model.clone(), cx);
                thread
                    .messages
                    .push(user_text_message(ClientUserMessageId::new(), "old user"));
                thread.messages.push(agent_text_message("old assistant"));
            });
        });

        let mut events = cx
            .update(|cx| {
                thread.update(cx, |thread, cx| {
                    thread.compact(ClientUserMessageId::new(), cx)
                })
            })
            .unwrap();
        cx.run_until_parked();

        let request = model.pending_completions().pop().unwrap();
        // End the stream without emitting any summary text.
        model.end_completion_stream(&request);
        cx.run_until_parked();

        // An error is surfaced, and the thread is left exactly as it was. The
        // compaction task drops the event stream after failing, so the channel
        // closes and this drain terminates.
        let mut saw_error = false;
        while let Some(event) = events.next().await {
            if event.is_err() {
                saw_error = true;
            }
        }
        assert!(saw_error, "expected an error event for the empty summary");
        thread.read_with(cx, |thread, _cx| {
            assert_eq!(thread.messages.len(), 2);
            assert!(matches!(&*thread.messages[0], Message::User(_)));
            assert!(matches!(&*thread.messages[1], Message::Agent(_)));
        });
    }

    /// `/compact` on an empty thread (nothing to summarize) is a no-op: it
    /// issues no model request and adds no marker.
    #[gpui::test]
    async fn test_manual_compact_noop_on_empty_thread(cx: &mut TestAppContext) {
        let (thread, _event_stream) = setup_thread_for_test(cx).await;
        let model = Arc::new(FakeLanguageModel::default());
        cx.update(|cx| thread.update(cx, |thread, cx| thread.set_model(model.clone(), cx)));

        let _events = cx
            .update(|cx| {
                thread.update(cx, |thread, cx| {
                    thread.compact(ClientUserMessageId::new(), cx)
                })
            })
            .unwrap();
        cx.run_until_parked();

        assert!(model.pending_completions().is_empty());
        thread.read_with(cx, |thread, _cx| {
            assert!(thread.messages.is_empty());
        });
    }

    /// The zero-content marker replays as an empty user message, which the UI
    /// drops (it renders content blocks, of which there are none), so reloading
    /// a compacted thread doesn't surface an empty `/compact` bubble.
    #[gpui::test]
    async fn test_manual_compact_marker_replays_as_empty_user_message(cx: &mut TestAppContext) {
        let (thread, _event_stream) = setup_thread_for_test(cx).await;
        let marker_id = ClientUserMessageId::new();

        let mut replay_events = cx.update(|cx| {
            thread.update(cx, |thread, cx| {
                thread
                    .messages
                    .push(user_text_message(ClientUserMessageId::new(), "before"));
                thread.messages.push(agent_text_message("answer"));
                thread.messages.push(Arc::new(Message::User(UserMessage {
                    id: marker_id.clone(),
                    content: Arc::from([]),
                })));
                thread.messages.push(summary_compaction("summary"));
                thread.replay(cx)
            })
        });

        // Skip the leading "before"/"answer" replay events.
        let _ = replay_events.next().await;
        let _ = replay_events.next().await;

        let event = replay_events.next().await;
        match event {
            Some(Ok(ThreadEvent::UserMessage(message))) => {
                assert_eq!(message.id, marker_id);
                assert!(
                    message.content.is_empty(),
                    "marker should replay with no content so the UI renders nothing"
                );
            }
            _ => panic!("expected the marker to replay as a user message, got {event:?}"),
        }

        let event = replay_events.next().await;
        assert!(
            matches!(&event, Some(Ok(ThreadEvent::ContextCompaction(_)))),
            "expected the compaction to replay after the marker, got {event:?}"
        );
    }

    /// When `agent.compaction_model` is configured, manual `/compact` streams
    /// to the configured model rather than the thread's primary model.
    #[gpui::test]
    async fn test_compaction_uses_configured_compaction_model(cx: &mut TestAppContext) {
        let (thread, _event_stream) = setup_thread_for_test(cx).await;
        let thread_model = Arc::new(FakeLanguageModel::default());
        let compaction_model = Arc::new(FakeLanguageModel::default());

        cx.update(|cx| {
            thread.update(cx, |thread, cx| {
                thread.set_model(thread_model.clone(), cx);
                thread
                    .messages
                    .push(user_text_message(ClientUserMessageId::new(), "old user"));
                thread.messages.push(agent_text_message("old assistant"));
            });
            set_registry_compaction_model(cx, Some(compaction_model.clone()));
        });

        let _events = cx
            .update(|cx| {
                thread.update(cx, |thread, cx| {
                    thread.compact(ClientUserMessageId::new(), cx)
                })
            })
            .unwrap();
        cx.run_until_parked();

        assert_eq!(
            thread_model.pending_completions().len(),
            0,
            "thread's primary model should not have been used for compaction"
        );
        let request = compaction_model
            .pending_completions()
            .pop()
            .expect("compaction model should have received the request");
        assert_eq!(
            request.intent,
            Some(CompletionIntent::ThreadContextSummarization)
        );

        thread.read_with(cx, |thread, _cx| {
            let telemetry = thread
                .pending_compaction_telemetry
                .as_ref()
                .expect("pending telemetry");
            assert_eq!(telemetry.model, compaction_model.telemetry_id());
        });

        compaction_model.send_completion_stream_text_chunk(&request, "summary");
        compaction_model.end_completion_stream(&request);
        cx.run_until_parked();
    }

    /// When `agent.compaction_model` is configured but doesn't resolve (e.g.
    /// the provider isn't registered), manual `/compact` falls back to the
    /// thread's primary model and the telemetry reflects the actual stream
    /// model — not the one the user tried to configure.
    #[gpui::test]
    async fn test_compaction_falls_back_when_compaction_model_unavailable(cx: &mut TestAppContext) {
        let (thread, _event_stream) = setup_thread_for_test(cx).await;
        let thread_model = Arc::new(FakeLanguageModel::default());

        cx.update(|cx| {
            thread.update(cx, |thread, cx| {
                thread.set_model(thread_model.clone(), cx);
                thread
                    .messages
                    .push(user_text_message(ClientUserMessageId::new(), "old user"));
                thread.messages.push(agent_text_message("old assistant"));
            });
            // Settings say "configured"; registry says "couldn't resolve".
            let mut settings = AgentSettings::get_global(cx).clone();
            settings.compaction_model = Some(LanguageModelSelection {
                provider: LanguageModelProviderSetting("missing".into()),
                model: "missing-model".into(),
                enable_thinking: false,
                effort: None,
                speed: None,
            });
            AgentSettings::override_global(settings, cx);
            set_registry_compaction_model(cx, None);
        });

        let _events = cx
            .update(|cx| {
                thread.update(cx, |thread, cx| {
                    thread.compact(ClientUserMessageId::new(), cx)
                })
            })
            .unwrap();
        cx.run_until_parked();

        let request = thread_model
            .pending_completions()
            .pop()
            .expect("thread model should have received the fallback request");
        assert_eq!(
            request.intent,
            Some(CompletionIntent::ThreadContextSummarization)
        );

        thread.read_with(cx, |thread, _cx| {
            let telemetry = thread
                .pending_compaction_telemetry
                .as_ref()
                .expect("pending telemetry");
            assert_eq!(telemetry.model, thread_model.telemetry_id());
        });

        thread_model.send_completion_stream_text_chunk(&request, "summary");
        thread_model.end_completion_stream(&request);
        cx.run_until_parked();
    }

    /// Auto-compaction triggered by the threshold also honors
    /// `agent.compaction_model`.
    #[gpui::test]
    async fn test_auto_compaction_uses_compaction_model(cx: &mut TestAppContext) {
        let (thread, _event_stream) = setup_thread_for_test(cx).await;
        let thread_model = Arc::new(FakeLanguageModel::default());
        let compaction_model = Arc::new(FakeLanguageModel::default());
        let old_user_message_id = ClientUserMessageId::new();

        cx.update(|cx| {
            thread.update(cx, |thread, cx| {
                thread.set_model(thread_model.clone(), cx);
                thread
                    .messages
                    .push(user_text_message(old_user_message_id.clone(), "old user"));
                thread.messages.push(agent_text_message("old assistant"));
                thread.request_token_usage.insert(
                    old_user_message_id.clone(),
                    TokenUsage {
                        input_tokens: u64::MAX,
                        ..Default::default()
                    },
                );
            });
            set_auto_compact_settings(
                cx,
                agent_settings::AutoCompactSettings {
                    enabled: true,
                    threshold: agent_settings::AutoCompactThreshold::Percentage(0.5),
                },
            );
            set_registry_compaction_model(cx, Some(compaction_model.clone()));
        });

        // The auto-compact gate fires inside `run_turn` when we kick off a
        // new user message. Drive a `send` so `perform_compaction_if_needed`
        // runs.
        let _events = cx
            .update(|cx| {
                thread.update(cx, |thread, cx| {
                    thread.send(ClientUserMessageId::new(), vec!["new prompt"], cx)
                })
            })
            .unwrap();
        cx.run_until_parked();

        assert_eq!(thread_model.pending_completions().len(), 0);
        let request = compaction_model
            .pending_completions()
            .pop()
            .expect("compaction model should have received the auto-compaction request");
        assert_eq!(
            request.intent,
            Some(CompletionIntent::ThreadContextSummarization)
        );

        compaction_model.send_completion_stream_text_chunk(&request, "summary");
        compaction_model.end_completion_stream(&request);
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn test_compaction_usage_counts_toward_cumulative_usage(cx: &mut TestAppContext) {
        let (thread, _event_stream) = setup_thread_for_test(cx).await;
        let model = Arc::new(FakeLanguageModel::default());
        let old_user_message_id = ClientUserMessageId::new();
        let new_user_message_id = ClientUserMessageId::new();
        let prior_usage = TokenUsage {
            input_tokens: 960_000,
            output_tokens: 25,
            ..Default::default()
        };
        let compaction_usage = TokenUsage {
            input_tokens: 40,
            output_tokens: 9,
            cache_creation_input_tokens: 2,
            cache_read_input_tokens: 3,
        };
        let final_usage = TokenUsage {
            input_tokens: 500,
            output_tokens: 50,
            ..Default::default()
        };

        cx.update(|cx| {
            thread.update(cx, |thread, cx| {
                thread.set_model(model.clone(), cx);
                thread
                    .messages
                    .push(user_text_message(old_user_message_id.clone(), "old user"));
                thread.messages.push(agent_text_message("old assistant"));
                thread
                    .request_token_usage
                    .insert(old_user_message_id.clone(), prior_usage);
                thread.cumulative_token_usage = prior_usage;
                thread.current_request_token_usage = prior_usage;
            });
        });

        let _events = cx
            .update(|cx| {
                thread.update(cx, |thread, cx| {
                    thread.send(new_user_message_id.clone(), vec!["new prompt"], cx)
                })
            })
            .unwrap();
        cx.run_until_parked();

        let compaction_request = model.pending_completions().pop().unwrap();
        assert_eq!(
            compaction_request.intent,
            Some(CompletionIntent::ThreadContextSummarization)
        );

        model.send_completion_stream_event(
            &compaction_request,
            LanguageModelCompletionEvent::UsageUpdate(TokenUsage {
                input_tokens: 40,
                output_tokens: 4,
                ..Default::default()
            }),
        );
        model.send_completion_stream_event(
            &compaction_request,
            LanguageModelCompletionEvent::UsageUpdate(compaction_usage),
        );
        model.send_completion_stream_text_chunk(&compaction_request, "compacted old context");
        model.end_completion_stream(&compaction_request);
        cx.run_until_parked();

        let expected_after_compaction = prior_usage + compaction_usage;
        thread.read_with(cx, |thread, _cx| {
            assert_eq!(thread.cumulative_token_usage(), expected_after_compaction);
            assert!(
                !thread
                    .request_token_usage
                    .contains_key(&new_user_message_id)
            );
        });

        let final_request = model.pending_completions().pop().unwrap();
        assert_eq!(final_request.intent, Some(CompletionIntent::UserPrompt));

        model.send_completion_stream_event(
            &final_request,
            LanguageModelCompletionEvent::UsageUpdate(final_usage),
        );
        model.end_completion_stream(&final_request);
        cx.run_until_parked();

        thread.read_with(cx, |thread, _cx| {
            assert_eq!(
                thread.cumulative_token_usage(),
                expected_after_compaction + final_usage
            );
            assert_eq!(
                thread.request_token_usage.get(&new_user_message_id),
                Some(&final_usage)
            );
        });
    }

    #[gpui::test]
    async fn test_replay_emits_context_compaction(cx: &mut TestAppContext) {
        let (thread, _event_stream) = setup_thread_for_test(cx).await;
        let user_message_id = ClientUserMessageId::new();

        let mut replay_events = cx.update(|cx| {
            thread.update(cx, |thread, cx| {
                thread
                    .messages
                    .push(user_text_message(user_message_id.clone(), "before"));
                thread.messages.push(summary_compaction("summary"));
                thread.messages.push(agent_text_message("after"));

                thread.replay(cx)
            })
        });

        let event = replay_events.next().await;
        assert!(
            matches!(
                &event,
                Some(Ok(ThreadEvent::UserMessage(UserMessage { id, .. }))) if id == &user_message_id
            ),
            "expected replayed user message, got {event:?}"
        );

        let event = replay_events.next().await;
        let compaction_id = match &event {
            Some(Ok(ThreadEvent::ContextCompaction(compaction))) => compaction.id.clone(),
            _ => panic!("expected context compaction event, got {event:?}"),
        };

        let event = replay_events.next().await;
        assert!(
            matches!(
                &event,
                Some(Ok(ThreadEvent::ContextCompactionUpdate(update)))
                    if update.id == compaction_id && update.summary_delta == "summary"
            ),
            "expected context compaction summary event, got {event:?}"
        );

        let event = replay_events.next().await;
        assert!(
            matches!(&event, Some(Ok(ThreadEvent::AgentText(text))) if text == "after"),
            "expected replayed agent text, got {event:?}"
        );
    }

    #[gpui::test]
    async fn test_native_compaction_boundary(cx: &mut TestAppContext) {
        let (thread, _event_stream) = setup_thread_for_test(cx).await;

        let request_messages = cx.update(|cx| {
            thread.update(cx, |thread, cx| {
                thread.messages.push(user_text_message(
                    ClientUserMessageId::new(),
                    "before native",
                ));
                thread.messages.push(Arc::new(Message::Compaction(
                    CompactionInfo::ProviderNative {
                        provider: LanguageModelProviderId::from("openai".to_string()),
                        items: vec![json!({"type": "compaction"})],
                    },
                )));
                thread.messages.push(user_text_message(
                    ClientUserMessageId::new(),
                    "after native",
                ));

                thread.build_request_messages(Vec::new(), cx)
            })
        });

        assert_eq!(
            request_texts_after_system(&request_messages),
            vec!["after native".to_string()]
        );
    }

    #[gpui::test]
    async fn test_retained_users_truncate_oldest(cx: &mut TestAppContext) {
        let (thread, _event_stream) = setup_thread_for_test(cx).await;
        let mut long_text = "START".to_string();
        long_text.push_str(&"x".repeat(COMPACTION_RETAINED_USER_MESSAGES_BYTE_BUDGET));
        long_text.push_str("END");

        let request_messages = cx.update(|cx| {
            thread.update(cx, |thread, cx| {
                thread.messages.push(user_text_message(
                    ClientUserMessageId::new(),
                    "dropped older user",
                ));
                thread
                    .messages
                    .push(agent_text_message("dropped assistant"));
                thread
                    .messages
                    .push(user_text_message(ClientUserMessageId::new(), &long_text));
                thread
                    .messages
                    .push(user_text_message(ClientUserMessageId::new(), "new"));
                thread.messages.push(summary_compaction("summary context"));
                thread.messages.push(agent_text_message("after assistant"));
                thread
                    .messages
                    .push(user_text_message(ClientUserMessageId::new(), "after user"));

                thread.build_request_messages(Vec::new(), cx)
            })
        });

        let request_texts = request_texts_after_system(&request_messages);
        assert_eq!(request_texts.len(), 5);
        assert_eq!(
            request_texts[0],
            format!(
                "START{}",
                "x".repeat(
                    COMPACTION_RETAINED_USER_MESSAGES_BYTE_BUDGET - "START".len() - "new".len()
                )
            )
        );
        assert_eq!(request_texts[1], "new");
        assert_eq!(request_texts[2], summary_request_text("summary context"));
        assert_eq!(request_texts[3], "after assistant");
        assert_eq!(request_texts[4], "after user");
        assert!(request_texts.iter().all(
            |text| !text.contains("dropped older user") && !text.contains("dropped assistant")
        ));
    }

    #[test]
    fn test_truncate_text_utf8_boundary() {
        let message = LanguageModelRequestMessage {
            role: Role::User,
            content: vec![MessageContent::Text("hello 👋 world".to_string())],
            cache: false,
            reasoning_details: None,
        };

        let truncated = truncate_user_message_to_byte_budget(message, 8).unwrap();
        assert_eq!(
            truncated.content,
            vec![MessageContent::Text("hello ".to_string())]
        );
    }

    #[test]
    fn test_truncate_keeps_fitting_images() {
        let image = LanguageModelImage {
            source: "image".into(),
        };
        let message = LanguageModelRequestMessage {
            role: Role::User,
            content: vec![
                MessageContent::Text("abc".to_string()),
                MessageContent::Image(image.clone()),
            ],
            cache: false,
            reasoning_details: None,
        };

        let truncated = truncate_user_message_to_byte_budget(message, 8).unwrap();
        assert_eq!(
            truncated.content,
            vec![
                MessageContent::Text("abc".to_string()),
                MessageContent::Image(image),
            ]
        );
    }

    fn setup_parent_with_subagents(
        cx: &mut TestAppContext,
        parent: &Entity<Thread>,
        count: usize,
    ) -> Vec<Entity<Thread>> {
        cx.update(|cx| {
            let mut subagents = Vec::new();
            for _ in 0..count {
                let subagent = cx.new(|cx| Thread::new_subagent(parent, cx));
                parent.update(cx, |thread, _cx| {
                    thread.register_running_subagent(subagent.downgrade());
                });
                subagents.push(subagent);
            }
            subagents
        })
    }

    struct ReplayImageTool;

    impl AgentTool for ReplayImageTool {
        type Input = ();
        type Output = String;

        const NAME: &'static str = "registered_image_tool";

        fn kind() -> acp::ToolKind {
            acp::ToolKind::Other
        }

        fn initial_title(
            &self,
            _input: Result<Self::Input, serde_json::Value>,
            _cx: &mut App,
        ) -> SharedString {
            "Registered Image Tool".into()
        }

        fn run(
            self: Arc<Self>,
            _input: ToolInput<Self::Input>,
            _event_stream: ToolCallEventStream,
            _cx: &mut App,
        ) -> Task<Result<Self::Output, Self::Output>> {
            Task::ready(Ok(String::new()))
        }
    }

    /// ADR-F52: sets the tool permission default to "AI が判定" and, when
    /// `register_judge_model` is set, gives the registry a fake model to judge
    /// with. Returns the fake model so the test can answer for it.
    fn setup_permission_judge(
        cx: &mut TestAppContext,
        register_judge_model: bool,
    ) -> (
        ToolCallEventStream,
        ToolCallEventStreamReceiver,
        Arc<language_model::fake_provider::FakeLanguageModel>,
    ) {
        use language_model::{
            LanguageModelProviderName,
            fake_provider::{FakeLanguageModel, FakeLanguageModelProvider},
        };

        crate::tests::init_test(cx);
        let judge_model = Arc::new(FakeLanguageModel::default());

        cx.update(|cx| {
            LanguageModelRegistry::test(cx);
            if !register_judge_model {
                // `LanguageModelRegistry::test` configures a default model,
                // which the judge would otherwise fall back to.
                LanguageModelRegistry::global(cx).update(cx, |registry, cx| {
                    registry.set_default_model(None, cx);
                });
            }
            if register_judge_model {
                let provider = Arc::new(
                    FakeLanguageModelProvider::new(
                        LanguageModelProviderId::from("fake-corp".to_string()),
                        LanguageModelProviderName::from("Fake Corp".to_string()),
                    )
                    .with_models(vec![judge_model.clone() as Arc<dyn LanguageModel>]),
                );
                let selection = SelectedModel {
                    provider: LanguageModelProviderId::from("fake-corp".to_string()),
                    model: judge_model.id(),
                };
                LanguageModelRegistry::global(cx).update(cx, |registry, cx| {
                    registry.register_provider(provider, cx);
                    registry.select_permission_judge_model(Some(&selection), cx);
                });
            }

            let mut settings = AgentSettings::get_global(cx).clone();
            settings.tool_permissions.default = ToolPermissionMode::Auto;
            AgentSettings::override_global(settings, cx);
        });

        let (event_stream, receiver) = ToolCallEventStream::test();
        (event_stream, receiver, judge_model)
    }

    fn judge_a_build_command(
        event_stream: &ToolCallEventStream,
        cx: &mut TestAppContext,
    ) -> Task<Result<()>> {
        cx.update(|cx| {
            event_stream.authorize(
                "cargo build",
                ToolPermissionContext::new(TerminalTool::NAME, vec!["cargo build".into()]),
                cx,
            )
        })
    }

    /// A `SAFE` verdict runs the tool call without ever showing a prompt, and
    /// leaves the judge's reason on the card.
    #[gpui::test]
    async fn test_permission_judge_safe_verdict_skips_the_prompt(cx: &mut TestAppContext) {
        let (event_stream, mut receiver, judge_model) = setup_permission_judge(cx, true);
        let authorize = judge_a_build_command(&event_stream, cx);

        assert_eq!(
            receiver.expect_permission_judge().await.state,
            acp_thread::PermissionJudgeState::Judging
        );

        cx.run_until_parked();
        let request = judge_model
            .pending_completions()
            .pop()
            .expect("the judge model should have been asked");
        assert_eq!(request.intent, Some(CompletionIntent::PermissionJudge));
        judge_model.send_last_completion_stream_text_chunk("SAFE\nビルドするだけです");
        judge_model.end_last_completion_stream();

        authorize.await.expect("a safe verdict must allow the call");
        let details = receiver.expect_permission_judge().await;
        assert_eq!(details.state, acp_thread::PermissionJudgeState::Safe);
        assert_eq!(details.reason, "ビルドするだけです");
    }

    /// An `UNSAFE` verdict falls back to the normal prompt, with the judge's
    /// reason recorded on the card.
    #[gpui::test]
    async fn test_permission_judge_unsafe_verdict_prompts_with_a_reason(cx: &mut TestAppContext) {
        let (event_stream, mut receiver, judge_model) = setup_permission_judge(cx, true);
        let authorize = judge_a_build_command(&event_stream, cx);

        assert_eq!(
            receiver.expect_permission_judge().await.state,
            acp_thread::PermissionJudgeState::Judging
        );
        cx.run_until_parked();
        judge_model.send_last_completion_stream_text_chunk("UNSAFE\n外部に送信します");
        judge_model.end_last_completion_stream();

        let details = receiver.expect_permission_judge().await;
        assert_eq!(details.state, acp_thread::PermissionJudgeState::Unsafe);
        assert_eq!(details.reason, "外部に送信します");

        let authorization = receiver.expect_authorization().await;
        authorization
            .response
            .send(acp_thread::SelectedPermissionOutcome::new(
                acp::PermissionOptionId::new("deny"),
                acp::PermissionOptionKind::RejectOnce,
            ))
            .expect("the authorization channel should still be open");
        authorize.await.expect_err("the user denied the tool call");
    }

    /// An answer that doesn't start with exactly `SAFE` is not an approval.
    #[gpui::test]
    async fn test_permission_judge_unparsable_answer_prompts(cx: &mut TestAppContext) {
        let (event_stream, mut receiver, judge_model) = setup_permission_judge(cx, true);
        let authorize = judge_a_build_command(&event_stream, cx);

        receiver.expect_permission_judge().await;
        cx.run_until_parked();
        judge_model.send_last_completion_stream_text_chunk("Sure, that looks SAFE to me!");
        judge_model.end_last_completion_stream();

        assert_eq!(
            receiver.expect_permission_judge().await.state,
            acp_thread::PermissionJudgeState::Unsafe
        );
        let authorization = receiver.expect_authorization().await;
        drop(authorization);
        authorize
            .await
            .expect_err("a dropped prompt cancels the tool call");
    }

    /// A judge that never answers hands the decision back to the user after the
    /// timeout rather than leaving the tool call hanging.
    #[gpui::test]
    async fn test_permission_judge_timeout_prompts(cx: &mut TestAppContext) {
        let (event_stream, mut receiver, judge_model) = setup_permission_judge(cx, true);
        let authorize = judge_a_build_command(&event_stream, cx);

        receiver.expect_permission_judge().await;
        cx.run_until_parked();
        assert_eq!(
            judge_model.completion_count(),
            1,
            "the judge should have been asked once"
        );

        cx.executor().advance_clock(
            crate::tool_permissions::PERMISSION_JUDGE_TIMEOUT + Duration::from_secs(1),
        );

        let details = receiver.expect_permission_judge().await;
        assert_eq!(details.state, acp_thread::PermissionJudgeState::Unsafe);
        assert_eq!(
            details.reason,
            crate::tool_permissions::PERMISSION_JUDGE_TIMEOUT_REASON
        );
        let authorization = receiver.expect_authorization().await;
        drop(authorization);
        authorize.await.expect_err("nothing approved the tool call");
    }

    /// With no model to judge with, the user is asked immediately — the tool
    /// call is never approved by default.
    #[gpui::test]
    async fn test_permission_judge_without_a_model_prompts(cx: &mut TestAppContext) {
        let (event_stream, mut receiver, judge_model) = setup_permission_judge(cx, false);
        let authorize = judge_a_build_command(&event_stream, cx);

        receiver.expect_permission_judge().await;
        let details = receiver.expect_permission_judge().await;
        assert_eq!(details.state, acp_thread::PermissionJudgeState::Unsafe);
        assert_eq!(
            details.reason,
            crate::tool_permissions::PERMISSION_JUDGE_NO_MODEL_REASON
        );
        assert_eq!(
            judge_model.completion_count(),
            0,
            "no request may be sent when no judge model is configured"
        );

        let authorization = receiver.expect_authorization().await;
        drop(authorization);
        authorize.await.expect_err("nothing approved the tool call");
    }

    /// An MCP tool whose arguments never arrived is still authorized, but the
    /// judge is not asked: approving on the tool's name alone would let an MCP
    /// server run arguments nobody reviewed.
    #[gpui::test]
    async fn test_permission_judge_is_skipped_for_third_party_tools(cx: &mut TestAppContext) {
        let (event_stream, mut receiver, judge_model) = setup_permission_judge(cx, true);

        let authorize = cx.update(|cx| {
            event_stream.authorize_third_party_tool(
                "ファイルを書き込む",
                "mcp:files:write".to_string(),
                "write".to_string(),
                None,
                cx,
            )
        });

        let details = receiver.expect_permission_judge().await;
        assert_eq!(details.state, acp_thread::PermissionJudgeState::Unsafe);
        assert_eq!(
            details.reason,
            acp_thread::PERMISSION_JUDGE_NO_ARGUMENTS_REASON
        );
        cx.run_until_parked();
        assert_eq!(
            judge_model.completion_count(),
            0,
            "the judge must not be asked about input it cannot see"
        );

        let authorization = receiver.expect_authorization().await;
        drop(authorization);
        authorize.await.expect_err("nothing approved the tool call");
    }

    /// Turning the mode back to plain auto-approval while the judge is thinking
    /// resolves the call from the settings, and the card says the judgement was
    /// dropped rather than claiming a verdict the judge never gave.
    #[gpui::test]
    async fn test_permission_judge_is_superseded_by_a_settings_change(cx: &mut TestAppContext) {
        use gpui::UpdateGlobal as _;

        let (event_stream, mut receiver, judge_model) = setup_permission_judge(cx, true);
        let authorize = judge_a_build_command(&event_stream, cx);

        receiver.expect_permission_judge().await;
        cx.run_until_parked();

        cx.update(|cx| {
            settings::SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings
                        .agent
                        .get_or_insert_default()
                        .tool_permissions
                        .get_or_insert_default()
                        .default = Some(ToolPermissionMode::Allow);
                });
            });
        });

        authorize
            .await
            .expect("the new settings allow the tool call");
        let details = receiver.expect_permission_judge().await;
        assert_eq!(details.state, acp_thread::PermissionJudgeState::Superseded);
        assert_eq!(
            judge_model.completion_count(),
            1,
            "the judge was asked once and its answer dropped"
        );
    }

    /// ADR-F52 A11 (d): cancelling the turn drops the whole authorization task
    /// while the judge is still thinking. The card must say the judgement was
    /// abandoned rather than 「AI が判定中…」 forever.
    #[gpui::test]
    async fn test_permission_judge_in_flight_is_superseded_when_dropped(cx: &mut TestAppContext) {
        let (event_stream, mut receiver, _judge_model) = setup_permission_judge(cx, true);
        let authorize = judge_a_build_command(&event_stream, cx);

        assert_eq!(
            receiver.expect_permission_judge().await.state,
            acp_thread::PermissionJudgeState::Judging
        );
        cx.run_until_parked();

        drop(authorize);
        cx.run_until_parked();

        let details = receiver.expect_permission_judge().await;
        assert_eq!(details.state, acp_thread::PermissionJudgeState::Superseded);
        assert_eq!(details.reason, acp_thread::PERMISSION_JUDGE_CANCELED_REASON);
    }

    /// A judgement that ran to a verdict must not also report itself abandoned
    /// once the authorization task is dropped.
    #[gpui::test]
    async fn test_permission_judge_verdict_is_not_reported_as_abandoned(cx: &mut TestAppContext) {
        let (event_stream, mut receiver, judge_model) = setup_permission_judge(cx, true);
        let authorize = judge_a_build_command(&event_stream, cx);

        receiver.expect_permission_judge().await;
        cx.run_until_parked();
        judge_model.send_last_completion_stream_text_chunk("SAFE\nビルドするだけです");
        judge_model.end_last_completion_stream();
        authorize.await.expect("a safe verdict must allow the call");

        let details = receiver.expect_permission_judge().await;
        assert_eq!(details.state, acp_thread::PermissionJudgeState::Safe);
        assert_eq!(
            receiver.try_next_permission_judge(),
            None,
            "a finished judgement must not also be reported as abandoned"
        );
    }

    /// ADR-F52 A11 (c): the same question in the same thread is answered from
    /// the remembered verdict, and the card says the answer was reused.
    #[gpui::test]
    async fn test_permission_judge_reuses_a_safe_verdict(cx: &mut TestAppContext) {
        let (event_stream, mut receiver, judge_model) = setup_permission_judge(cx, true);

        let authorize = judge_a_build_command(&event_stream, cx);
        receiver.expect_permission_judge().await;
        cx.run_until_parked();
        judge_model.send_last_completion_stream_text_chunk("SAFE\nビルドするだけです");
        judge_model.end_last_completion_stream();
        authorize.await.expect("a safe verdict must allow the call");
        let details = receiver.expect_permission_judge().await;
        assert_eq!(details.state, acp_thread::PermissionJudgeState::Safe);
        assert!(!details.reused, "the first verdict was freshly asked for");

        let authorize = judge_a_build_command(&event_stream, cx);
        let details = receiver.expect_permission_judge().await;
        assert_eq!(details.state, acp_thread::PermissionJudgeState::Safe);
        assert!(details.reused, "the second call must reuse the verdict");
        assert_eq!(details.reason, "ビルドするだけです");
        authorize
            .await
            .expect("the remembered verdict must allow the call");
        cx.run_until_parked();
        assert_eq!(
            judge_model.completion_count(),
            0,
            "the judge must not be asked the same question twice"
        );
    }

    /// A different command is a different question, so it is judged again.
    #[gpui::test]
    async fn test_permission_judge_does_not_reuse_across_commands(cx: &mut TestAppContext) {
        let (event_stream, mut receiver, judge_model) = setup_permission_judge(cx, true);

        let authorize = judge_a_build_command(&event_stream, cx);
        receiver.expect_permission_judge().await;
        cx.run_until_parked();
        judge_model.send_last_completion_stream_text_chunk("SAFE\nビルドするだけです");
        judge_model.end_last_completion_stream();
        authorize.await.expect("a safe verdict must allow the call");
        receiver.expect_permission_judge().await;

        let authorize = cx.update(|cx| {
            event_stream.authorize(
                "cargo test",
                ToolPermissionContext::new(TerminalTool::NAME, vec!["cargo test".into()]),
                cx,
            )
        });
        assert_eq!(
            receiver.expect_permission_judge().await.state,
            acp_thread::PermissionJudgeState::Judging
        );
        cx.run_until_parked();
        assert_eq!(
            judge_model.completion_count(),
            1,
            "a different command must be judged on its own"
        );
        drop(authorize);
    }

    /// An `UNSAFE` verdict is never remembered: the user's answer to the prompt
    /// it caused — not the verdict — is the decision.
    #[gpui::test]
    async fn test_permission_judge_does_not_reuse_an_unsafe_verdict(cx: &mut TestAppContext) {
        let (event_stream, mut receiver, judge_model) = setup_permission_judge(cx, true);

        for round in 0..2 {
            let authorize = judge_a_build_command(&event_stream, cx);
            assert_eq!(
                receiver.expect_permission_judge().await.state,
                acp_thread::PermissionJudgeState::Judging
            );
            cx.run_until_parked();
            assert_eq!(
                judge_model.completion_count(),
                1,
                "round {round}: an unsafe verdict must be asked for again"
            );
            judge_model.send_last_completion_stream_text_chunk("UNSAFE\n外部に送信します");
            judge_model.end_last_completion_stream();
            assert_eq!(
                receiver.expect_permission_judge().await.state,
                acp_thread::PermissionJudgeState::Unsafe
            );
            let authorization = receiver.expect_authorization().await;
            authorization
                .response
                .send(acp_thread::SelectedPermissionOutcome::new(
                    acp::PermissionOptionId::new("allow"),
                    acp::PermissionOptionKind::AllowOnce,
                ))
                .expect("the authorization channel should still be open");
            authorize.await.expect("the user allowed the tool call");
        }
    }

    /// Changing any setting drops every remembered verdict, so a verdict never
    /// outlives the configuration it was made under.
    #[gpui::test]
    async fn test_permission_judge_forgets_verdicts_when_settings_change(cx: &mut TestAppContext) {
        use gpui::UpdateGlobal as _;

        let (event_stream, mut receiver, judge_model) = setup_permission_judge(cx, true);

        let authorize = judge_a_build_command(&event_stream, cx);
        receiver.expect_permission_judge().await;
        cx.run_until_parked();
        judge_model.send_last_completion_stream_text_chunk("SAFE\nビルドするだけです");
        judge_model.end_last_completion_stream();
        authorize.await.expect("a safe verdict must allow the call");
        receiver.expect_permission_judge().await;

        // Still 「AI が判定」, so the gate keeps reaching the judge — only the
        // remembered verdict is dropped.
        cx.update(|cx| {
            settings::SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings
                        .agent
                        .get_or_insert_default()
                        .tool_permissions
                        .get_or_insert_default()
                        .default = Some(ToolPermissionMode::Auto);
                });
            });
        });

        let authorize = judge_a_build_command(&event_stream, cx);
        assert_eq!(
            receiver.expect_permission_judge().await.state,
            acp_thread::PermissionJudgeState::Judging,
            "the verdict must not survive a settings change"
        );
        cx.run_until_parked();
        assert_eq!(
            judge_model.completion_count(),
            1,
            "the judge must be asked again after the settings changed"
        );
        drop(authorize);
    }

    /// ADR-F52 A11 (b): an MCP tool's real arguments reach the judge, so
    /// 「AI が判定」 decides on what the server will actually be asked to do.
    #[gpui::test]
    async fn test_permission_judge_reads_third_party_tool_input(cx: &mut TestAppContext) {
        let (event_stream, mut receiver, judge_model) = setup_permission_judge(cx, true);

        let input = serde_json::json!({ "path": "/repo/notes.md", "contents": "hello" });
        let authorize = cx.update(|cx| {
            event_stream.authorize_third_party_tool(
                "MCP ツール `write` を実行",
                "mcp:files:write".to_string(),
                "write".to_string(),
                Some(&input),
                cx,
            )
        });

        assert_eq!(
            receiver.expect_permission_judge().await.state,
            acp_thread::PermissionJudgeState::Judging
        );
        cx.run_until_parked();
        let request = judge_model
            .pending_completions()
            .pop()
            .expect("the judge model should have been asked");
        let prompt = request
            .messages
            .last()
            .expect("the judge prompt has a user message")
            .string_contents();
        assert!(
            prompt.contains("/repo/notes.md") && prompt.contains("hello"),
            "the judge must see the tool's real arguments: {prompt}"
        );
        assert!(
            prompt.contains("mcp:files:write"),
            "the judge must be told which server's tool it is: {prompt}"
        );

        judge_model.send_last_completion_stream_text_chunk("SAFE\nプロジェクト内の編集です");
        judge_model.end_last_completion_stream();
        authorize.await.expect("a safe verdict must allow the call");
        assert_eq!(
            receiver.expect_permission_judge().await.state,
            acp_thread::PermissionJudgeState::Safe
        );
    }

    /// An MCP tool called with no arguments gives the judge nothing but the
    /// tool's own name to go on, and a name is what an MCP server once talked
    /// its way past this gate with. Every empty shape of input is asked about
    /// instead of judged (ADR-F52).
    #[gpui::test]
    async fn test_permission_judge_is_not_asked_about_an_empty_third_party_input(
        cx: &mut TestAppContext,
    ) {
        for input in [
            serde_json::json!({}),
            serde_json::Value::Null,
            serde_json::json!([]),
        ] {
            let (event_stream, mut receiver, judge_model) = setup_permission_judge(cx, true);
            let authorize = cx.update(|cx| {
                event_stream.authorize_third_party_tool(
                    "MCP ツール `list` を実行",
                    "mcp:files:list".to_string(),
                    "list".to_string(),
                    Some(&input),
                    cx,
                )
            });

            let details = receiver.expect_permission_judge().await;
            assert_eq!(
                details.state,
                acp_thread::PermissionJudgeState::Unsafe,
                "{input} carries nothing to judge"
            );
            assert_eq!(
                details.reason,
                acp_thread::PERMISSION_JUDGE_NO_ARGUMENTS_REASON
            );
            cx.run_until_parked();
            assert_eq!(
                judge_model.completion_count(),
                0,
                "the judge must not be asked to approve a tool's name: {input}"
            );

            let authorization = receiver.expect_authorization().await;
            drop(authorization);
            authorize
                .await
                .expect_err("nothing approved the tool call: {input}");
        }
    }

    /// The judge's memory is thread-scoped, so another thread asking exactly
    /// the same question pays for its own verdict (ADR-F52).
    #[gpui::test]
    async fn test_permission_judge_does_not_reuse_across_threads(cx: &mut TestAppContext) {
        let (event_stream, mut receiver, judge_model) = setup_permission_judge(cx, true);

        let authorize = judge_a_build_command(&event_stream, cx);
        receiver.expect_permission_judge().await;
        cx.run_until_parked();
        judge_model.send_last_completion_stream_text_chunk("SAFE\nビルドするだけです");
        judge_model.end_last_completion_stream();
        authorize.await.expect("a safe verdict must allow the call");
        receiver.expect_permission_judge().await;

        let (other_thread_stream, mut other_receiver) = ToolCallEventStream::test();
        let authorize = judge_a_build_command(&other_thread_stream, cx);
        assert_eq!(
            other_receiver.expect_permission_judge().await.state,
            acp_thread::PermissionJudgeState::Judging,
            "another thread must not inherit this thread's verdicts"
        );
        cx.run_until_parked();
        assert_eq!(
            judge_model.completion_count(),
            1,
            "another thread must ask for its own verdict"
        );
        drop(authorize);
    }

    /// A judgement still in flight is deliberately left out of what the thread
    /// saves: a thread saved mid-judgement would come back saying 「AI が判定
    /// 中…」 about a call that ended long ago. The finished verdict is saved.
    #[gpui::test]
    async fn test_permission_judge_saves_only_a_finished_judgement(cx: &mut TestAppContext) {
        let (event_stream, mut receiver, judge_model) = setup_permission_judge(cx, true);
        let permission_judge = event_stream.permission_judge.clone();

        let authorize = judge_a_build_command(&event_stream, cx);
        assert_eq!(
            receiver.expect_permission_judge().await.state,
            acp_thread::PermissionJudgeState::Judging
        );
        cx.run_until_parked();
        assert!(
            permission_judge.borrow().to_db().is_empty(),
            "a judgement still running must not reach the thread's row"
        );

        judge_model.send_last_completion_stream_text_chunk("SAFE\nビルドするだけです");
        judge_model.end_last_completion_stream();
        authorize.await.expect("a safe verdict must allow the call");
        receiver.expect_permission_judge().await;

        let saved = permission_judge.borrow().to_db();
        let judgement = saved
            .get(&LanguageModelToolUseId::from("test_id"))
            .expect("the finished verdict must reach the thread's row");
        assert_eq!(judgement.state, acp_thread::PermissionJudgeState::Safe);
        assert_eq!(judgement.reason, "ビルドするだけです");
        assert!(!judgement.reused);
        assert_eq!(
            judgement.model_id,
            judge_model.id().0.to_string(),
            "the row names the model that judged"
        );
    }

    #[gpui::test]
    async fn test_authorize_sandbox_allow_always_does_not_cache_thread_grant(
        cx: &mut TestAppContext,
    ) {
        crate::tests::init_test(cx);

        let (event_stream, mut receiver) = ToolCallEventStream::test();
        let request = SandboxRequest {
            network: crate::sandboxing::NetworkRequest::None,
            allow_fs_write_all: false,
            write_paths: vec![
                settings::GrantedWritePath::from_requested(PathBuf::from("/tmp/build")),
                settings::GrantedWritePath::from_requested(PathBuf::from("/tmp/cache")),
                settings::GrantedWritePath::from_requested(PathBuf::from("/tmp/logs")),
                settings::GrantedWritePath::from_requested(PathBuf::from("/tmp/secret")),
            ],
        };

        let authorize = cx.update(|cx| {
            event_stream.authorize_sandbox(
                request.clone(),
                "needs to write build artifacts".to_string(),
                cx,
            )
        });
        let authorization = receiver.expect_authorization().await;
        let details =
            acp_thread::sandbox_authorization_details_from_meta(&authorization.tool_call.meta)
                .expect("sandbox authorization should include request details");
        assert!(details.network_hosts.is_empty());
        assert!(!details.network_all_hosts);
        assert_eq!(details.allow_fs_write_all, request.allow_fs_write_all);
        assert_eq!(details.write_paths, request.write_paths);
        assert!(authorization.tool_call.fields.content.is_none());

        let acp_thread::PermissionOptions::Flat(options) = &authorization.options else {
            panic!("expected flat sandbox permission options");
        };
        let options = options
            .iter()
            .map(|option| {
                (
                    option.option_id.0.as_ref(),
                    option.name.as_ref(),
                    option.kind,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            options,
            vec![
                (
                    "allow",
                    "許可 (今回のみ)",
                    acp::PermissionOptionKind::AllowOnce
                ),
                (
                    "allow_thread",
                    "許可 (このスレッド)",
                    acp::PermissionOptionKind::AllowAlways,
                ),
                (
                    "allow_always",
                    "常に許可",
                    acp::PermissionOptionKind::AllowAlways,
                ),
                ("deny", "拒否", acp::PermissionOptionKind::RejectOnce),
            ]
        );

        let send_result = authorization
            .response
            .send(acp_thread::SelectedPermissionOutcome::new(
                acp::PermissionOptionId::new("allow_always"),
                acp::PermissionOptionKind::AllowAlways,
            ));
        assert!(send_result.is_ok());
        authorize.await.unwrap();

        // 「常に許可」 persists to settings only
        let effective = event_stream.effective_sandbox_request(
            &SandboxRequest::default(),
            &agent_settings::SandboxPermissions::default(),
        );
        assert!(
            effective.write_paths.is_empty(),
            "allow always should not record an in-memory thread grant: {:?}",
            effective.write_paths
        );
    }

    /// ADR-F46: when the sandbox can't be created, the card explains why and
    /// offers exactly two choices, neither of which runs the command without a
    /// sandbox.
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    #[gpui::test]
    async fn test_sandbox_retry_prompt_offers_only_retry_and_cancel(cx: &mut TestAppContext) {
        crate::tests::init_test(cx);

        let (event_stream, mut receiver) = ToolCallEventStream::test();
        let decision = cx.update(|cx| {
            event_stream.ask_to_retry_sandbox_creation(
                Some("cargo build".to_string()),
                "bwrap not found on PATH".to_string(),
                Some("installing-bubblewrap".to_string()),
                0,
                cx,
            )
        });
        let authorization = receiver.expect_authorization().await;
        assert_eq!(
            authorization.tool_call.fields.title, None,
            "the card must keep showing the command"
        );
        let details = acp_thread::sandbox_fallback_authorization_details_from_meta(
            &authorization.tool_call.meta,
        )
        .expect("the retry prompt should include the failure details");
        assert_eq!(details.command.as_deref(), Some("cargo build"));
        assert_eq!(details.reason, "bwrap not found on PATH");
        assert_eq!(
            details.docs_section.as_deref(),
            Some("installing-bubblewrap")
        );

        let acp_thread::PermissionOptions::Flat(options) = &authorization.options else {
            panic!("expected flat retry options");
        };
        let options = options
            .iter()
            .map(|option| {
                (
                    option.option_id.0.as_ref(),
                    option.name.as_ref(),
                    option.kind,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            options,
            vec![
                ("retry", "再試行", acp::PermissionOptionKind::RejectAlways),
                ("deny", "キャンセル", acp::PermissionOptionKind::RejectOnce),
            ]
        );

        authorization
            .response
            .send(acp_thread::SelectedPermissionOutcome::new(
                acp::PermissionOptionId::new(acp_thread::SANDBOX_FALLBACK_RETRY_OPTION_ID),
                acp::PermissionOptionKind::RejectAlways,
            ))
            .unwrap();
        assert_eq!(decision.await.unwrap(), SandboxRetryDecision::Retry);
    }

    #[test]
    fn test_sandbox_retry_label_numbers_the_next_attempt() {
        let retry_label = |retries: usize| {
            let acp_thread::PermissionOptions::Flat(options) =
                sandbox_retry_permission_options(retries)
            else {
                panic!("expected flat retry options");
            };
            options
                .into_iter()
                .find(|option| {
                    option.option_id.0.as_ref() == acp_thread::SANDBOX_FALLBACK_RETRY_OPTION_ID
                })
                .expect("retry option present")
                .name
        };

        assert_eq!(retry_label(0), "再試行");
        assert_eq!(retry_label(1), "再試行 (2 回目)");
        assert_eq!(retry_label(2), "再試行 (3 回目)");
    }

    /// Cancelling records nothing for the thread: the next command gets the
    /// sandbox (or the same card) again.
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    #[gpui::test]
    async fn test_sandbox_retry_prompt_cancel_records_nothing(cx: &mut TestAppContext) {
        crate::tests::init_test(cx);

        let grants = Rc::new(RefCell::new(ThreadSandboxGrants::default()));
        let (event_stream, mut receiver) = ToolCallEventStream::test_with_grants(grants.clone());
        let decision = cx.update(|cx| {
            event_stream.ask_to_retry_sandbox_creation(
                None,
                "bwrap probe failed".to_string(),
                None,
                0,
                cx,
            )
        });
        let authorization = receiver.expect_authorization().await;
        authorization
            .response
            .send(acp_thread::SelectedPermissionOutcome::new(
                acp::PermissionOptionId::new(acp_thread::SandboxPermission::Deny.as_id()),
                acp::PermissionOptionKind::RejectOnce,
            ))
            .unwrap();
        assert_eq!(decision.await.unwrap(), SandboxRetryDecision::Cancel);
        assert_eq!(
            grants.borrow().to_db(),
            crate::db::DbSandboxGrants::default()
        );
    }

    fn set_tool_permissions(
        cx: &mut TestAppContext,
        tool_permissions: agent_settings::ToolPermissions,
    ) {
        cx.update(|cx| {
            let mut settings = AgentSettings::get_global(cx).clone();
            settings.tool_permissions = tool_permissions;
            AgentSettings::override_global(settings, cx);
        });
    }

    /// ADR-F46: auto-approval doesn't reach a command outside the sandbox. The
    /// card says so, and offers only a one-time allow or a denial.
    #[gpui::test]
    async fn test_unsandboxed_command_asks_even_with_auto_approve(cx: &mut TestAppContext) {
        crate::tests::init_test(cx);
        set_tool_permissions(
            cx,
            agent_settings::ToolPermissions {
                default: ToolPermissionMode::Allow,
                tools: Default::default(),
            },
        );

        let (event_stream, mut receiver) = ToolCallEventStream::test();
        let authorize = cx.update(|cx| {
            event_stream.authorize_unsandboxed_command(
                "cargo build".to_string(),
                "設定でサンドボックスが無効になっています".to_string(),
                false,
                cx,
            )
        });
        let authorization = receiver.expect_authorization().await;
        assert_eq!(
            authorization.tool_call.fields.title.as_deref(),
            Some("cargo build")
        );
        let details =
            acp_thread::sandbox_authorization_details_from_meta(&authorization.tool_call.meta)
                .expect("the card should say the command runs outside the sandbox");
        assert_eq!(
            details.outside_sandbox_reason.as_deref(),
            Some("設定でサンドボックスが無効になっています")
        );

        let acp_thread::PermissionOptions::Flat(options) = &authorization.options else {
            panic!("expected flat options");
        };
        let options = options
            .iter()
            .map(|option| {
                (
                    option.option_id.0.as_ref(),
                    option.name.as_ref(),
                    option.kind,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            options,
            vec![
                (
                    "allow",
                    "許可 (今回のみ)",
                    acp::PermissionOptionKind::AllowOnce
                ),
                ("deny", "拒否", acp::PermissionOptionKind::RejectOnce),
            ]
        );

        authorization
            .response
            .send(acp_thread::SelectedPermissionOutcome::new(
                acp::PermissionOptionId::new("allow"),
                acp::PermissionOptionKind::AllowOnce,
            ))
            .unwrap();
        authorize.await.expect("a one-time allow runs the command");
    }

    /// Nothing but the one-time allow runs the command: an "always" outcome
    /// (which this card never offers) is treated as a denial, not persisted.
    #[gpui::test]
    async fn test_unsandboxed_command_treats_an_always_outcome_as_denial(cx: &mut TestAppContext) {
        crate::tests::init_test(cx);

        let (event_stream, mut receiver) = ToolCallEventStream::test();
        let authorize = cx.update(|cx| {
            event_stream.authorize_unsandboxed_command(
                "cargo build".to_string(),
                "設定でサンドボックスが無効になっています".to_string(),
                false,
                cx,
            )
        });
        let authorization = receiver.expect_authorization().await;
        authorization
            .response
            .send(acp_thread::SelectedPermissionOutcome::new(
                acp::PermissionOptionId::new(format!("always_allow:{}", TerminalTool::NAME)),
                acp::PermissionOptionKind::AllowAlways,
            ))
            .unwrap();
        assert!(authorize.await.is_err());
    }

    #[gpui::test]
    async fn test_unsandboxed_command_deny_rule_refuses_without_asking(cx: &mut TestAppContext) {
        crate::tests::init_test(cx);
        let mut tools = HashMap::default();
        tools.insert(
            Arc::from(TerminalTool::NAME),
            agent_settings::ToolRules {
                default: None,
                always_allow: vec![],
                always_deny: vec![
                    agent_settings::CompiledRegex::new(r"^rm\b", false)
                        .expect("valid deny pattern"),
                ],
                always_confirm: vec![],
                invalid_patterns: vec![],
            },
        );
        set_tool_permissions(
            cx,
            agent_settings::ToolPermissions {
                default: ToolPermissionMode::Allow,
                tools,
            },
        );

        let (event_stream, mut receiver) = ToolCallEventStream::test();
        let authorize = cx.update(|cx| {
            event_stream.authorize_unsandboxed_command(
                "rm build.log".to_string(),
                "設定でサンドボックスが無効になっています".to_string(),
                false,
                cx,
            )
        });
        assert!(authorize.await.is_err());
        assert!(
            receiver.try_recv().is_err(),
            "a denied command must not show a card"
        );
    }

    /// A command that may write anywhere but the protected paths is nearly the
    /// same as one outside the sandbox (ADR-F46): the card says so and, like
    /// the unsandboxed card, only offers a one-time allow.
    #[gpui::test]
    async fn test_unsandboxed_command_card_for_unrestricted_writes(cx: &mut TestAppContext) {
        crate::tests::init_test(cx);

        let (event_stream, mut receiver) = ToolCallEventStream::test();
        let _authorize = cx.update(|cx| {
            event_stream.authorize_unsandboxed_command(
                "cargo build".to_string(),
                "設定でファイルシステムへの書き込みがすべて許可されています".to_string(),
                true,
                cx,
            )
        });
        let authorization = receiver.expect_authorization().await;
        let details =
            acp_thread::sandbox_authorization_details_from_meta(&authorization.tool_call.meta)
                .expect("the card should carry the sandbox details");
        assert!(details.allow_fs_write_all);
        assert_eq!(
            details.outside_sandbox_reason.as_deref(),
            Some("設定でファイルシステムへの書き込みがすべて許可されています")
        );
    }

    /// ADR-F46: a model-requested `allow_fs_write_all` is nearly the same as
    /// running outside the sandbox, so its card only offers a one-time allow,
    /// and a standing choice (which the card never offers) is refused and
    /// recorded nowhere.
    #[gpui::test]
    async fn test_unrestricted_write_request_can_only_be_allowed_once(cx: &mut TestAppContext) {
        crate::tests::init_test(cx);

        let request = SandboxRequest {
            allow_fs_write_all: true,
            ..Default::default()
        };
        for standing_choice in [
            acp_thread::SandboxPermission::AllowThread,
            acp_thread::SandboxPermission::AllowAlways,
        ] {
            let grants = Rc::new(RefCell::new(ThreadSandboxGrants::default()));
            let (event_stream, mut receiver) =
                ToolCallEventStream::test_with_grants(grants.clone());
            let authorize = cx.update(|cx| {
                event_stream.authorize_sandbox(request.clone(), "rebuild".to_string(), cx)
            });
            let authorization = receiver.expect_authorization().await;
            let acp_thread::PermissionOptions::Flat(options) = &authorization.options else {
                panic!("expected flat options");
            };
            assert_eq!(
                options
                    .iter()
                    .map(|option| option.option_id.0.as_ref())
                    .collect::<Vec<_>>(),
                vec!["allow", "deny"]
            );
            authorization
                .response
                .send(acp_thread::SelectedPermissionOutcome::new(
                    acp::PermissionOptionId::new(standing_choice.as_id()),
                    acp::PermissionOptionKind::AllowAlways,
                ))
                .unwrap();
            assert!(authorize.await.is_err(), "{standing_choice:?}");
            assert_eq!(
                grants.borrow().to_db(),
                crate::db::DbSandboxGrants::default()
            );
        }

        let (event_stream, mut receiver) = ToolCallEventStream::test();
        let authorize = cx.update(|cx| {
            event_stream.authorize_sandbox(request.clone(), "rebuild".to_string(), cx)
        });
        let authorization = receiver.expect_authorization().await;
        authorization
            .response
            .send(acp_thread::SelectedPermissionOutcome::new(
                acp::PermissionOptionId::new(acp_thread::SandboxPermission::AllowOnce.as_id()),
                acp::PermissionOptionKind::AllowOnce,
            ))
            .unwrap();
        authorize.await.expect("a one-time allow runs the command");
    }

    /// Regression test: the Windows-drive (DrvFs) warning prompt must target
    /// the ACP-facing *scoped* tool-call id. When it used the raw provider id,
    /// the update matched no existing tool call, the ACP layer rejected the
    /// resulting title-less insert ("title is required for a tool call"), the
    /// event stream was torn down, and every terminal command was auto-declined
    /// without the user ever seeing a prompt.
    #[gpui::test]
    async fn test_windows_fs_warning_targets_scoped_tool_call_id(cx: &mut TestAppContext) {
        crate::tests::init_test(cx);

        let (event_stream, mut receiver) = ToolCallEventStream::test();
        let authorize = cx.update(|cx| event_stream.authorize_windows_fs_warning(cx));

        let authorization = receiver.expect_authorization().await;
        assert_eq!(
            &authorization.tool_call.tool_call_id,
            event_stream.tool_call_id(),
            "the warning prompt must reference the scoped ACP tool-call id, \
             not the raw provider id"
        );
        let details =
            acp_thread::sandbox_authorization_details_from_meta(&authorization.tool_call.meta)
                .expect("warning authorization should include sandbox details");
        assert!(details.warn_windows_fs);

        authorization
            .response
            .send(acp_thread::SelectedPermissionOutcome::new(
                acp::PermissionOptionId::new(acp_thread::SandboxPermission::AllowOnce.as_id()),
                acp::PermissionOptionKind::AllowOnce,
            ))
            .unwrap();
        authorize.await.unwrap();
    }

    /// 決定 4: the read-and-edit profile hands the model the file tools and
    /// nothing that runs a command or reaches the network. Both terminal
    /// variants are configured under the single `terminal` name, so leaving
    /// that name out has to drop the sandboxed variant too.
    #[gpui::test]
    async fn test_edit_profile_enables_file_tools_only(cx: &mut TestAppContext) {
        use feature_flags::FeatureFlagAppExt as _;

        let (thread, _event_stream) = setup_thread_for_test(cx).await;
        let model = Arc::new(FakeLanguageModel::default());
        cx.update(|cx| {
            thread.update(cx, |thread, cx| {
                thread.set_model(model, cx);
                thread
                    .add_default_tools(Rc::new(crate::tests::FakeThreadEnvironment::default()), cx);
            });
        });

        let tool_names = |cx: &mut TestAppContext| {
            cx.update(|cx| {
                thread.read_with(cx, |thread, cx| {
                    thread
                        .enabled_tools(cx)
                        .keys()
                        .map(|name| name.to_string())
                        .collect::<Vec<_>>()
                })
            })
        };

        // Only one terminal variant exists at a time and both are exposed
        // under the `terminal` name, so the profile has to drop it in either
        // sandbox state.
        for sandboxing in [false, true] {
            cx.update(|cx| cx.update_flags(sandboxing, vec!["sandboxing".to_string()]));

            cx.update(|cx| {
                thread.update(cx, |thread, cx| {
                    thread.set_profile(AgentProfileId(builtin_profiles::WRITE.into()), cx)
                });
            });
            assert!(
                tool_names(cx).iter().any(|name| name == TerminalTool::NAME),
                "with sandboxing {sandboxing} the write profile still hands over a terminal, \
                 so the edit profile's missing one is that profile's doing"
            );

            cx.update(|cx| {
                thread.update(cx, |thread, cx| {
                    thread.set_profile(AgentProfileId(builtin_profiles::EDIT.into()), cx)
                });
            });
            let tools = tool_names(cx);
            let enabled = |name: &str| tools.iter().any(|tool| tool == name);

            for name in [
                EditFileTool::NAME,
                WriteFileTool::NAME,
                CreateDirectoryTool::NAME,
                ReadFileTool::NAME,
                GrepTool::NAME,
                ListDirectoryTool::NAME,
                FindPathTool::NAME,
                DiagnosticsTool::NAME,
                RepoMapTool::NAME,
                GitDiffTool::NAME,
            ] {
                assert!(
                    enabled(name),
                    "{name} should be enabled by the edit profile"
                );
            }
            for name in [
                TerminalTool::NAME,
                SandboxedTerminalTool::NAME,
                FetchTool::NAME,
                WebSearchTool::NAME,
                DeletePathTool::NAME,
                MovePathTool::NAME,
                CopyPathTool::NAME,
                SpawnAgentTool::NAME,
            ] {
                assert!(
                    !enabled(name),
                    "{name} should be disabled by the edit profile"
                );
            }
        }
    }

    /// 決定 3: every choice on the card for a path outside the project has to
    /// mean what it says. The choice that names the path writes a pattern and
    /// is kept; the tool-wide one writes the tool's default, which out here
    /// wouldn't stop the next prompt while it silently approved the tool
    /// everywhere inside the project, so it is gone.
    #[gpui::test]
    async fn test_outside_project_card_offers_only_path_scoped_choices(cx: &mut TestAppContext) {
        crate::tests::init_test(cx);

        let outside = "/etc/hosts".to_string();
        let (event_stream, mut receiver) = ToolCallEventStream::test();
        let context = ToolPermissionContext::new(EditFileTool::NAME, vec![outside.clone()]);
        let _authorize =
            cx.update(|cx| event_stream.authorize_outside_project("編集", context, cx));
        let authorization = receiver.expect_authorization().await;

        let acp_thread::PermissionOptions::Dropdown(choices) = &authorization.options else {
            panic!("a path card is a plain dropdown");
        };

        // The builder does offer that choice for this very context, so the
        // card's not having it is this code's doing.
        let offered_by_the_builder =
            ToolPermissionContext::new(EditFileTool::NAME, vec![outside.clone()])
                .build_permission_options();
        let acp_thread::PermissionOptions::Dropdown(offered_by_the_builder) =
            &offered_by_the_builder
        else {
            panic!("a path card is a plain dropdown");
        };
        assert!(
            offered_by_the_builder
                .iter()
                .any(|choice| choice.sub_patterns.is_empty()
                    && matches!(choice.allow.kind, acp::PermissionOptionKind::AllowAlways)),
            "the tool-wide choice is what this card has to drop"
        );

        assert!(
            !choices.iter().any(|choice| choice.sub_patterns.is_empty()
                && matches!(choice.allow.kind, acp::PermissionOptionKind::AllowAlways)),
            "no choice may write the tool's own default here, but the card offers {:?}",
            choices
                .iter()
                .map(|choice| choice.label())
                .collect::<Vec<_>>()
        );
        // The once-only pair `run_authorization_loop` auto-resolves against is
        // untouched.
        for kind in [
            acp::PermissionOptionKind::AllowOnce,
            acp::PermissionOptionKind::RejectOnce,
        ] {
            assert!(
                authorization.options.first_option_of_kind(kind).is_some(),
                "{kind:?} has to stay on the card"
            );
        }

        let standing = choices
            .iter()
            .find(|choice| !choice.sub_patterns.is_empty())
            .expect("the choice that names this path is still offered");
        assert!(
            matches!(standing.allow.kind, acp::PermissionOptionKind::AllowAlways),
            "the surviving standing choice is the always-allow one"
        );
        assert!(
            matches!(
                standing.build_outcome(true).params,
                Some(acp_thread::SelectedPermissionParams::Terminal { .. })
            ),
            "it carries patterns, so pressing it writes a pattern instead of the tool's default"
        );

        // Pressing it has to lift the prompt for what it named — and for
        // nothing else. The label is the containing directory, so that is the
        // scope asserted here.
        cx.update(|cx| {
            let mut settings = agent_settings::AgentSettings::get_global(cx).clone();
            settings.tool_permissions.default = settings::ToolPermissionMode::Confirm;
            settings.tool_permissions.tools.insert(
                Arc::from(EditFileTool::NAME),
                agent_settings::ToolRules {
                    default: None,
                    always_allow: standing
                        .sub_patterns
                        .iter()
                        .map(|pattern| {
                            agent_settings::CompiledRegex::new(pattern, false)
                                .expect("the offered pattern compiles")
                        })
                        .collect(),
                    always_deny: Vec::new(),
                    always_confirm: Vec::new(),
                    invalid_patterns: Vec::new(),
                },
            );
            agent_settings::AgentSettings::override_global(settings, cx);

            let settings = agent_settings::AgentSettings::get_global(cx);
            assert_eq!(
                decide_permission_for_paths_outside_project(
                    EditFileTool::NAME,
                    std::slice::from_ref(&outside),
                    settings,
                ),
                ToolPermissionDecision::Allow,
                "the path the choice named no longer prompts"
            );
            assert_eq!(
                decide_permission_for_paths_outside_project(
                    EditFileTool::NAME,
                    &["/var/tmp/notes".to_string()],
                    settings,
                ),
                ToolPermissionDecision::Confirm,
                "another place outside the project still prompts"
            );
            assert_eq!(
                crate::decide_permission_for_paths(
                    EditFileTool::NAME,
                    &["root/src/main.rs".to_string()],
                    settings,
                ),
                ToolPermissionDecision::Confirm,
                "and the tool's default is untouched, so nothing inside the project \
                 was approved along the way"
            );
        });
    }

    /// 決定 1: the Windows-drive (DrvFs) warning is a property of where the
    /// project lives, not of the command, so 「続行」 answers it for the rest of
    /// the thread. 「中止」 is not remembered, and another thread is asked on
    /// its own.
    #[gpui::test]
    async fn test_windows_fs_warning_is_asked_once_per_thread(cx: &mut TestAppContext) {
        crate::tests::init_test(cx);

        let accept = |authorization: ToolCallAuthorization| {
            authorization
                .response
                .send(acp_thread::SelectedPermissionOutcome::new(
                    acp::PermissionOptionId::new(acp_thread::SandboxPermission::AllowOnce.as_id()),
                    acp::PermissionOptionKind::AllowOnce,
                ))
                .expect("the warning prompt should still be waiting");
        };

        let grants = Rc::new(RefCell::new(ThreadSandboxGrants::default()));
        let (event_stream, mut receiver) = ToolCallEventStream::test_with_grants(grants.clone());
        let authorize = cx.update(|cx| event_stream.authorize_windows_fs_warning(cx));
        accept(receiver.expect_authorization().await);
        authorize.await.expect("continuing runs the command");

        let (event_stream, mut receiver) = ToolCallEventStream::test_with_grants(grants.clone());
        cx.update(|cx| event_stream.authorize_windows_fs_warning(cx))
            .await
            .expect("the accepted warning no longer gates the thread's commands");
        assert!(
            receiver.try_recv().is_err(),
            "the warning must not be shown twice in the same thread"
        );

        // The acknowledgement is not a grant: nothing was recorded, so a
        // reopened thread (which restores only the persisted grants) asks
        // again.
        assert_eq!(
            grants.borrow().to_db(),
            crate::db::DbSandboxGrants::default()
        );

        let other_thread = Rc::new(RefCell::new(ThreadSandboxGrants::default()));
        let (event_stream, mut receiver) =
            ToolCallEventStream::test_with_grants(other_thread.clone());
        let authorize = cx.update(|cx| event_stream.authorize_windows_fs_warning(cx));
        let authorization = receiver.expect_authorization().await;
        authorization
            .response
            .send(acp_thread::SelectedPermissionOutcome::new(
                acp::PermissionOptionId::new(acp_thread::SandboxPermission::Deny.as_id()),
                acp::PermissionOptionKind::RejectOnce,
            ))
            .unwrap();
        assert!(
            authorize.await.is_err(),
            "another thread is asked on its own, and aborting cancels the command"
        );

        let (event_stream, mut receiver) = ToolCallEventStream::test_with_grants(other_thread);
        let authorize = cx.update(|cx| event_stream.authorize_windows_fs_warning(cx));
        accept(receiver.expect_authorization().await);
        authorize
            .await
            .expect("an abort must not be remembered, so the next command asks again");
    }

    #[test]
    fn test_auto_resolve_permission_outcome_uses_once_only_options() {
        let options = acp_thread::PermissionOptions::Dropdown(vec![
            acp_thread::PermissionOptionChoice {
                allow: acp::PermissionOption::new(
                    acp::PermissionOptionId::new("always_allow:test_tool"),
                    "Always allow",
                    acp::PermissionOptionKind::AllowAlways,
                ),
                deny: acp::PermissionOption::new(
                    acp::PermissionOptionId::new("always_deny:test_tool"),
                    "Always deny",
                    acp::PermissionOptionKind::RejectAlways,
                ),
                sub_patterns: vec![],
            },
            acp_thread::PermissionOptionChoice {
                allow: acp::PermissionOption::new(
                    acp::PermissionOptionId::new("allow"),
                    "Allow once",
                    acp::PermissionOptionKind::AllowOnce,
                ),
                deny: acp::PermissionOption::new(
                    acp::PermissionOptionId::new("deny"),
                    "Deny once",
                    acp::PermissionOptionKind::RejectOnce,
                ),
                sub_patterns: vec![],
            },
        ]);

        let allow = auto_resolve_permission_outcome(&options, true)
            .expect("allow auto-resolve should use once-only option");
        assert_eq!(allow.option_id, acp::PermissionOptionId::new("allow"));
        assert_eq!(allow.option_kind, acp::PermissionOptionKind::AllowOnce);

        let deny = auto_resolve_permission_outcome(&options, false)
            .expect("deny auto-resolve should use once-only option");
        assert_eq!(deny.option_id, acp::PermissionOptionId::new("deny"));
        assert_eq!(deny.option_kind, acp::PermissionOptionKind::RejectOnce);
    }

    #[gpui::test]
    async fn test_replay_tool_call_replays_image_content(cx: &mut TestAppContext) {
        let (thread, _event_stream) = setup_thread_for_test(cx).await;

        let registered_tool_use_id = LanguageModelToolUseId::from("registered_tool_id");
        let missing_tool_use_id = LanguageModelToolUseId::from("missing_tool_id");
        let image_data = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";
        let image = LanguageModelImage {
            source: image_data.into(),
        };

        let mut replay_events = cx.update(|cx| {
            thread.update(cx, |thread, cx| {
                thread.add_tool(ReplayImageTool);

                let registered_tool_use = LanguageModelToolUse {
                    id: registered_tool_use_id.clone(),
                    name: ReplayImageTool::NAME.into(),
                    raw_input: "null".to_string(),
                    input: language_model::LanguageModelToolUseInput::Json(json!(null)),
                    is_input_complete: true,
                    thought_signature: None,
                };
                let missing_tool_use = LanguageModelToolUse {
                    id: missing_tool_use_id.clone(),
                    name: "missing_image_tool".into(),
                    raw_input: "{}".to_string(),
                    input: language_model::LanguageModelToolUseInput::Json(json!({})),
                    is_input_complete: true,
                    thought_signature: None,
                };

                let mut tool_results = IndexMap::default();
                tool_results.insert(
                    registered_tool_use_id.clone(),
                    LanguageModelToolResult {
                        tool_use_id: registered_tool_use_id.clone(),
                        tool_name: ReplayImageTool::NAME.into(),
                        is_error: false,
                        content: vec![
                            LanguageModelToolResultContent::Text("before".into()),
                            LanguageModelToolResultContent::Image(image.clone()),
                            LanguageModelToolResultContent::Text("after".into()),
                        ],
                        output: Some(json!("raw output")),
                    },
                );
                tool_results.insert(
                    missing_tool_use_id.clone(),
                    LanguageModelToolResult {
                        tool_use_id: missing_tool_use_id.clone(),
                        tool_name: "missing_image_tool".into(),
                        is_error: false,
                        content: vec![LanguageModelToolResultContent::Image(image.clone())],
                        output: Some(json!("raw output")),
                    },
                );

                thread.messages.push(Arc::new(Message::Agent(AgentMessage {
                    content: vec![
                        AgentMessageContent::ToolUse(registered_tool_use),
                        AgentMessageContent::ToolUse(missing_tool_use),
                    ],
                    tool_results,
                    reasoning_details: None,
                })));

                thread.replay(cx)
            })
        });

        let mut tool_use_ids_with_image_content = HashSet::default();
        while let Some(event) = replay_events.next().await {
            let event = event.unwrap();
            if let ThreadEvent::ToolCallUpdate(acp_thread::ToolCallUpdate::UpdateFields(update)) =
                event
                && let Some(content) = &update.fields.content
                && content.iter().any(|content| {
                    matches!(
                        content,
                        acp::ToolCallContent::Content(acp::Content {
                            content: acp::ContentBlock::Image(_),
                            ..
                        })
                    )
                })
            {
                tool_use_ids_with_image_content.insert(update.tool_call_id.to_string());
            }
        }

        // Both tool uses live in the message pushed above, at index 0 (see
        // `scoped_tool_call_id`).
        assert!(
            tool_use_ids_with_image_content
                .contains(&scoped_tool_call_id(0, &registered_tool_use_id).to_string())
        );
        assert!(
            tool_use_ids_with_image_content
                .contains(&scoped_tool_call_id(0, &missing_tool_use_id).to_string())
        );
    }

    #[gpui::test]
    async fn test_set_model_propagates_to_subagents(cx: &mut TestAppContext) {
        let (parent, _event_stream) = setup_thread_for_test(cx).await;
        let subagents = setup_parent_with_subagents(cx, &parent, 2);

        let new_model: Arc<dyn LanguageModel> = Arc::new(FakeLanguageModel::with_id_and_thinking(
            "test-provider",
            "new-model",
            "New Model",
            false,
        ));

        cx.update(|cx| {
            parent.update(cx, |thread, cx| {
                thread.set_model(new_model, cx);
            });

            for subagent in &subagents {
                let subagent_model_id = subagent.read(cx).model().unwrap().id();
                assert_eq!(
                    subagent_model_id.0.as_ref(),
                    "new-model",
                    "Subagent model should match parent model after set_model"
                );
            }
        });
    }

    #[gpui::test]
    async fn test_set_summarization_model_propagates_to_subagents(cx: &mut TestAppContext) {
        let (parent, _event_stream) = setup_thread_for_test(cx).await;
        let subagents = setup_parent_with_subagents(cx, &parent, 2);

        let summary_model: Arc<dyn LanguageModel> =
            Arc::new(FakeLanguageModel::with_id_and_thinking(
                "test-provider",
                "summary-model",
                "Summary Model",
                false,
            ));

        cx.update(|cx| {
            parent.update(cx, |thread, cx| {
                thread.set_summarization_model(Some(summary_model), cx);
            });

            for subagent in &subagents {
                let subagent_summary_id = subagent.read(cx).summarization_model().unwrap().id();
                assert_eq!(
                    subagent_summary_id.0.as_ref(),
                    "summary-model",
                    "Subagent summarization model should match parent after set_summarization_model"
                );
            }
        });
    }

    #[gpui::test]
    async fn test_set_thinking_enabled_propagates_to_subagents(cx: &mut TestAppContext) {
        let (parent, _event_stream) = setup_thread_for_test(cx).await;
        let subagents = setup_parent_with_subagents(cx, &parent, 2);

        cx.update(|cx| {
            parent.update(cx, |thread, cx| {
                thread.set_thinking_enabled(true, cx);
            });

            for subagent in &subagents {
                assert!(
                    subagent.read(cx).thinking_enabled(),
                    "Subagent thinking should be enabled after parent enables it"
                );
            }

            parent.update(cx, |thread, cx| {
                thread.set_thinking_enabled(false, cx);
            });

            for subagent in &subagents {
                assert!(
                    !subagent.read(cx).thinking_enabled(),
                    "Subagent thinking should be disabled after parent disables it"
                );
            }
        });
    }

    #[gpui::test]
    async fn test_set_thinking_effort_propagates_to_subagents(cx: &mut TestAppContext) {
        let (parent, _event_stream) = setup_thread_for_test(cx).await;
        let subagents = setup_parent_with_subagents(cx, &parent, 2);

        cx.update(|cx| {
            parent.update(cx, |thread, cx| {
                thread.set_thinking_effort(Some("high".to_string()), cx);
            });

            for subagent in &subagents {
                assert_eq!(
                    subagent.read(cx).thinking_effort().map(|s| s.as_str()),
                    Some("high"),
                    "Subagent thinking effort should match parent"
                );
            }

            parent.update(cx, |thread, cx| {
                thread.set_thinking_effort(None, cx);
            });

            for subagent in &subagents {
                assert_eq!(
                    subagent.read(cx).thinking_effort(),
                    None,
                    "Subagent thinking effort should be None after parent clears it"
                );
            }
        });
    }

    // A workspace with untrusted worktrees downgrades `write` to `minimal`,
    // which enables no tools at all — the model then has no way to read files
    // and improvises an explanation. The downgrade has to lift the moment the
    // folder is trusted, so the user does not have to re-pick the profile or
    // open a new thread after answering the trust prompt.
    #[gpui::test]
    async fn test_restricted_workspace_profile_downgrade_is_reversible(cx: &mut TestAppContext) {
        use project::trusted_worktrees::{PathTrust, TrustedWorktrees};

        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            LanguageModelRegistry::test(cx);
            project::trusted_worktrees::init(Default::default(), cx);
        });

        let fs = fs::FakeFs::new(cx.background_executor.clone());
        fs.insert_tree(
            util::path!("/project"),
            json!({ "main.rs": "fn main() {}" }),
        )
        .await;
        let templates = Templates::new();
        let project = Project::test(fs.clone(), [util::path!("/project").as_ref()], cx).await;
        cx.run_until_parked();

        let worktree_id = project.read_with(cx, |project, cx| {
            project.worktrees(cx).next().unwrap().read(cx).id()
        });

        let thread = cx.update(|cx| {
            let project_context = cx.new(|_cx| prompt_store::ProjectContext::default());
            let context_server_store = project.read(cx).context_server_store();
            let context_server_registry =
                cx.new(|cx| ContextServerRegistry::new(context_server_store, cx));
            cx.new(|cx| {
                Thread::new(
                    project.clone(),
                    project_context,
                    context_server_registry,
                    templates,
                    None,
                    cx,
                )
            })
        });

        cx.update(|cx| {
            assert_eq!(
                AgentSettings::get_global(cx).default_profile.as_str(),
                builtin_profiles::WRITE,
                "this test assumes the shipped default profile"
            );

            let trusted = TrustedWorktrees::try_get_global(cx).unwrap();
            let worktree_store = project.read(cx).worktree_store();

            assert!(
                !trusted.update(cx, |trusted, cx| {
                    trusted.can_trust(&worktree_store, worktree_id, cx)
                }),
                "a worktree that was never trusted should start out restricted"
            );

            assert!(thread.read(cx).profile_was_downgraded(cx));
            assert_eq!(
                thread.read(cx).profile(cx).as_str(),
                builtin_profiles::MINIMAL,
            );

            // 決定 4: the read-and-edit profile hands out edit tools, so a
            // restricted workspace downgrades it as well.
            let (downgraded, was_downgraded) = Thread::profile_for_restricted_workspace(
                AgentProfileId(builtin_profiles::EDIT.into()),
                &project,
                cx,
            );
            assert!(was_downgraded);
            assert_eq!(downgraded.as_str(), builtin_profiles::MINIMAL);

            trusted.update(cx, |trusted, cx| {
                trusted.trust(
                    &worktree_store,
                    HashSet::from_iter([PathTrust::Worktree(worktree_id)]),
                    cx,
                );
            });

            assert!(!thread.read(cx).profile_was_downgraded(cx));
            assert_eq!(
                thread.read(cx).profile(cx).as_str(),
                builtin_profiles::WRITE,
                "trusting the folder must restore the user's profile in the running thread"
            );
        });
    }

    #[gpui::test]
    async fn test_subagent_inherits_settings_at_creation(cx: &mut TestAppContext) {
        let (parent, _event_stream) = setup_thread_for_test(cx).await;

        cx.update(|cx| {
            parent.update(cx, |thread, cx| {
                thread.set_speed(Speed::Fast, cx);
                thread.set_thinking_enabled(true, cx);
                thread.set_thinking_effort(Some("high".to_string()), cx);
                thread.set_profile(AgentProfileId("custom-profile".into()), cx);
            });
        });

        let subagents = setup_parent_with_subagents(cx, &parent, 1);

        cx.update(|cx| {
            let sub = subagents[0].read(cx);
            assert_eq!(sub.speed(), Some(Speed::Fast));
            assert!(sub.thinking_enabled());
            assert_eq!(sub.thinking_effort().map(|s| s.as_str()), Some("high"));
            assert_eq!(sub.profile(cx), AgentProfileId("custom-profile".into()));
        });
    }

    #[gpui::test]
    async fn test_set_speed_propagates_to_subagents(cx: &mut TestAppContext) {
        let (parent, _event_stream) = setup_thread_for_test(cx).await;
        let subagents = setup_parent_with_subagents(cx, &parent, 2);

        cx.update(|cx| {
            parent.update(cx, |thread, cx| {
                thread.set_speed(Speed::Fast, cx);
            });

            for subagent in &subagents {
                assert_eq!(
                    subagent.read(cx).speed(),
                    Some(Speed::Fast),
                    "Subagent speed should match parent after set_speed"
                );
            }
        });
    }

    #[gpui::test]
    async fn test_dropped_subagent_does_not_panic(cx: &mut TestAppContext) {
        let (parent, _event_stream) = setup_thread_for_test(cx).await;
        let subagents = setup_parent_with_subagents(cx, &parent, 1);

        // Drop the subagent so the WeakEntity can no longer be upgraded
        drop(subagents);

        // Should not panic even though the subagent was dropped
        cx.update(|cx| {
            parent.update(cx, |thread, cx| {
                thread.set_thinking_enabled(true, cx);
                thread.set_speed(Speed::Fast, cx);
                thread.set_thinking_effort(Some("high".to_string()), cx);
            });
        });
    }

    #[gpui::test]
    async fn test_handle_tool_use_json_parse_error_adds_tool_use_to_content(
        cx: &mut TestAppContext,
    ) {
        let (thread, event_stream) = setup_thread_for_test(cx).await;

        let tool_use_id = LanguageModelToolUseId::from("test_tool_id");
        let tool_name: Arc<str> = Arc::from("test_tool");
        let raw_input: Arc<str> = Arc::from("{invalid json");
        let json_parse_error = "expected value at line 1 column 1".to_string();

        let (_cancellation_tx, cancellation_rx) = watch::channel(false);

        let (_owning_message_ix, result) = cx
            .update(|cx| {
                thread.update(cx, |thread, cx| {
                    // Call the function under test
                    thread
                        .handle_tool_use_json_parse_error_event(
                            tool_use_id.clone(),
                            tool_name.clone(),
                            raw_input.clone(),
                            json_parse_error,
                            &event_stream,
                            cancellation_rx,
                            cx,
                        )
                        .unwrap()
                })
            })
            .await;

        // Verify the result is an error
        assert!(result.is_error);
        assert_eq!(result.tool_use_id, tool_use_id);
        assert_eq!(result.tool_name, tool_name);
        assert!(matches!(
            result.content.as_slice(),
            [LanguageModelToolResultContent::Text(_)]
        ));

        thread.update(cx, |thread, _cx| {
            // Verify the tool use was added to the message content
            {
                let last_message = thread.pending_message();
                assert_eq!(
                    last_message.content.len(),
                    1,
                    "Should have one tool_use in content"
                );

                match &last_message.content[0] {
                    AgentMessageContent::ToolUse(tool_use) => {
                        assert_eq!(tool_use.id, tool_use_id);
                        assert_eq!(tool_use.name, tool_name);
                        assert_eq!(tool_use.raw_input, raw_input.to_string());
                        assert!(tool_use.is_input_complete);
                        // Should fall back to empty object for invalid JSON
                        assert_eq!(
                            tool_use.input,
                            language_model::LanguageModelToolUseInput::Json(json!({}))
                        );
                    }
                    _ => panic!("Expected ToolUse content"),
                }
            }

            // Insert the tool result (simulating what the caller does)
            thread
                .pending_message()
                .tool_results
                .insert(result.tool_use_id.clone(), result);

            // Verify the tool result was added
            let last_message = thread.pending_message();
            assert_eq!(
                last_message.tool_results.len(),
                1,
                "Should have one tool_result"
            );
            assert!(last_message.tool_results.contains_key(&tool_use_id));
        })
    }
}
