mod agent_profile;
mod user_agents_md;

use std::cmp::Ordering::{Equal, Greater, Less};
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, LazyLock};

use anyhow::Context as _;
use collections::{HashSet, IndexMap};
use fs::Fs;
use futures::channel::oneshot;
use gpui::{App, Pixels, SharedString, px};
use language_model::LanguageModel;
use project::DisableAiSettings;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings::{
    DockPosition, DockSide, IntoGpui, LanguageModelParameters, LanguageModelSelection,
    NotifyWhenAgentWaiting, PanelTabBar, PlaySoundWhenAgentDone, RegisterSetting, Settings,
    SettingsContent, SettingsStore, SidebarDockPosition, SidebarSide, ThinkingBlockDisplay,
    ToolPermissionMode, update_settings_file_with_completion,
};
use util::ResultExt as _;

pub use crate::agent_profile::*;
pub use crate::user_agents_md::{UserAgentsMd, UserAgentsMdState, init as init_user_agents_md};

pub const SUMMARIZE_THREAD_PROMPT: &str = include_str!("prompts/summarize_thread_prompt.txt");
pub const SUMMARIZE_THREAD_DETAILED_PROMPT: &str =
    include_str!("prompts/summarize_thread_detailed_prompt.txt");
pub const COMPACTION_PROMPT: &str = include_str!("prompts/compaction_prompt.txt");

/// Default for [`AgentSettings::auto_compact_min_context_window`], mirroring
/// `agent.auto_compact_min_context_window` in `assets/settings/default.json`.
pub const DEFAULT_AUTO_COMPACT_MIN_CONTEXT_WINDOW: u64 = 80_000;

/// Floor for [`AgentSettings::auto_compact_min_context_window`]. Below this a
/// compaction pass has no room to summarize into, so the setting is clamped
/// rather than honored (TODO F13 ③).
pub const AUTO_COMPACT_MIN_CONTEXT_WINDOW_FLOOR: u64 = 8_000;

/// Default for [`AgentSettings::idle_disconnect_minutes`], mirroring
/// `agent.idle_disconnect_minutes` in `assets/settings/default.json`.
pub const DEFAULT_IDLE_DISCONNECT_MINUTES: u64 = 5;

/// Ceiling for [`AgentSettings::system_prompt_repo_map_bytes`], matching the
/// `repo_map` tool's own default budget: anything larger belongs in a tool call
/// the model can scope, not in every request's system prompt.
pub const MAX_SYSTEM_PROMPT_REPO_MAP_BYTES: usize = 32_768;

/// How much of each chat attachment is sent to the agent, mirroring
/// `agent.context_attachments` in `assets/settings/default.json`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContextAttachmentSettings {
    pub inline_max_bytes: usize,
    pub message_budget_bytes: usize,
    pub outline_max_bytes: usize,
    pub fetch_max_bytes: usize,
    pub diagnostics_max_files: usize,
}

impl ContextAttachmentSettings {
    pub const DEFAULT: Self = Self {
        inline_max_bytes: 16_384,
        message_budget_bytes: 65_536,
        outline_max_bytes: 8_192,
        fetch_max_bytes: 32_768,
        diagnostics_max_files: 50,
    };

    /// Values below these floors would leave no room for the outline header or
    /// the reference an attachment is reduced to, so they are raised to them.
    const FLOOR: Self = Self {
        inline_max_bytes: 1_024,
        message_budget_bytes: 4_096,
        outline_max_bytes: 512,
        fetch_max_bytes: 1_024,
        diagnostics_max_files: 1,
    };

    fn from_content(content: Option<settings::ContextAttachmentsSettingsContent>) -> Self {
        let content = content.unwrap_or_default();
        let default = Self::DEFAULT;
        let floor = Self::FLOOR;
        Self {
            inline_max_bytes: content
                .inline_max_bytes
                .unwrap_or(default.inline_max_bytes)
                .max(floor.inline_max_bytes),
            message_budget_bytes: content
                .message_budget_bytes
                .unwrap_or(default.message_budget_bytes)
                .max(floor.message_budget_bytes),
            outline_max_bytes: content
                .outline_max_bytes
                .unwrap_or(default.outline_max_bytes)
                .max(floor.outline_max_bytes),
            fetch_max_bytes: content
                .fetch_max_bytes
                .unwrap_or(default.fetch_max_bytes)
                .max(floor.fetch_max_bytes),
            diagnostics_max_files: content
                .diagnostics_max_files
                .unwrap_or(default.diagnostics_max_files)
                .max(floor.diagnostics_max_files),
        }
    }
}

impl Default for ContextAttachmentSettings {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Bounds on the width of the threads list. They constrain the configured
/// default as well as the width the user drags to, so that no width the sidebar
/// can hold is able to crowd out the editor or collapse the list.
pub const THREADS_LIST_MIN_WIDTH: Pixels = px(200.0);
pub const THREADS_LIST_MAX_WIDTH: Pixels = px(800.0);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PanelLayout {
    pub(crate) agent_dock: Option<DockPosition>,
    pub(crate) project_panel_dock: Option<DockSide>,
    pub(crate) outline_panel_dock: Option<DockSide>,
    pub(crate) collaboration_panel_dock: Option<DockPosition>,
    pub(crate) git_panel_dock: Option<DockPosition>,
}

impl PanelLayout {
    const AGENT: Self = Self {
        agent_dock: Some(DockPosition::Left),
        project_panel_dock: Some(DockSide::Right),
        outline_panel_dock: Some(DockSide::Right),
        collaboration_panel_dock: Some(DockPosition::Right),
        git_panel_dock: Some(DockPosition::Right),
    };

    const EDITOR: Self = Self {
        agent_dock: Some(DockPosition::Right),
        project_panel_dock: Some(DockSide::Left),
        outline_panel_dock: Some(DockSide::Left),
        collaboration_panel_dock: Some(DockPosition::Left),
        git_panel_dock: Some(DockPosition::Left),
    };

    pub fn is_agent_layout(&self) -> bool {
        *self == Self::AGENT
    }

    pub fn is_editor_layout(&self) -> bool {
        *self == Self::EDITOR
    }

    fn read_from(content: &SettingsContent) -> Self {
        Self {
            agent_dock: content.agent.as_ref().and_then(|a| a.dock),
            project_panel_dock: content.project_panel.as_ref().and_then(|p| p.dock),
            outline_panel_dock: content.outline_panel.as_ref().and_then(|p| p.dock),
            collaboration_panel_dock: content.collaboration_panel.as_ref().and_then(|p| p.dock),
            git_panel_dock: content.git_panel.as_ref().and_then(|p| p.dock),
        }
    }

    fn write_to(&self, settings: &mut SettingsContent) {
        settings.agent.get_or_insert_default().dock = self.agent_dock;
        settings.project_panel.get_or_insert_default().dock = self.project_panel_dock;
        settings.outline_panel.get_or_insert_default().dock = self.outline_panel_dock;
        settings.collaboration_panel.get_or_insert_default().dock = self.collaboration_panel_dock;
        settings.git_panel.get_or_insert_default().dock = self.git_panel_dock;
    }

    fn write_diff_to(&self, current_merged: &PanelLayout, settings: &mut SettingsContent) {
        if self.agent_dock != current_merged.agent_dock {
            settings.agent.get_or_insert_default().dock = self.agent_dock;
        }
        if self.project_panel_dock != current_merged.project_panel_dock {
            settings.project_panel.get_or_insert_default().dock = self.project_panel_dock;
        }
        if self.outline_panel_dock != current_merged.outline_panel_dock {
            settings.outline_panel.get_or_insert_default().dock = self.outline_panel_dock;
        }
        if self.collaboration_panel_dock != current_merged.collaboration_panel_dock {
            settings.collaboration_panel.get_or_insert_default().dock =
                self.collaboration_panel_dock;
        }
        if self.git_panel_dock != current_merged.git_panel_dock {
            settings.git_panel.get_or_insert_default().dock = self.git_panel_dock;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowLayout {
    Editor(Option<PanelLayout>),
    Agent(Option<PanelLayout>),
    Custom(PanelLayout),
}

impl WindowLayout {
    pub fn agent() -> Self {
        Self::Agent(None)
    }

    pub fn editor() -> Self {
        Self::Editor(None)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AutoCompactThreshold {
    /// Compact once the context window is at least this full, as a fraction in
    /// the range `(0.0, 1.0]`.
    Percentage(f64),
    /// Compact once at least this many tokens have been used.
    TokensUsed(u64),
    /// Compact once fewer than this many tokens remain in the context window.
    TokensRemaining(u64),
}

impl AutoCompactThreshold {
    /// The threshold used when none is configured, or when the configured value
    /// is invalid (90% of the context window).
    pub const DEFAULT: Self = Self::Percentage(0.9);
}

impl fmt::Display for AutoCompactThreshold {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Percentage(percent) => write!(formatter, "{}%", percent * 100.0),
            Self::TokensUsed(tokens) => write!(formatter, "{tokens}"),
            Self::TokensRemaining(tokens) => write!(formatter, "-{tokens}"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AutoCompactSettings {
    pub enabled: bool,
    pub threshold: AutoCompactThreshold,
}

fn parse_auto_compact_threshold(raw: &str) -> anyhow::Result<AutoCompactThreshold> {
    let trimmed = raw.trim();
    if let Some(percent) = trimmed.strip_suffix('%') {
        let value: f64 = percent
            .trim_end()
            .parse()
            .with_context(|| format!("invalid auto_compact threshold percentage {raw:?}"))?;
        anyhow::ensure!(
            value > 0.0 && value <= 100.0,
            "auto_compact threshold percentage must be between 0% and 100%, got {raw:?}"
        );
        Ok(AutoCompactThreshold::Percentage(value / 100.0))
    } else {
        let tokens: i64 = trimmed.parse().with_context(|| {
            format!(
                "invalid auto_compact threshold {raw:?}; \
                 expected a percentage like \"90%\" or an integer number of tokens"
            )
        })?;
        match tokens.cmp(&0) {
            Greater => Ok(AutoCompactThreshold::TokensUsed(tokens as u64)),
            Less => Ok(AutoCompactThreshold::TokensRemaining(tokens.unsigned_abs())),
            Equal => {
                anyhow::bail!("auto_compact threshold of 0 is not valid")
            }
        }
    }
}

#[derive(Clone, Debug, RegisterSetting)]
pub struct AgentSettings {
    pub enabled: bool,
    pub button: bool,
    pub dock: DockPosition,
    pub starts_open: bool,
    pub flexible: bool,
    pub sidebar_enabled: bool,
    pub sidebar_side: SidebarDockPosition,
    pub panel_thread_history: bool,
    pub panel_tab_bar: PanelTabBar,
    pub threads_sidebar_default_width: Pixels,
    pub default_width: Pixels,
    pub default_height: Pixels,
    pub max_content_width: Option<Pixels>,
    pub default_model: Option<LanguageModelSelection>,
    pub subagent_model: Option<LanguageModelSelection>,
    pub inline_assistant_model: Option<LanguageModelSelection>,
    pub inline_assistant_use_streaming_tools: bool,
    pub commit_message_model: Option<LanguageModelSelection>,
    pub commit_message_include_project_rules: bool,
    pub commit_message_instructions: Option<String>,
    pub thread_summary_model: Option<LanguageModelSelection>,
    /// Model that judges whether a tool action is dangerous when the tool
    /// permission mode is `auto` (ADR-F52).
    pub permission_judge_model: Option<LanguageModelSelection>,
    pub compaction_model: Option<LanguageModelSelection>,
    pub inline_alternatives: Vec<LanguageModelSelection>,
    pub favorite_models: Vec<LanguageModelSelection>,
    pub default_profile: AgentProfileId,
    pub profiles: IndexMap<AgentProfileId, AgentProfileSettings>,

    pub notify_when_agent_waiting: NotifyWhenAgentWaiting,
    pub play_sound_when_agent_done: PlaySoundWhenAgentDone,
    pub prevent_idle_sleep: bool,
    pub single_file_review: bool,
    pub model_parameters: Vec<LanguageModelParameters>,
    pub auto_compact: AutoCompactSettings,
    /// Smallest model context window (in tokens) that still gets
    /// auto-compaction, already clamped to
    /// [`AUTO_COMPACT_MIN_CONTEXT_WINDOW_FLOOR`].
    pub auto_compact_min_context_window: u64,
    pub attach_active_file: bool,
    pub context_attachments: ContextAttachmentSettings,
    /// Byte budget for the repository map prepended to the system prompt,
    /// already clamped to `0..=`[`MAX_SYSTEM_PROMPT_REPO_MAP_BYTES`]. `0`
    /// disables it.
    pub system_prompt_repo_map_bytes: usize,
    /// How long an external agent's connection is kept once no thread of that
    /// agent is open any more, in minutes. `0` keeps it for as long as the
    /// window.
    pub idle_disconnect_minutes: u64,
    pub enable_feedback: bool,
    pub expand_edit_card: bool,
    pub expand_terminal_card: bool,
    pub terminal_init_command: Option<String>,
    pub thinking_display: ThinkingBlockDisplay,
    pub cancel_generation_on_terminal_stop: bool,
    pub use_modifier_to_send: bool,
    pub message_editor_min_lines: usize,
    pub show_turn_stats: bool,
    pub show_merge_conflict_indicator: bool,
    pub tool_permissions: ToolPermissions,
    pub sandbox_permissions: SandboxPermissions,
}

impl AgentSettings {
    pub fn enabled(&self, cx: &App) -> bool {
        self.enabled && !DisableAiSettings::get_global(cx).disable_ai
    }

    /// ADR-F49 (new decision): how long a connection to an external agent is kept
    /// once nothing uses it. `None` when the user turned the disconnect off.
    pub fn idle_disconnect(&self) -> Option<std::time::Duration> {
        // Saturating: the minutes come straight from the user's settings file,
        // and a number large enough to overflow the seconds is still one a
        // user can write.
        (self.idle_disconnect_minutes > 0).then(|| {
            std::time::Duration::from_secs(self.idle_disconnect_minutes.saturating_mul(60))
        })
    }

    pub fn temperature_for_model(model: &Arc<dyn LanguageModel>, cx: &App) -> Option<f32> {
        let settings = Self::get_global(cx);
        for setting in settings.model_parameters.iter().rev() {
            if let Some(provider) = &setting.provider
                && provider.0 != model.provider_id().0
            {
                continue;
            }
            if let Some(setting_model) = &setting.model
                && *setting_model != model.id().0
            {
                continue;
            }
            return setting.temperature;
        }
        return None;
    }

    pub fn sidebar_side(&self) -> SidebarSide {
        match self.sidebar_side {
            SidebarDockPosition::Left => SidebarSide::Left,
            SidebarDockPosition::Right => SidebarSide::Right,
        }
    }

    pub fn set_message_editor_max_lines(&self) -> usize {
        self.message_editor_min_lines * 2
    }

    pub fn favorite_model_ids(&self) -> HashSet<SharedString> {
        self.favorite_models
            .iter()
            .map(|sel| SharedString::from(format!("{}/{}", sel.provider.0, sel.model)))
            .collect()
    }
}

pub fn language_model_to_selection(
    model: &Arc<dyn LanguageModel>,
    override_selection: Option<&LanguageModelSelection>,
) -> LanguageModelSelection {
    let provider = model.provider_id().0.to_string().into();
    let model_name = model.id().0.to_string();
    match override_selection {
        Some(current) => LanguageModelSelection {
            provider,
            model: model_name,
            enable_thinking: current.enable_thinking && model.supports_thinking(),
            effort: current
                .effort
                .clone()
                .filter(|value| {
                    model
                        .supported_effort_levels()
                        .iter()
                        .any(|level| level.value.as_ref() == value.as_str())
                })
                .or_else(|| {
                    model
                        .default_effort_level()
                        .map(|effort| effort.value.to_string())
                }),
            speed: current.speed.filter(|_| model.supports_fast_mode()),
        },
        None => LanguageModelSelection {
            provider,
            model: model_name,
            enable_thinking: model.supports_thinking(),
            effort: model
                .default_effort_level()
                .map(|effort| effort.value.to_string()),
            speed: None,
        },
    }
}

impl AgentSettings {
    pub fn get_layout(cx: &App) -> WindowLayout {
        let store = cx.global::<SettingsStore>();
        let merged = store.merged_settings();
        let user_layout = store
            .raw_user_settings()
            .map(|u| PanelLayout::read_from(u.content.as_ref()))
            .unwrap_or_default();
        let merged_layout = PanelLayout::read_from(merged);

        if merged_layout.is_agent_layout() {
            return WindowLayout::Agent(Some(user_layout));
        }

        if merged_layout.is_editor_layout() {
            return WindowLayout::Editor(Some(user_layout));
        }

        WindowLayout::Custom(user_layout)
    }

    pub fn set_layout(
        layout: WindowLayout,
        fs: Arc<dyn Fs>,
        cx: &App,
    ) -> oneshot::Receiver<anyhow::Result<()>> {
        let merged = PanelLayout::read_from(cx.global::<SettingsStore>().merged_settings());

        match layout {
            WindowLayout::Agent(None) => {
                update_settings_file_with_completion(fs, cx, move |settings, _cx| {
                    PanelLayout::AGENT.write_diff_to(&merged, settings);
                })
            }
            WindowLayout::Editor(None) => {
                update_settings_file_with_completion(fs, cx, move |settings, _cx| {
                    PanelLayout::EDITOR.write_diff_to(&merged, settings);
                })
            }
            WindowLayout::Agent(Some(saved))
            | WindowLayout::Editor(Some(saved))
            | WindowLayout::Custom(saved) => {
                update_settings_file_with_completion(fs, cx, move |settings, _cx| {
                    saved.write_to(settings);
                })
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq, Hash, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AgentProfileId(pub Arc<str>);

impl AgentProfileId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for AgentProfileId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Default for AgentProfileId {
    fn default() -> Self {
        Self("write".into())
    }
}

/// Persistent "allow always" sandbox grants for agent-run terminal commands.
///
/// Coverage decisions for these grants are made in
/// `agent::sandboxing::ThreadSandboxGrants::covers_with_persistent`, which
/// combines them with the in-memory per-thread grants. `write_paths` are
/// stored as minimal, lexically-normalized subtrees (see
/// [`compile_sandbox_permissions`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxPermissions {
    /// Allow sandboxed commands to reach any host over the network.
    pub allow_all_hosts: bool,
    /// Hosts sandboxed commands may always reach, in canonical form (exact
    /// hostnames or leading-`*.` subdomain wildcards). Parsed/validated where
    /// consumed (`agent::sandboxing`).
    pub network_hosts: Vec<String>,
    /// Allow sandboxed commands to write anywhere except protected paths. That
    /// is nearly the same as running outside the sandbox, so while it is on
    /// terminal commands are never auto-approved (ADR-F46).
    pub allow_fs_write_all: bool,
    /// Run agent terminal commands outside the OS sandbox, with Zed's own
    /// permissions. This is the model-facing "off switch": when set, the
    /// sandboxed terminal tool is not exposed and the system prompt omits the
    /// sandbox section, so the model uses the plain `terminal` tool (on
    /// Windows, WSL sandbox setup is skipped). Such commands are never
    /// auto-approved (ADR-F46). Only the user's settings set this; the agent
    /// has no way to ask for a command to run outside the sandbox — see
    /// `agent::sandboxing`.
    pub allow_unsandboxed: bool,
    /// Directory subtree grants, each paired with the canonical
    /// (symlink-resolved) target established when the grant was approved.
    pub write_paths: Vec<settings::GrantedWritePath>,
    /// Whether sandbox escalation prompts warn about domains or write paths
    /// that contain potentially confusable Unicode characters (homoglyphs,
    /// invisible characters, or bidirectional overrides). Enabled by default.
    pub warn_confusable_unicode: bool,
    /// Whether to warn (Windows/WSL only) when a sandbox grant targets a file on
    /// a Windows-hosted (DrvFs) filesystem, whose sandbox-integrity guarantees
    /// are weaker than a distro-native filesystem. Enabled by default.
    pub warn_ntfs_grants: bool,
}

impl Default for SandboxPermissions {
    fn default() -> Self {
        Self {
            allow_all_hosts: false,
            network_hosts: Vec::new(),
            allow_fs_write_all: false,
            allow_unsandboxed: false,
            write_paths: Vec::new(),
            // The confusable-Unicode warning is a safety net, so it defaults on.
            warn_confusable_unicode: true,
            // The weaker-guarantee warning for Windows-hosted grants defaults on.
            warn_ntfs_grants: true,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ToolPermissions {
    /// Global default permission when no tool-specific rules or patterns match.
    pub default: ToolPermissionMode,
    pub tools: collections::HashMap<Arc<str>, ToolRules>,
}

impl ToolPermissions {
    /// Returns all invalid regex patterns across all tools.
    pub fn invalid_patterns(&self) -> Vec<&InvalidRegexPattern> {
        self.tools
            .values()
            .flat_map(|rules| rules.invalid_patterns.iter())
            .collect()
    }

    /// Returns true if any tool has invalid regex patterns.
    pub fn has_invalid_patterns(&self) -> bool {
        self.tools
            .values()
            .any(|rules| !rules.invalid_patterns.is_empty())
    }
}

/// Represents a regex pattern that failed to compile.
#[derive(Clone, Debug)]
pub struct InvalidRegexPattern {
    /// The pattern string that failed to compile.
    pub pattern: String,
    /// Which rule list this pattern was in (e.g., "always_deny", "always_allow", "always_confirm").
    pub rule_type: String,
    /// The error message from the regex compiler.
    pub error: String,
}

#[derive(Clone, Debug, Default)]
pub struct ToolRules {
    pub default: Option<ToolPermissionMode>,
    pub always_allow: Vec<CompiledRegex>,
    pub always_deny: Vec<CompiledRegex>,
    pub always_confirm: Vec<CompiledRegex>,
    /// Patterns that failed to compile. If non-empty, tool calls should be blocked.
    pub invalid_patterns: Vec<InvalidRegexPattern>,
}

#[derive(Clone)]
pub struct CompiledRegex {
    pub pattern: String,
    pub case_sensitive: bool,
    pub regex: regex::Regex,
}

impl std::fmt::Debug for CompiledRegex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompiledRegex")
            .field("pattern", &self.pattern)
            .field("case_sensitive", &self.case_sensitive)
            .finish()
    }
}

impl CompiledRegex {
    pub fn new(pattern: &str, case_sensitive: bool) -> Option<Self> {
        Self::try_new(pattern, case_sensitive).ok()
    }

    pub fn try_new(pattern: &str, case_sensitive: bool) -> Result<Self, regex::Error> {
        let regex = regex::RegexBuilder::new(pattern)
            .case_insensitive(!case_sensitive)
            .build()?;
        Ok(Self {
            pattern: pattern.to_string(),
            case_sensitive,
            regex,
        })
    }

    pub fn is_match(&self, input: &str) -> bool {
        self.regex.is_match(input)
    }
}

pub const HARDCODED_SECURITY_DENIAL_MESSAGE: &str = "組み込みのセキュリティルールによりブロックされました。この操作は許可するには危険すぎると判断されており、設定で上書きすることはできません。";

/// Security rules that are always enforced and cannot be overridden by any setting.
/// These protect against catastrophic operations like wiping filesystems.
pub struct HardcodedSecurityRules {
    pub terminal_deny: Vec<CompiledRegex>,
}

pub static HARDCODED_SECURITY_RULES: LazyLock<HardcodedSecurityRules> = LazyLock::new(|| {
    const FLAGS: &str = r"(--[a-zA-Z0-9][-a-zA-Z0-9_]*(=[^\s]*)?\s+|-[a-zA-Z]+\s+)*";
    const TRAILING_FLAGS: &str = r"(\s+--[a-zA-Z0-9][-a-zA-Z0-9_]*(=[^\s]*)?|\s+-[a-zA-Z]+)*\s*";

    HardcodedSecurityRules {
        terminal_deny: vec![
            // Recursive deletion of root - "rm -rf /", "rm -rf /*"
            CompiledRegex::new(
                &format!(r"\brm\s+{FLAGS}(--\s+)?/\*?{TRAILING_FLAGS}$"),
                false,
            )
            .expect("hardcoded regex should compile"),
            // Recursive deletion of home via tilde - "rm -rf ~", "rm -rf ~/"
            CompiledRegex::new(
                &format!(r"\brm\s+{FLAGS}(--\s+)?~/?\*?{TRAILING_FLAGS}$"),
                false,
            )
            .expect("hardcoded regex should compile"),
            // Recursive deletion of home via env var - "rm -rf $HOME", "rm -rf ${HOME}"
            CompiledRegex::new(
                &format!(r"\brm\s+{FLAGS}(--\s+)?(\$HOME|\$\{{HOME\}})/?(\*)?{TRAILING_FLAGS}$"),
                false,
            )
            .expect("hardcoded regex should compile"),
            // Recursive deletion of current directory - "rm -rf .", "rm -rf ./"
            CompiledRegex::new(
                &format!(r"\brm\s+{FLAGS}(--\s+)?\./?\*?{TRAILING_FLAGS}$"),
                false,
            )
            .expect("hardcoded regex should compile"),
            // Recursive deletion of parent directory - "rm -rf ..", "rm -rf ../"
            CompiledRegex::new(
                &format!(r"\brm\s+{FLAGS}(--\s+)?\.\./?\*?{TRAILING_FLAGS}$"),
                false,
            )
            .expect("hardcoded regex should compile"),
        ],
    }
});

/// Checks if input matches any hardcoded security rules that cannot be bypassed.
/// Returns the denial reason string if blocked, None otherwise.
///
/// `terminal_tool_name` should be the tool name used for the terminal tool
/// (e.g. `"terminal"`). `extracted_commands` can optionally provide parsed
/// sub-commands for chained command checking; callers with access to a shell
/// parser should extract sub-commands and pass them here.
pub fn check_hardcoded_security_rules(
    tool_name: &str,
    terminal_tool_name: &str,
    input: &str,
    extracted_commands: Option<&[String]>,
) -> Option<String> {
    if tool_name != terminal_tool_name {
        return None;
    }

    let rules = &*HARDCODED_SECURITY_RULES;
    let terminal_patterns = &rules.terminal_deny;

    if matches_hardcoded_patterns(input, terminal_patterns) {
        return Some(HARDCODED_SECURITY_DENIAL_MESSAGE.into());
    }

    if let Some(commands) = extracted_commands {
        for command in commands {
            if matches_hardcoded_patterns(command, terminal_patterns) {
                return Some(HARDCODED_SECURITY_DENIAL_MESSAGE.into());
            }
        }
    }

    None
}

fn matches_hardcoded_patterns(command: &str, patterns: &[CompiledRegex]) -> bool {
    for pattern in patterns {
        if pattern.is_match(command) {
            return true;
        }
    }

    for expanded in expand_rm_to_single_path_commands(command) {
        for pattern in patterns {
            if pattern.is_match(&expanded) {
                return true;
            }
        }
    }

    false
}

fn expand_rm_to_single_path_commands(command: &str) -> Vec<String> {
    let trimmed = command.trim();

    let first_token = trimmed.split_whitespace().next();
    if !first_token.is_some_and(|t| t.eq_ignore_ascii_case("rm")) {
        return vec![];
    }

    let parts: Vec<&str> = trimmed.split_whitespace().collect();
    let mut flags = Vec::new();
    let mut paths = Vec::new();
    let mut past_double_dash = false;

    for part in parts.iter().skip(1) {
        if !past_double_dash && *part == "--" {
            past_double_dash = true;
            flags.push(*part);
            continue;
        }
        if !past_double_dash && part.starts_with('-') {
            flags.push(*part);
        } else {
            paths.push(*part);
        }
    }

    let flags_str = if flags.is_empty() {
        String::new()
    } else {
        format!("{} ", flags.join(" "))
    };

    let mut results = Vec::new();
    for path in &paths {
        if path.starts_with('$') {
            let home_prefix = if path.starts_with("${HOME}") {
                Some("${HOME}")
            } else if path.starts_with("$HOME") {
                Some("$HOME")
            } else {
                None
            };

            if let Some(prefix) = home_prefix {
                let suffix = &path[prefix.len()..];
                if suffix.is_empty() {
                    results.push(format!("rm {flags_str}{path}"));
                } else if suffix.starts_with('/') {
                    let normalized_suffix = normalize_path(suffix);
                    let reconstructed = if normalized_suffix == "/" {
                        prefix.to_string()
                    } else {
                        format!("{prefix}{normalized_suffix}")
                    };
                    results.push(format!("rm {flags_str}{reconstructed}"));
                } else {
                    results.push(format!("rm {flags_str}{path}"));
                }
            } else {
                results.push(format!("rm {flags_str}{path}"));
            }
            continue;
        }

        let mut normalized = normalize_path(path);
        if normalized.is_empty() && !Path::new(path).has_root() {
            normalized = ".".to_string();
        }

        results.push(format!("rm {flags_str}{normalized}"));
    }

    results
}

pub fn normalize_path(raw: &str) -> String {
    let is_absolute = Path::new(raw).has_root();
    let mut components: Vec<&str> = Vec::new();
    for component in Path::new(raw).components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if components.last() == Some(&"..") {
                    components.push("..");
                } else if !components.is_empty() {
                    components.pop();
                } else if !is_absolute {
                    components.push("..");
                }
            }
            Component::Normal(segment) => {
                if let Some(s) = segment.to_str() {
                    components.push(s);
                }
            }
            Component::RootDir | Component::Prefix(_) => {}
        }
    }
    let joined = components.join("/");
    if is_absolute {
        format!("/{joined}")
    } else {
        joined
    }
}

impl Settings for AgentSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let agent = content.agent.clone().unwrap();
        Self {
            enabled: agent.enabled.unwrap(),
            button: agent.button.unwrap(),
            dock: agent.dock.unwrap(),
            starts_open: agent.starts_open.unwrap_or(false),
            sidebar_enabled: agent.sidebar_enabled.unwrap_or(false),
            sidebar_side: agent.sidebar_side.unwrap(),
            panel_thread_history: agent.panel_thread_history.unwrap_or(true),
            panel_tab_bar: agent.panel_tab_bar.unwrap_or_default(),
            // Clamped once here so that every reader gets a width the sidebar can
            // actually hold, rather than each call site having to remember to.
            threads_sidebar_default_width: agent
                .threads_sidebar_default_width
                .unwrap()
                .into_gpui()
                .clamp(THREADS_LIST_MIN_WIDTH, THREADS_LIST_MAX_WIDTH),
            default_width: agent.default_width.unwrap().into_gpui(),
            default_height: agent.default_height.unwrap().into_gpui(),
            max_content_width: if agent.limit_content_width.unwrap() {
                Some(agent.max_content_width.unwrap().into_gpui())
            } else {
                None
            },
            flexible: agent.flexible.unwrap(),
            default_model: agent.default_model,
            subagent_model: agent.subagent_model,
            inline_assistant_model: agent.inline_assistant_model,
            inline_assistant_use_streaming_tools: agent
                .inline_assistant_use_streaming_tools
                .unwrap_or(true),
            commit_message_include_project_rules: agent
                .commit_message_include_project_rules
                .unwrap(),
            commit_message_model: agent.commit_message_model,
            commit_message_instructions: agent.commit_message_instructions,
            thread_summary_model: agent.thread_summary_model,
            permission_judge_model: agent.permission_judge_model,
            compaction_model: agent.compaction_model,
            inline_alternatives: agent.inline_alternatives.unwrap_or_default(),
            favorite_models: agent.favorite_models,
            default_profile: AgentProfileId(agent.default_profile.unwrap()),
            profiles: agent
                .profiles
                .unwrap()
                .into_iter()
                .map(|(key, val)| (AgentProfileId(key), val.into()))
                .collect(),

            notify_when_agent_waiting: agent.notify_when_agent_waiting.unwrap(),
            play_sound_when_agent_done: agent.play_sound_when_agent_done.unwrap_or_default(),
            prevent_idle_sleep: agent.prevent_idle_sleep.unwrap(),
            single_file_review: agent.single_file_review.unwrap(),
            model_parameters: agent.model_parameters,
            auto_compact: {
                let auto_compact = agent.auto_compact.unwrap();
                let threshold = parse_auto_compact_threshold(&auto_compact.threshold.unwrap().0)
                    .log_err()
                    .unwrap_or(AutoCompactThreshold::DEFAULT);
                AutoCompactSettings {
                    enabled: auto_compact.enabled.unwrap(),
                    threshold,
                }
            },
            auto_compact_min_context_window: agent
                .auto_compact_min_context_window
                .unwrap_or(DEFAULT_AUTO_COMPACT_MIN_CONTEXT_WINDOW)
                .max(AUTO_COMPACT_MIN_CONTEXT_WINDOW_FLOOR),
            attach_active_file: agent.attach_active_file.unwrap_or(false),
            context_attachments: ContextAttachmentSettings::from_content(agent.context_attachments),
            system_prompt_repo_map_bytes: agent
                .system_prompt_repo_map_bytes
                .unwrap_or(0)
                .min(MAX_SYSTEM_PROMPT_REPO_MAP_BYTES),
            idle_disconnect_minutes: agent
                .idle_disconnect_minutes
                .unwrap_or(DEFAULT_IDLE_DISCONNECT_MINUTES),
            enable_feedback: agent.enable_feedback.unwrap(),
            expand_edit_card: agent.expand_edit_card.unwrap(),
            expand_terminal_card: agent.expand_terminal_card.unwrap(),
            terminal_init_command: agent
                .terminal_init_command
                .filter(|command| !command.trim().is_empty()),
            thinking_display: agent.thinking_display.unwrap(),
            cancel_generation_on_terminal_stop: agent.cancel_generation_on_terminal_stop.unwrap(),
            use_modifier_to_send: agent.use_modifier_to_send.unwrap(),
            message_editor_min_lines: agent.message_editor_min_lines.unwrap(),
            show_turn_stats: agent.show_turn_stats.unwrap(),
            show_merge_conflict_indicator: agent.show_merge_conflict_indicator.unwrap(),
            tool_permissions: compile_tool_permissions(agent.tool_permissions),
            sandbox_permissions: compile_sandbox_permissions(agent.sandbox_permissions),
        }
    }
}

fn compile_sandbox_permissions(
    content: Option<settings::SandboxPermissionsContent>,
) -> SandboxPermissions {
    let Some(content) = content else {
        return SandboxPermissions::default();
    };

    let mut write_paths: Vec<settings::GrantedWritePath> = Vec::new();
    for entry in content.write_paths.map(|paths| paths.0).unwrap_or_default() {
        // Normalize away `..`/`.` *before* folding the WSL spelling:
        // `canonical_wsl_grant_spelling` leaves a path containing `..`
        // unchanged (ADR-F49 決定 40), so folding first on a path like
        // `\\wsl$\Ubuntu\home\..\tmp` would be a no-op and only
        // `normalize_lexically` afterwards would run, leaving the old
        // `\\wsl$\` spelling stored instead of the folded one. Coverage
        // checks are purely lexical, so entries whose requested (or
        // resolved) path escapes the filesystem root are dropped.
        let Ok(requested) = util::paths::normalize_lexically(&entry.requested) else {
            continue;
        };
        let requested = fold_wsl_grant(requested);
        let granted = match entry.resolved {
            Some(resolved) => {
                let Ok(resolved) = util::paths::normalize_lexically(&resolved) else {
                    continue;
                };
                let resolved = fold_wsl_grant(resolved);
                settings::GrantedWritePath::resolved_on_fs(requested, resolved, entry.on_windows_fs)
            }
            None => settings::GrantedWritePath::from_requested(requested),
        };
        insert_granted_subtree(&mut write_paths, granted);
    }

    let network_hosts = content
        .network_hosts
        .map(|hosts| hosts.0)
        .unwrap_or_default();

    SandboxPermissions {
        allow_all_hosts: content.allow_all_hosts.unwrap_or(false),
        network_hosts,
        allow_fs_write_all: content.allow_fs_write_all.unwrap_or(false),
        allow_unsandboxed: content.allow_unsandboxed.unwrap_or(false),
        write_paths,
        warn_confusable_unicode: content.warn_confusable_unicode.unwrap_or(true),
        warn_ntfs_grants: content.warn_ntfs_grants.unwrap_or(true),
    }
}

/// ADR-F49 decision 34: a grant in a WSL distribution, in the one spelling
/// grants are stored and compared in. Grants saved before that decision name
/// the distribution as `wsl.exe` reported it (`\\wsl.localhost\Ubuntu\…`), and
/// comparison is a plain `starts_with`, so the same folder approved again was
/// asked for again and left a second row behind. Folding them here is what
/// makes the old row cover the new grant; `migrator` folds the rows in the
/// settings file itself so the duplicate also disappears from the list.
///
/// Not gated on Windows: the spelling only ever arises there, but a settings
/// file is read on whatever machine has it, and both ends of a comparison must
/// fold the same way.
fn fold_wsl_grant(path: PathBuf) -> PathBuf {
    path.to_str()
        .and_then(util::path_mapping::canonical_wsl_grant_spelling)
        .map_or(path, PathBuf::from)
}

/// Subtree-insert mirroring [`util::paths::insert_subtree`], but over
/// [`settings::GrantedWritePath`] entries compared by their canonical
/// (symlink-resolved) grant path — the path actually enforced at write time.
///
/// Insertion is a no-op when the new grant's canonical path is already covered
/// by an existing entry; otherwise the new grant is added and any existing
/// entries whose canonical path is a descendant of it are pruned. Containment
/// is purely lexical, so callers should normalize paths first.
fn insert_granted_subtree(
    subtrees: &mut Vec<settings::GrantedWritePath>,
    granted: settings::GrantedWritePath,
) {
    if subtrees.iter().any(|existing| {
        granted
            .canonical_or_requested()
            .starts_with(existing.canonical_or_requested())
    }) {
        return;
    }
    subtrees.retain(|existing| {
        !existing
            .canonical_or_requested()
            .starts_with(granted.canonical_or_requested())
    });
    subtrees.push(granted);
}

fn compile_tool_permissions(content: Option<settings::ToolPermissionsContent>) -> ToolPermissions {
    let Some(content) = content else {
        return ToolPermissions::default();
    };

    let tools = content
        .tools
        .into_iter()
        .map(|(tool_name, rules_content)| {
            let mut invalid_patterns = Vec::new();

            let (always_allow, allow_errors) = compile_regex_rules(
                rules_content.always_allow.map(|v| v.0).unwrap_or_default(),
                "always_allow",
            );
            invalid_patterns.extend(allow_errors);

            let (always_deny, deny_errors) = compile_regex_rules(
                rules_content.always_deny.map(|v| v.0).unwrap_or_default(),
                "always_deny",
            );
            invalid_patterns.extend(deny_errors);

            let (always_confirm, confirm_errors) = compile_regex_rules(
                rules_content
                    .always_confirm
                    .map(|v| v.0)
                    .unwrap_or_default(),
                "always_confirm",
            );
            invalid_patterns.extend(confirm_errors);

            // Log invalid patterns for debugging. Users will see an error when they
            // attempt to use a tool with invalid patterns in their settings.
            for invalid in &invalid_patterns {
                log::error!(
                    "Invalid regex pattern in tool_permissions for '{}' tool ({}): '{}' - {}",
                    tool_name,
                    invalid.rule_type,
                    invalid.pattern,
                    invalid.error,
                );
            }

            let rules = ToolRules {
                // Preserve tool-specific default; None means fall back to global default at decision time
                default: rules_content.default,
                always_allow,
                always_deny,
                always_confirm,
                invalid_patterns,
            };
            (tool_name, rules)
        })
        .collect();

    ToolPermissions {
        default: content.default.unwrap_or_default(),
        tools,
    }
}

fn compile_regex_rules(
    rules: Vec<settings::ToolRegexRule>,
    rule_type: &str,
) -> (Vec<CompiledRegex>, Vec<InvalidRegexPattern>) {
    let mut compiled = Vec::new();
    let mut errors = Vec::new();

    for rule in rules {
        if rule.pattern.is_empty() {
            errors.push(InvalidRegexPattern {
                pattern: rule.pattern,
                rule_type: rule_type.to_string(),
                error: "empty regex patterns are not allowed".to_string(),
            });
            continue;
        }
        let case_sensitive = rule.case_sensitive.unwrap_or(false);
        match CompiledRegex::try_new(&rule.pattern, case_sensitive) {
            Ok(regex) => compiled.push(regex),
            Err(error) => {
                errors.push(InvalidRegexPattern {
                    pattern: rule.pattern,
                    rule_type: rule_type.to_string(),
                    error: error.to_string(),
                });
            }
        }
    }

    (compiled, errors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, UpdateGlobal, px};
    use serde_json::json;
    use settings::ToolPermissionMode;
    use settings::ToolPermissionsContent;
    use std::path::PathBuf;

    #[test]
    fn test_parse_auto_compact_threshold() {
        use AutoCompactThreshold::*;

        assert_eq!(
            parse_auto_compact_threshold("90%").unwrap(),
            Percentage(0.9)
        );
        assert_eq!(AutoCompactThreshold::DEFAULT, Percentage(0.9));
        assert_eq!(
            parse_auto_compact_threshold("  92.5% ").unwrap(),
            Percentage(0.925)
        );
        assert_eq!(
            parse_auto_compact_threshold("95.5%").unwrap(),
            Percentage(0.955)
        );
        assert_eq!(
            parse_auto_compact_threshold("100%").unwrap(),
            Percentage(1.0)
        );
        // Token counts must be integers; a non-integer token value is invalid.
        assert!(parse_auto_compact_threshold("100.5").is_err());
        assert_eq!(
            parse_auto_compact_threshold("100000").unwrap(),
            TokensUsed(100_000)
        );
        assert_eq!(
            parse_auto_compact_threshold("-20000").unwrap(),
            TokensRemaining(20_000)
        );

        assert_eq!(Percentage(0.9).to_string(), "90%");
        assert_eq!(Percentage(0.925).to_string(), "92.5%");
        assert_eq!(TokensUsed(100_000).to_string(), "100000");
        assert_eq!(TokensRemaining(20_000).to_string(), "-20000");

        // 0 is invalid in every form.
        assert!(parse_auto_compact_threshold("0").is_err());
        assert!(parse_auto_compact_threshold("0%").is_err());
        // Out-of-range percentages and bare decimals are invalid.
        assert!(parse_auto_compact_threshold("150%").is_err());
        assert!(parse_auto_compact_threshold("0.8").is_err());
        assert!(parse_auto_compact_threshold("eighty percent").is_err());
    }

    #[test]
    fn test_compiled_regex_case_insensitive() {
        let regex = CompiledRegex::new("rm\\s+-rf", false).unwrap();
        assert!(regex.is_match("rm -rf /"));
        assert!(regex.is_match("RM -RF /"));
        assert!(regex.is_match("Rm -Rf /"));
    }

    #[test]
    fn test_compiled_regex_case_sensitive() {
        let regex = CompiledRegex::new("DROP\\s+TABLE", true).unwrap();
        assert!(regex.is_match("DROP TABLE users"));
        assert!(!regex.is_match("drop table users"));
    }

    #[test]
    fn test_invalid_regex_returns_none() {
        let result = CompiledRegex::new("[invalid(regex", false);
        assert!(result.is_none());
    }

    #[gpui::test]
    fn test_prevent_idle_sleep_defaults_to_true_and_follows_user_settings(cx: &mut gpui::App) {
        let store = SettingsStore::test(cx);
        cx.set_global(store);
        project::DisableAiSettings::register(cx);
        AgentSettings::register(cx);
        assert!(AgentSettings::get_global(cx).prevent_idle_sleep);

        for (content, expected) in [
            (r#"{"agent": {"prevent_idle_sleep": false}}"#, false),
            (r#"{"agent": {"prevent_idle_sleep": true}}"#, true),
            (r#"{"agent": {"prevent_idle_sleep": null}}"#, true),
            (r#"{"agent": {}}"#, true),
        ] {
            SettingsStore::update_global(cx, |store, cx| {
                store
                    .set_user_settings(content, cx)
                    .expect("user settings should load");
            });
            assert_eq!(
                AgentSettings::get_global(cx).prevent_idle_sleep,
                expected,
                "{content}"
            );
        }
    }

    #[gpui::test]
    fn test_threads_sidebar_default_width(cx: &mut gpui::App) {
        let store = SettingsStore::test(cx);
        cx.set_global(store);
        project::DisableAiSettings::register(cx);
        AgentSettings::register(cx);

        assert_eq!(
            AgentSettings::get_global(cx).threads_sidebar_default_width,
            px(300.),
            "default.json supplies the Threads Sidebar width"
        );

        SettingsStore::update_global(cx, |store, cx| {
            store
                .set_user_settings(
                    r#"{ "agent": { "threads_sidebar_default_width": 360 } }"#,
                    cx,
                )
                .expect("user settings load");
        });

        let settings = AgentSettings::get_global(cx);
        assert_eq!(settings.threads_sidebar_default_width, px(360.));
        assert_eq!(
            settings.default_width,
            px(640.),
            "setting the Threads Sidebar width leaves the agent panel width unchanged"
        );
        assert_eq!(
            settings.sidebar_side,
            SidebarDockPosition::Left,
            "setting the Threads Sidebar width leaves its position unchanged"
        );

        for content in [
            r#"{ "agent": { "threads_sidebar_default_width": null } }"#,
            r#"{ "agent": {} }"#,
        ] {
            SettingsStore::update_global(cx, |store, cx| {
                store
                    .set_user_settings(content, cx)
                    .expect("user settings load");
            });
            assert_eq!(
                AgentSettings::get_global(cx).threads_sidebar_default_width,
                px(300.),
                "an unset width falls back to the default"
            );
        }
    }

    #[gpui::test]
    fn test_terminal_init_command_filters_empty_without_trimming(cx: &mut gpui::App) {
        let store = SettingsStore::test(cx);
        cx.set_global(store);
        project::DisableAiSettings::register(cx);
        AgentSettings::register(cx);

        SettingsStore::update_global(cx, |store, cx| {
            let new_text = store
                .new_text_for_update("{}".to_string(), |settings| {
                    settings.agent.get_or_insert_default().terminal_init_command =
                        Some(" claude --resume ".to_string());
                })
                .unwrap();
            assert!(
                new_text.contains(r#""terminal_init_command": " claude --resume ""#),
                "updated settings JSON should include terminal_init_command, got {new_text}"
            );
            store.set_user_settings(&new_text, cx).unwrap();
        });
        assert_eq!(
            AgentSettings::get_global(cx)
                .terminal_init_command
                .as_deref(),
            Some(" claude --resume ")
        );

        SettingsStore::update_global(cx, |store, cx| {
            store
                .set_user_settings(r#"{ "agent": { "terminal_init_command": "   " } }"#, cx)
                .unwrap();
        });
        assert!(
            AgentSettings::get_global(cx)
                .terminal_init_command
                .is_none()
        );

        SettingsStore::update_global(cx, |store, cx| {
            store
                .set_user_settings(r#"{ "agent": { "terminal_init_command": null } }"#, cx)
                .unwrap();
        });
        assert!(
            AgentSettings::get_global(cx)
                .terminal_init_command
                .is_none()
        );
    }

    /// ADR-F49 (new decision): the minutes are the user's own number, so the
    /// grace has to survive one no one meant literally.
    #[gpui::test]
    fn test_a_huge_idle_disconnect_is_not_an_overflow(cx: &mut gpui::App) {
        let store = SettingsStore::test(cx);
        cx.set_global(store);
        project::DisableAiSettings::register(cx);
        AgentSettings::register(cx);

        SettingsStore::update_global(cx, |store, cx| {
            store
                .set_user_settings(
                    r#"{ "agent": { "idle_disconnect_minutes": 18446744073709551615 } }"#,
                    cx,
                )
                .unwrap();
        });
        assert_eq!(
            AgentSettings::get_global(cx).idle_disconnect(),
            Some(std::time::Duration::from_secs(u64::MAX)),
        );

        SettingsStore::update_global(cx, |store, cx| {
            store
                .set_user_settings(r#"{ "agent": { "idle_disconnect_minutes": 0 } }"#, cx)
                .unwrap();
        });
        assert_eq!(AgentSettings::get_global(cx).idle_disconnect(), None);
    }

    #[test]
    fn test_tool_permissions_parsing() {
        let json = json!({
            "tools": {
                "terminal": {
                    "default": "allow",
                    "always_deny": [
                        { "pattern": "rm\\s+-rf" }
                    ],
                    "always_allow": [
                        { "pattern": "^git\\s" }
                    ]
                }
            }
        });

        let content: ToolPermissionsContent = serde_json::from_value(json).unwrap();
        let permissions = compile_tool_permissions(Some(content));

        let terminal_rules = permissions.tools.get("terminal").unwrap();
        assert_eq!(terminal_rules.default, Some(ToolPermissionMode::Allow));
        assert_eq!(terminal_rules.always_deny.len(), 1);
        assert_eq!(terminal_rules.always_allow.len(), 1);
        assert!(terminal_rules.always_deny[0].is_match("rm -rf /"));
        assert!(terminal_rules.always_allow[0].is_match("git status"));
    }

    #[test]
    fn test_tool_rules_default() {
        let json = json!({
            "tools": {
                "edit_file": {
                    "default": "deny"
                }
            }
        });

        let content: ToolPermissionsContent = serde_json::from_value(json).unwrap();
        let permissions = compile_tool_permissions(Some(content));

        let rules = permissions.tools.get("edit_file").unwrap();
        assert_eq!(rules.default, Some(ToolPermissionMode::Deny));
    }

    #[test]
    fn test_tool_permissions_empty() {
        let permissions = compile_tool_permissions(None);
        assert!(permissions.tools.is_empty());
        assert_eq!(permissions.default, ToolPermissionMode::Confirm);
    }

    // The agent panel's "auto approve" switch writes exactly this: the global
    // default becomes `allow`, with no per-tool rules of its own. Anything the
    // user configured per tool has to survive the switch.
    #[test]
    fn test_tool_permissions_auto_approve_default() {
        let content: ToolPermissionsContent =
            serde_json::from_value(json!({ "default": "allow" })).unwrap();
        let permissions = compile_tool_permissions(Some(content));
        assert_eq!(permissions.default, ToolPermissionMode::Allow);
        assert!(permissions.tools.is_empty());

        let content: ToolPermissionsContent = serde_json::from_value(json!({
            "default": "allow",
            "tools": {
                "terminal": {
                    "always_confirm": [{ "pattern": "^sudo\\s" }]
                }
            }
        }))
        .unwrap();
        let permissions = compile_tool_permissions(Some(content));
        assert_eq!(permissions.default, ToolPermissionMode::Allow);
        let terminal_rules = permissions.tools.get("terminal").unwrap();
        assert_eq!(terminal_rules.default, None);
        assert_eq!(terminal_rules.always_confirm.len(), 1);
    }

    // ADR-F52: the judged mode reaches the compiled permissions as a global
    // default and as a tool-specific one, without disturbing anything the user
    // configured per tool.
    #[test]
    fn test_tool_permissions_judged_default() {
        let content: ToolPermissionsContent = serde_json::from_value(json!({
            "default": "auto",
            "tools": {
                "terminal": {
                    "always_deny": [{ "pattern": "^curl " }]
                },
                "edit_file": {
                    "default": "auto"
                }
            }
        }))
        .unwrap();
        let permissions = compile_tool_permissions(Some(content));

        assert_eq!(permissions.default, ToolPermissionMode::Auto);
        let terminal_rules = permissions.tools.get("terminal").unwrap();
        assert_eq!(terminal_rules.default, None);
        assert_eq!(terminal_rules.always_deny.len(), 1);
        assert_eq!(
            permissions.tools.get("edit_file").unwrap().default,
            Some(ToolPermissionMode::Auto)
        );
    }

    #[test]
    fn test_sandbox_permissions_empty() {
        let permissions = compile_sandbox_permissions(None);
        assert_eq!(permissions, SandboxPermissions::default());
        // The confusable-Unicode warning is a safety net, so it's on by default.
        assert!(permissions.warn_confusable_unicode);
    }

    #[test]
    fn test_sandbox_permissions_warn_confusable_unicode_can_be_disabled() {
        let content: settings::SandboxPermissionsContent =
            serde_json::from_value(json!({ "warn_confusable_unicode": false })).unwrap();
        let permissions = compile_sandbox_permissions(Some(content));
        assert!(!permissions.warn_confusable_unicode);

        // Omitting the key keeps the warning enabled.
        let content: settings::SandboxPermissionsContent =
            serde_json::from_value(json!({})).unwrap();
        let permissions = compile_sandbox_permissions(Some(content));
        assert!(permissions.warn_confusable_unicode);
    }

    #[test]
    fn test_sandbox_permissions_parsing_and_pruning() {
        let json = json!({
            "allow_all_hosts": true,
            "network_hosts": ["github.com", "*.npmjs.org"],
            "allow_unsandboxed": true,
            "write_paths": [
                "/tmp/build/cache",
                "/tmp/build",
                "/var/log"
            ]
        });

        let content: settings::SandboxPermissionsContent = serde_json::from_value(json).unwrap();
        let permissions = compile_sandbox_permissions(Some(content));

        assert!(permissions.allow_all_hosts);
        assert_eq!(
            permissions.network_hosts,
            vec!["github.com".to_string(), "*.npmjs.org".to_string()]
        );
        assert!(!permissions.allow_fs_write_all);
        assert!(permissions.allow_unsandboxed);
        assert_eq!(
            permissions.write_paths,
            vec![
                settings::GrantedWritePath::from_requested(PathBuf::from("/tmp/build")),
                settings::GrantedWritePath::from_requested(PathBuf::from("/var/log")),
            ]
        );
    }

    #[test]
    fn test_sandbox_permissions_normalizes_and_prunes_parent_traversal() {
        let json = json!({
            "write_paths": [
                "/tmp/build/../build/cache",
                "/tmp/build",
            ]
        });

        let content: settings::SandboxPermissionsContent = serde_json::from_value(json).unwrap();
        let permissions = compile_sandbox_permissions(Some(content));

        // `/tmp/build/../build/cache` normalizes to `/tmp/build/cache`, which is
        // then pruned as a redundant child of `/tmp/build`.
        assert_eq!(
            permissions.write_paths,
            vec![settings::GrantedWritePath::from_requested(PathBuf::from(
                "/tmp/build"
            ))]
        );
    }

    #[test]
    fn test_sandbox_permissions_bare_string_has_no_resolved() {
        let json = json!({
            "write_paths": ["/tmp/build"]
        });

        let content: settings::SandboxPermissionsContent = serde_json::from_value(json).unwrap();
        let permissions = compile_sandbox_permissions(Some(content));

        assert_eq!(
            permissions.write_paths,
            vec![settings::GrantedWritePath::from_requested(PathBuf::from(
                "/tmp/build"
            ))]
        );
        assert_eq!(permissions.write_paths[0].resolved, None);
    }

    #[test]
    fn test_sandbox_permissions_object_preserves_resolved() {
        let json = json!({
            "write_paths": [
                { "requested": "/tmp/link", "resolved": "/tmp/real" }
            ]
        });

        let content: settings::SandboxPermissionsContent = serde_json::from_value(json).unwrap();
        let permissions = compile_sandbox_permissions(Some(content));

        assert_eq!(
            permissions.write_paths,
            vec![settings::GrantedWritePath::resolved(
                PathBuf::from("/tmp/link"),
                PathBuf::from("/tmp/real"),
            )]
        );
        assert_eq!(
            permissions.write_paths[0].resolved,
            Some(PathBuf::from("/tmp/real"))
        );
    }

    #[test]
    fn test_sandbox_permissions_dedup_keys_on_resolved_path() {
        // The requested paths are unrelated, but the resolved (canonical)
        // targets form a subtree, so dedup must prune by the resolved path.
        let json = json!({
            "write_paths": [
                { "requested": "/tmp/link/cache", "resolved": "/tmp/real/cache" },
                { "requested": "/tmp/other", "resolved": "/tmp/real" },
            ]
        });

        let content: settings::SandboxPermissionsContent = serde_json::from_value(json).unwrap();
        let permissions = compile_sandbox_permissions(Some(content));

        assert_eq!(
            permissions.write_paths,
            vec![settings::GrantedWritePath::resolved(
                PathBuf::from("/tmp/other"),
                PathBuf::from("/tmp/real"),
            )]
        );
    }

    /// ADR-F49 decision 34: a grant saved before the distribution's name was
    /// folded to lower case names the same folder as one saved after, so it
    /// covers it — no second approval — and the two rows become one.
    #[test]
    fn test_sandbox_permissions_fold_wsl_distribution_spellings() {
        let json = json!({
            "write_paths": [
                { "requested": "/tmp/x", "resolved": r"\\wsl.localhost\Ubuntu\tmp\x" },
                { "requested": "/tmp/x", "resolved": r"\\wsl.localhost\ubuntu\tmp\x" },
                r"\\wsl$\Ubuntu\home\u\build",
            ]
        });

        let content: settings::SandboxPermissionsContent = serde_json::from_value(json).unwrap();
        let permissions = compile_sandbox_permissions(Some(content));

        assert_eq!(
            permissions.write_paths,
            vec![
                settings::GrantedWritePath::resolved(
                    PathBuf::from("/tmp/x"),
                    PathBuf::from(r"\\wsl.localhost\ubuntu\tmp\x"),
                ),
                settings::GrantedWritePath::from_requested(PathBuf::from(
                    r"\\wsl.localhost\ubuntu\home\u\build"
                )),
            ]
        );
    }

    /// ADR-F49 決定 40 の残: folding has to run *after* lexical normalization.
    /// `canonical_wsl_grant_spelling` leaves any path containing `..`
    /// unchanged, so folding it first on `\\wsl$\Ubuntu\home\..\tmp` is a
    /// no-op and only `normalize_lexically` afterwards would resolve the
    /// `..` — leaving the old `\\wsl$\` spelling stored instead of the
    /// folded one.
    ///
    /// `normalize_lexically` reads a path with the host's rules, and only
    /// Windows splits a `\\server\share\…` spelling into components, so a row
    /// with a `..` in it is only folded on Windows. Elsewhere the spelling is
    /// one opaque component and the row stays as it was written — what it did
    /// before this order change, and no wider than it (a `\\wsl…` row never
    /// names a place a Unix host can write).
    #[test]
    fn test_sandbox_permissions_fold_wsl_grant_with_parent_dir_component() {
        let json = json!({
            "write_paths": [
                r"\\wsl$\Ubuntu\home\..\tmp",
            ]
        });

        let content: settings::SandboxPermissionsContent = serde_json::from_value(json).unwrap();
        let permissions = compile_sandbox_permissions(Some(content));

        let expected = if cfg!(target_os = "windows") {
            r"\\wsl.localhost\ubuntu\tmp"
        } else {
            r"\\wsl$\Ubuntu\home\..\tmp"
        };
        assert_eq!(
            permissions.write_paths,
            vec![settings::GrantedWritePath::from_requested(PathBuf::from(
                expected
            ))]
        );
    }

    /// ADR-F49 決定 40 の残: normalizing before folding must not let a `..`
    /// carry a grant out of the distribution it was approved in. Each of these
    /// leaves that distribution's root — into a second distribution, into the
    /// share root, or above it — so `normalize_lexically` refuses it and the
    /// row is dropped, exactly as it was when folding ran first. Windows only:
    /// elsewhere the spelling is one opaque component, stored unchanged.
    #[cfg(target_os = "windows")]
    #[test]
    fn test_sandbox_permissions_drops_wsl_grants_leaving_their_distribution() {
        for escaping in [
            r"\\wsl$\Ubuntu\..\Debian\home",
            r"\\wsl$\Ubuntu\..\..",
            r"\\wsl.localhost\Ubuntu\home\..\..\..\etc",
            r"\\wsl.localhost\Ubuntu\home\u\..\..\..\Debian\home\u",
        ] {
            let json = json!({ "write_paths": [escaping] });
            let content: settings::SandboxPermissionsContent =
                serde_json::from_value(json).unwrap();

            assert_eq!(
                compile_sandbox_permissions(Some(content)).write_paths,
                vec![],
                "{escaping}"
            );
        }
    }

    /// ADR-F49 決定 40: a `..` that only walks back to the distribution's own
    /// root leaves a bare share, which `canonical_wsl_grant_spelling` does not
    /// fold, so the row keeps naming that root in the spelling it was written
    /// in — it never turns into a grant on a different distribution.
    #[cfg(target_os = "windows")]
    #[test]
    fn test_sandbox_permissions_keeps_wsl_distribution_root_unfolded() {
        let json = json!({ "write_paths": [r"\\wsl$\Ubuntu\home\.."] });
        let content: settings::SandboxPermissionsContent = serde_json::from_value(json).unwrap();

        assert_eq!(
            compile_sandbox_permissions(Some(content)).write_paths,
            vec![settings::GrantedWritePath::from_requested(PathBuf::from(
                r"\\wsl$\Ubuntu\"
            ))]
        );
    }

    #[test]
    fn test_tool_rules_default_returns_confirm() {
        let default_rules = ToolRules::default();
        assert_eq!(default_rules.default, None);
        assert!(default_rules.always_allow.is_empty());
        assert!(default_rules.always_deny.is_empty());
        assert!(default_rules.always_confirm.is_empty());
    }

    #[test]
    fn test_tool_permissions_with_multiple_tools() {
        let json = json!({
            "tools": {
                "terminal": {
                    "default": "allow",
                    "always_deny": [{ "pattern": "rm\\s+-rf" }]
                },
                "edit_file": {
                    "default": "confirm",
                    "always_deny": [{ "pattern": "\\.env$" }]
                },
                "delete_path": {
                    "default": "deny"
                }
            }
        });

        let content: ToolPermissionsContent = serde_json::from_value(json).unwrap();
        let permissions = compile_tool_permissions(Some(content));

        assert_eq!(permissions.tools.len(), 3);

        let terminal = permissions.tools.get("terminal").unwrap();
        assert_eq!(terminal.default, Some(ToolPermissionMode::Allow));
        assert_eq!(terminal.always_deny.len(), 1);

        let edit_file = permissions.tools.get("edit_file").unwrap();
        assert_eq!(edit_file.default, Some(ToolPermissionMode::Confirm));
        assert!(edit_file.always_deny[0].is_match("secrets.env"));

        let delete_path = permissions.tools.get("delete_path").unwrap();
        assert_eq!(delete_path.default, Some(ToolPermissionMode::Deny));
    }

    #[test]
    fn test_tool_permissions_with_all_rule_types() {
        let json = json!({
            "tools": {
                "terminal": {
                    "always_deny": [{ "pattern": "rm\\s+-rf" }],
                    "always_confirm": [{ "pattern": "sudo\\s" }],
                    "always_allow": [{ "pattern": "^git\\s+status" }]
                }
            }
        });

        let content: ToolPermissionsContent = serde_json::from_value(json).unwrap();
        let permissions = compile_tool_permissions(Some(content));

        let terminal = permissions.tools.get("terminal").unwrap();
        assert_eq!(terminal.always_deny.len(), 1);
        assert_eq!(terminal.always_confirm.len(), 1);
        assert_eq!(terminal.always_allow.len(), 1);

        assert!(terminal.always_deny[0].is_match("rm -rf /"));
        assert!(terminal.always_confirm[0].is_match("sudo apt install"));
        assert!(terminal.always_allow[0].is_match("git status"));
    }

    #[test]
    fn test_invalid_regex_is_tracked_and_valid_ones_still_compile() {
        let json = json!({
            "tools": {
                "terminal": {
                    "always_deny": [
                        { "pattern": "[invalid(regex" },
                        { "pattern": "valid_pattern" }
                    ],
                    "always_allow": [
                        { "pattern": "[another_bad" }
                    ]
                }
            }
        });

        let content: ToolPermissionsContent = serde_json::from_value(json).unwrap();
        let permissions = compile_tool_permissions(Some(content));

        let terminal = permissions.tools.get("terminal").unwrap();

        // Valid patterns should still be compiled
        assert_eq!(terminal.always_deny.len(), 1);
        assert!(terminal.always_deny[0].is_match("valid_pattern"));

        // Invalid patterns should be tracked (order depends on processing order)
        assert_eq!(terminal.invalid_patterns.len(), 2);

        let deny_invalid = terminal
            .invalid_patterns
            .iter()
            .find(|p| p.rule_type == "always_deny")
            .expect("should have invalid pattern from always_deny");
        assert_eq!(deny_invalid.pattern, "[invalid(regex");
        assert!(!deny_invalid.error.is_empty());

        let allow_invalid = terminal
            .invalid_patterns
            .iter()
            .find(|p| p.rule_type == "always_allow")
            .expect("should have invalid pattern from always_allow");
        assert_eq!(allow_invalid.pattern, "[another_bad");

        // ToolPermissions helper methods should work
        assert!(permissions.has_invalid_patterns());
        assert_eq!(permissions.invalid_patterns().len(), 2);
    }

    #[test]
    fn test_deny_takes_precedence_over_allow_and_confirm() {
        let json = json!({
            "tools": {
                "terminal": {
                    "default": "allow",
                    "always_deny": [{ "pattern": "dangerous" }],
                    "always_confirm": [{ "pattern": "dangerous" }],
                    "always_allow": [{ "pattern": "dangerous" }]
                }
            }
        });

        let content: ToolPermissionsContent = serde_json::from_value(json).unwrap();
        let permissions = compile_tool_permissions(Some(content));
        let terminal = permissions.tools.get("terminal").unwrap();

        assert!(
            terminal.always_deny[0].is_match("run dangerous command"),
            "Deny rule should match"
        );
        assert!(
            terminal.always_allow[0].is_match("run dangerous command"),
            "Allow rule should also match (but deny takes precedence at evaluation time)"
        );
        assert!(
            terminal.always_confirm[0].is_match("run dangerous command"),
            "Confirm rule should also match (but deny takes precedence at evaluation time)"
        );
    }

    #[test]
    fn test_confirm_takes_precedence_over_allow() {
        let json = json!({
            "tools": {
                "terminal": {
                    "default": "allow",
                    "always_confirm": [{ "pattern": "risky" }],
                    "always_allow": [{ "pattern": "risky" }]
                }
            }
        });

        let content: ToolPermissionsContent = serde_json::from_value(json).unwrap();
        let permissions = compile_tool_permissions(Some(content));
        let terminal = permissions.tools.get("terminal").unwrap();

        assert!(
            terminal.always_confirm[0].is_match("do risky thing"),
            "Confirm rule should match"
        );
        assert!(
            terminal.always_allow[0].is_match("do risky thing"),
            "Allow rule should also match (but confirm takes precedence at evaluation time)"
        );
    }

    #[test]
    fn test_regex_matches_anywhere_in_string_not_just_anchored() {
        let json = json!({
            "tools": {
                "terminal": {
                    "always_deny": [
                        { "pattern": "rm\\s+-rf" },
                        { "pattern": "/etc/passwd" }
                    ]
                }
            }
        });

        let content: ToolPermissionsContent = serde_json::from_value(json).unwrap();
        let permissions = compile_tool_permissions(Some(content));
        let terminal = permissions.tools.get("terminal").unwrap();

        assert!(
            terminal.always_deny[0].is_match("echo hello && rm -rf /"),
            "Should match rm -rf in the middle of a command chain"
        );
        assert!(
            terminal.always_deny[0].is_match("cd /tmp; rm -rf *"),
            "Should match rm -rf after semicolon"
        );
        assert!(
            terminal.always_deny[1].is_match("cat /etc/passwd | grep root"),
            "Should match /etc/passwd in a pipeline"
        );
        assert!(
            terminal.always_deny[1].is_match("vim /etc/passwd"),
            "Should match /etc/passwd as argument"
        );
    }

    #[test]
    fn test_fork_bomb_pattern_matches() {
        let fork_bomb_regex = CompiledRegex::new(r":\(\)\{\s*:\|:&\s*\};:", false).unwrap();
        assert!(
            fork_bomb_regex.is_match(":(){ :|:& };:"),
            "Should match the classic fork bomb"
        );
        assert!(
            fork_bomb_regex.is_match(":(){ :|:&};:"),
            "Should match fork bomb without spaces"
        );
    }

    #[test]
    fn test_compiled_regex_stores_case_sensitivity() {
        let case_sensitive = CompiledRegex::new("test", true).unwrap();
        let case_insensitive = CompiledRegex::new("test", false).unwrap();

        assert!(case_sensitive.case_sensitive);
        assert!(!case_insensitive.case_sensitive);
    }

    #[test]
    fn test_invalid_regex_is_skipped_not_fail() {
        let json = json!({
            "tools": {
                "terminal": {
                    "always_deny": [
                        { "pattern": "[invalid(regex" },
                        { "pattern": "valid_pattern" }
                    ]
                }
            }
        });

        let content: ToolPermissionsContent = serde_json::from_value(json).unwrap();
        let permissions = compile_tool_permissions(Some(content));

        let terminal = permissions.tools.get("terminal").unwrap();
        assert_eq!(terminal.always_deny.len(), 1);
        assert!(terminal.always_deny[0].is_match("valid_pattern"));
    }

    #[test]
    fn test_unconfigured_tool_not_in_permissions() {
        let json = json!({
            "tools": {
                "terminal": {
                    "default": "allow"
                }
            }
        });

        let content: ToolPermissionsContent = serde_json::from_value(json).unwrap();
        let permissions = compile_tool_permissions(Some(content));

        assert!(permissions.tools.contains_key("terminal"));
        assert!(!permissions.tools.contains_key("edit_file"));
        assert!(!permissions.tools.contains_key("fetch"));
    }

    #[test]
    fn test_always_allow_pattern_only_matches_specified_commands() {
        // Reproduces user-reported bug: when always_allow has pattern "^echo\s",
        // only "echo hello" should be allowed, not "git status".
        //
        // User config:
        //   always_allow_tool_actions: false
        //   tool_permissions.tools.terminal.always_allow: [{ pattern: "^echo\\s" }]
        let json = json!({
            "tools": {
                "terminal": {
                    "always_allow": [
                        { "pattern": "^echo\\s" }
                    ]
                }
            }
        });

        let content: ToolPermissionsContent = serde_json::from_value(json).unwrap();
        let permissions = compile_tool_permissions(Some(content));

        let terminal = permissions.tools.get("terminal").unwrap();

        // Verify the pattern was compiled
        assert_eq!(
            terminal.always_allow.len(),
            1,
            "Should have one always_allow pattern"
        );

        // Verify the pattern matches "echo hello"
        assert!(
            terminal.always_allow[0].is_match("echo hello"),
            "Pattern ^echo\\s should match 'echo hello'"
        );

        // Verify the pattern does NOT match "git status"
        assert!(
            !terminal.always_allow[0].is_match("git status"),
            "Pattern ^echo\\s should NOT match 'git status'"
        );

        // Verify the pattern does NOT match "echoHello" (no space)
        assert!(
            !terminal.always_allow[0].is_match("echoHello"),
            "Pattern ^echo\\s should NOT match 'echoHello' (requires whitespace)"
        );

        assert_eq!(
            terminal.default, None,
            "default should be None when not specified"
        );
    }

    #[test]
    fn test_empty_regex_pattern_is_invalid() {
        let json = json!({
            "tools": {
                "terminal": {
                    "always_allow": [
                        { "pattern": "" }
                    ],
                    "always_deny": [
                        { "case_sensitive": true }
                    ],
                    "always_confirm": [
                        { "pattern": "" },
                        { "pattern": "valid_pattern" }
                    ]
                }
            }
        });

        let content: ToolPermissionsContent = serde_json::from_value(json).unwrap();
        let permissions = compile_tool_permissions(Some(content));

        let terminal = permissions.tools.get("terminal").unwrap();

        assert_eq!(terminal.always_allow.len(), 0);
        assert_eq!(terminal.always_deny.len(), 0);
        assert_eq!(terminal.always_confirm.len(), 1);
        assert!(terminal.always_confirm[0].is_match("valid_pattern"));

        assert_eq!(terminal.invalid_patterns.len(), 3);
        for invalid in &terminal.invalid_patterns {
            assert_eq!(invalid.pattern, "");
            assert!(invalid.error.contains("empty"));
        }
    }

    #[test]
    fn test_default_json_tool_permissions_parse() {
        let default_json = include_str!("../../../assets/settings/default.json");
        let value: serde_json_lenient::Value = serde_json_lenient::from_str(default_json).unwrap();
        let agent = value
            .get("agent")
            .expect("default.json should have 'agent' key");
        let tool_permissions_value = agent
            .get("tool_permissions")
            .expect("agent should have 'tool_permissions' key");

        let content: ToolPermissionsContent =
            serde_json_lenient::from_value(tool_permissions_value.clone()).unwrap();
        let permissions = compile_tool_permissions(Some(content));

        assert_eq!(permissions.default, ToolPermissionMode::Confirm);

        assert!(
            permissions.tools.is_empty(),
            "default.json should not have any active tool-specific rules, found: {:?}",
            permissions.tools.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_tool_permissions_explicit_global_default() {
        let json_allow = json!({
            "default": "allow"
        });
        let content: ToolPermissionsContent = serde_json::from_value(json_allow).unwrap();
        let permissions = compile_tool_permissions(Some(content));
        assert_eq!(permissions.default, ToolPermissionMode::Allow);

        let json_deny = json!({
            "default": "deny"
        });
        let content: ToolPermissionsContent = serde_json::from_value(json_deny).unwrap();
        let permissions = compile_tool_permissions(Some(content));
        assert_eq!(permissions.default, ToolPermissionMode::Deny);
    }

    #[gpui::test]
    fn test_get_layout(cx: &mut gpui::App) {
        let store = SettingsStore::test(cx);
        cx.set_global(store);
        project::DisableAiSettings::register(cx);
        AgentSettings::register(cx);

        // Should be Agent with an empty user layout (user hasn't customized).
        let layout = AgentSettings::get_layout(cx);
        let WindowLayout::Agent(Some(user_layout)) = layout else {
            panic!("expected Agent(Some), got {:?}", layout);
        };
        assert_eq!(user_layout, PanelLayout::default());

        // User explicitly sets agent dock to left (matching the default).
        // The merged result is still agent, but the user layout captures
        // only what the user wrote.
        SettingsStore::update_global(cx, |store, cx| {
            store
                .set_user_settings(r#"{ "agent": { "dock": "left" } }"#, cx)
                .unwrap();
        });

        let layout = AgentSettings::get_layout(cx);
        let WindowLayout::Agent(Some(user_layout)) = layout else {
            panic!("expected Agent(Some), got {:?}", layout);
        };
        assert_eq!(user_layout.agent_dock, Some(DockPosition::Left));
        assert_eq!(user_layout.project_panel_dock, None);
        assert_eq!(user_layout.outline_panel_dock, None);
        assert_eq!(user_layout.collaboration_panel_dock, None);
        assert_eq!(user_layout.git_panel_dock, None);

        // User sets a combination that doesn't match either preset:
        // agent on the left but project panel also on the left.
        SettingsStore::update_global(cx, |store, cx| {
            store
                .set_user_settings(
                    r#"{
                        "agent": { "dock": "left" },
                        "project_panel": { "dock": "left" }
                    }"#,
                    cx,
                )
                .unwrap();
        });

        let layout = AgentSettings::get_layout(cx);
        let WindowLayout::Custom(user_layout) = layout else {
            panic!("expected Custom, got {:?}", layout);
        };
        assert_eq!(user_layout.agent_dock, Some(DockPosition::Left));
        assert_eq!(user_layout.project_panel_dock, Some(DockSide::Left));
    }

    #[gpui::test]
    fn test_set_layout_round_trip(cx: &mut gpui::App) {
        let store = SettingsStore::test(cx);
        cx.set_global(store);
        project::DisableAiSettings::register(cx);
        AgentSettings::register(cx);

        // User has a custom layout: agent on the right with project panel
        // also on the right. This doesn't match either preset.
        SettingsStore::update_global(cx, |store, cx| {
            store
                .set_user_settings(
                    r#"{
                        "agent": { "dock": "right" },
                        "project_panel": { "dock": "right" }
                    }"#,
                    cx,
                )
                .unwrap();
        });

        let original = AgentSettings::get_layout(cx);
        let WindowLayout::Custom(ref original_user_layout) = original else {
            panic!("expected Custom, got {:?}", original);
        };
        assert_eq!(original_user_layout.agent_dock, Some(DockPosition::Right));
        assert_eq!(
            original_user_layout.project_panel_dock,
            Some(DockSide::Right)
        );
        assert_eq!(original_user_layout.outline_panel_dock, None);

        // Switch to the agent layout. This overwrites the user settings.
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |settings| {
                PanelLayout::AGENT.write_to(settings);
            });
        });

        let layout = AgentSettings::get_layout(cx);
        assert!(matches!(layout, WindowLayout::Agent(_)));

        // Restore the original custom layout.
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |settings| {
                original_user_layout.write_to(settings);
            });
        });

        // Should be back to the same custom layout.
        let restored = AgentSettings::get_layout(cx);
        let WindowLayout::Custom(restored_user_layout) = restored else {
            panic!("expected Custom, got {:?}", restored);
        };
        assert_eq!(restored_user_layout.agent_dock, Some(DockPosition::Right));
        assert_eq!(
            restored_user_layout.project_panel_dock,
            Some(DockSide::Right)
        );
        assert_eq!(restored_user_layout.outline_panel_dock, None);
    }

    #[gpui::test]
    async fn test_set_layout_minimal_diff(cx: &mut TestAppContext) {
        let fs = fs::FakeFs::new(cx.background_executor.clone());
        fs.save(
            paths::settings_file().as_path(),
            &serde_json::json!({
                "agent": { "dock": "left" },
                "project_panel": { "dock": "left" }
            })
            .to_string()
            .into(),
            Default::default(),
        )
        .await
        .unwrap();

        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            project::DisableAiSettings::register(cx);
            AgentSettings::register(cx);

            // User has agent=left (matches preset) and project_panel=left (does not)
            SettingsStore::update_global(cx, |store, cx| {
                store
                    .set_user_settings(
                        r#"{
                            "agent": { "dock": "left" },
                            "project_panel": { "dock": "left" }
                        }"#,
                        cx,
                    )
                    .unwrap();
            });

            let layout = AgentSettings::get_layout(cx);
            assert!(matches!(layout, WindowLayout::Custom(_)));

            AgentSettings::set_layout(WindowLayout::agent(), fs.clone(), cx)
        })
        .await
        .ok();

        cx.run_until_parked();

        let written = fs.load(paths::settings_file().as_path()).await.unwrap();
        cx.update(|cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store.set_user_settings(&written, cx).unwrap();
            });

            // The user settings should still have agent=left (preserved)
            // and now project_panel=right (changed to match preset).
            let store = cx.global::<SettingsStore>();
            let user_layout = store
                .raw_user_settings()
                .map(|u| PanelLayout::read_from(u.content.as_ref()))
                .unwrap_or_default();

            assert_eq!(user_layout.agent_dock, Some(DockPosition::Left));
            assert_eq!(user_layout.project_panel_dock, Some(DockSide::Right));
            // Other fields weren't in user settings and didn't need changing.
            assert_eq!(user_layout.outline_panel_dock, None);

            // And the merged result should now match agent.
            let layout = AgentSettings::get_layout(cx);
            assert!(matches!(layout, WindowLayout::Agent(_)));
        });
    }
}
