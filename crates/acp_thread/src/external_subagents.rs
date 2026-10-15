//! ADR-F97: an external agent's subagents (the ACP subagents RFD, which
//! Claude's adapter speaks) and the shell commands it left running in the
//! background. A child is announced on its parent's session and then has a
//! session of its own, which lives here as a thread of the parent's
//! conversation.

use super::*;

/// The work state the agent reported for a child, as the panel names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExternalSubagentStatus {
    Running,
    /// The child waits for the user: a permission or a question.
    AwaitingUser,
    /// The child's work ended. It can still be given more (SendMessage).
    Idle,
    Failed,
    Cancelled,
    /// The adapter can no longer observe the child.
    Unknown,
}

impl ExternalSubagentStatus {
    pub fn is_working(self) -> bool {
        matches!(self, Self::Running | Self::AwaitingUser)
    }
}

/// One `subagent_update` of the agent, already read off the wire.
#[derive(Clone, Debug)]
pub struct ExternalSubagentReport {
    pub session_id: acp_v1::SessionId,
    pub title: Option<String>,
    pub description: Option<String>,
    pub status: ExternalSubagentStatus,
    pub error: Option<String>,
}

pub struct ExternalSubagent {
    pub session_id: acp_v1::SessionId,
    pub title: Option<SharedString>,
    pub description: Option<SharedString>,
    pub status: ExternalSubagentStatus,
    pub error: Option<SharedString>,
    /// The parent's Agent / Task call that launched the child: the id of the
    /// first message the child was sent.
    pub parent_tool_call_id: Option<acp_v1::ToolCallId>,
    pub work_started_at: Instant,
    pub work_ended_at: Option<Instant>,
    pub thread: Entity<AcpThread>,
}

impl ExternalSubagent {
    /// How long the child has worked, or worked last, when it is not working.
    pub fn elapsed(&self) -> Duration {
        self.work_ended_at
            .unwrap_or_else(Instant::now)
            .saturating_duration_since(self.work_started_at)
    }
}

#[derive(Default)]
pub(crate) struct ExternalSubagents {
    children: Vec<ExternalSubagent>,
    /// On a child's thread: the messages it was already shown, so that a
    /// message the agent sends again (a reloaded session) is not shown twice.
    received_message_ids: HashSet<String>,
}

/// A shell command an agent started in the background. Only that it was
/// started is known: no client but JetBrains AIR is told how it ends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackgroundShellTask {
    pub session_id: acp_v1::SessionId,
    pub tool_call_id: acp_v1::ToolCallId,
    pub label: SharedString,
}

const CLAUDE_CODE_META_KEY: &str = "claudeCode";
const CLAUDE_CODE_TOOL_NAME_KEY: &str = "toolName";
const BACKGROUND_SHELL_LABEL_MAX_CHARS: usize = 80;

/// The name Claude Code gave the tool behind a call (`_meta.claudeCode.toolName`).
pub fn claude_code_tool_name(tool_call: &ToolCall) -> Option<&str> {
    tool_call
        .meta
        .as_ref()?
        .get(CLAUDE_CODE_META_KEY)?
        .get(CLAUDE_CODE_TOOL_NAME_KEY)?
        .as_str()
}

/// Whether a parent's call is the one that launches subagents.
pub fn is_claude_code_subagent_call(tool_call: &ToolCall) -> bool {
    matches!(claude_code_tool_name(tool_call), Some("Agent" | "Task"))
}

/// The model and the effort a parent's Agent / Task call asked for, when it
/// named them. Without them the child runs with its definition's or the
/// parent's, which the client is not told.
pub fn requested_model_and_effort(tool_call: &ToolCall) -> (Option<String>, Option<String>) {
    let input = tool_call.raw_input.as_ref();
    let field = |name: &str| {
        input
            .and_then(|input| input.get(name))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
    };
    (field("model"), field("effort"))
}

fn background_shell_label(tool_call: &ToolCall) -> Option<SharedString> {
    if !matches!(
        claude_code_tool_name(tool_call),
        Some("Bash" | "PowerShell")
    ) {
        return None;
    }
    let input = tool_call.raw_input.as_ref()?;
    if input
        .get("run_in_background")
        .and_then(serde_json::Value::as_bool)
        != Some(true)
    {
        return None;
    }
    if matches!(
        tool_call.status(),
        ToolCallStatus::Failed | ToolCallStatus::Rejected | ToolCallStatus::Canceled
    ) {
        return None;
    }
    let text = |name: &str| {
        input
            .get(name)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
    };
    let label = match text("description") {
        Some(description) => description.to_string(),
        None => {
            let first_line = text("command")?.lines().next()?.trim();
            let mut label: String = first_line
                .chars()
                .take(BACKGROUND_SHELL_LABEL_MAX_CHARS)
                .collect();
            if first_line.chars().count() > BACKGROUND_SHELL_LABEL_MAX_CHARS {
                label.push('…');
            }
            label
        }
    };
    Some(label.into())
}

impl AcpThread {
    pub fn external_subagents(&self) -> &[ExternalSubagent] {
        &self.external_subagents.children
    }

    pub fn external_subagent(&self, session_id: &acp_v1::SessionId) -> Option<&ExternalSubagent> {
        self.external_subagents
            .children
            .iter()
            .find(|child| &child.session_id == session_id)
    }

    pub fn external_subagent_thread(
        &self,
        session_id: &acp_v1::SessionId,
    ) -> Option<&Entity<AcpThread>> {
        self.external_subagent(session_id)
            .map(|child| &child.thread)
    }

    pub fn has_working_external_subagents(&self) -> bool {
        self.external_subagents
            .children
            .iter()
            .any(|child| child.status.is_working())
    }

    /// Applies what the agent reported about a child of this session. The
    /// first report of a child makes its thread, which is returned so the
    /// connection can route the child's session to it; a child reported again
    /// keeps its one thread. A reloaded session is a new thread that is sent
    /// its children's whole history again, so it makes new threads for them
    /// rather than taking ones a previous thread of the session made: those
    /// already show that history, and would show it twice.
    pub fn apply_external_subagent_report(
        &mut self,
        report: ExternalSubagentReport,
        cx: &mut Context<Self>,
    ) -> Option<Entity<AcpThread>> {
        let now = Instant::now();
        let title = report.title.map(SharedString::from);
        let description = report.description.map(SharedString::from);
        let error = report.error.map(SharedString::from);

        if let Some(child) = self
            .external_subagents
            .children
            .iter_mut()
            .find(|child| child.session_id == report.session_id)
        {
            if !child.status.is_working() && report.status.is_working() {
                child.work_started_at = now;
                child.work_ended_at = None;
            } else if child.status.is_working() && !report.status.is_working() {
                child.work_ended_at = Some(now);
            }
            let status_changed = child.status != report.status;
            child.status = report.status;
            child.error = error;
            // The child's own view shows the status its parent holds.
            if status_changed {
                child.thread.update(cx, |_, cx| cx.notify());
            }
            if title.is_some() {
                child.title = title.clone();
            }
            if description.is_some() {
                child.description = description;
            }
            if let Some(title) = title {
                child.thread.update(cx, |thread, cx| {
                    if thread.title.as_ref() != Some(&title) {
                        thread.title = Some(title);
                        cx.emit(AcpThreadEvent::TitleUpdated);
                    }
                });
            }
            cx.notify();
            return None;
        }

        let project = self.project.clone();
        let action_log = cx.new(|_| ActionLog::new(project.clone()));
        let parent_session_id = self.session_id.clone();
        let work_dirs = self.work_dirs.clone();
        let connection = self.connection.clone();
        let prompt_capabilities = self.prompt_capabilities.clone();
        let session_id = report.session_id.clone();
        let thread_title = title.clone().or_else(|| description.clone());
        let thread = cx.new(|cx| {
            AcpThread::new(
                Some(parent_session_id),
                thread_title,
                work_dirs,
                connection,
                project,
                action_log,
                session_id,
                watch::Receiver::constant(prompt_capabilities),
                cx,
            )
        });
        self.external_subagents.children.push(ExternalSubagent {
            session_id: report.session_id.clone(),
            title,
            description,
            status: report.status,
            error,
            parent_tool_call_id: None,
            work_started_at: now,
            work_ended_at: (!report.status.is_working()).then_some(now),
            thread: thread.clone(),
        });
        cx.emit(AcpThreadEvent::SubagentSpawned(report.session_id));
        cx.notify();
        Some(thread)
    }

    /// Shows a message the child was sent (its prompt, or a later
    /// SendMessage) as the child's user message. Returns `false` for a
    /// message it already shows.
    pub fn receive_external_subagent_message(
        &mut self,
        message_id: String,
        content: Vec<acp_v1::ContentBlock>,
        cx: &mut Context<Self>,
    ) -> Result<bool, acp_v1::Error> {
        if !self
            .external_subagents
            .received_message_ids
            .insert(message_id.clone())
        {
            return Ok(false);
        }
        for block in content {
            self.handle_session_update(
                acp_v1::SessionUpdate::UserMessageChunk(
                    acp_v1::ContentChunk::new(block)
                        .message_id(Some(acp_v1::MessageId::new(message_id.clone()))),
                ),
                cx,
            )?;
        }
        Ok(true)
    }

    /// Ties a child to the parent's call whose id the first message this
    /// thread sent it carries, so that the call's card shows the child. A
    /// later message (SendMessage) does not move it. The message can come
    /// before the call itself: the card then shows the child once the call is
    /// there.
    pub fn link_external_subagent_tool_call(
        &mut self,
        session_id: &acp_v1::SessionId,
        tool_call_id: acp_v1::ToolCallId,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(child) = self
            .external_subagents
            .children
            .iter_mut()
            .find(|child| &child.session_id == session_id)
        else {
            return false;
        };
        if child.parent_tool_call_id.is_some() {
            return false;
        }
        child.parent_tool_call_id = Some(tool_call_id.clone());
        self.show_external_subagent_on_card(&tool_call_id, cx);
        cx.notify();
        true
    }

    /// Shows a child on the card of the call that launched it, once both the
    /// child's link and the call are there: called when either arrives.
    pub fn show_external_subagent_on_card(
        &mut self,
        tool_call_id: &acp_v1::ToolCallId,
        cx: &mut Context<Self>,
    ) {
        let Some(session_id) = self
            .external_subagents
            .children
            .iter()
            .find(|child| child.parent_tool_call_id.as_ref() == Some(tool_call_id))
            .map(|child| child.session_id.clone())
        else {
            return;
        };
        let Some((index, tool_call)) = self.tool_call_mut(tool_call_id) else {
            return;
        };
        if tool_call
            .subagent_session_info
            .as_ref()
            .is_some_and(|info| info.session_id == session_id)
        {
            return;
        }
        tool_call.subagent_session_info = Some(SubagentSessionInfo {
            session_id,
            message_start_index: 0,
            message_end_index: None,
        });
        cx.emit(AcpThreadEvent::EntryUpdated(index));
    }

    /// The shell commands this thread and its children started in the
    /// background, oldest first.
    pub fn background_shell_tasks(&self, cx: &App) -> Vec<BackgroundShellTask> {
        let mut tasks = Vec::new();
        self.collect_background_shell_tasks(&mut tasks, cx);
        tasks
    }

    fn collect_background_shell_tasks(&self, tasks: &mut Vec<BackgroundShellTask>, cx: &App) {
        for entry in &self.entries {
            if let AgentThreadEntry::ToolCall(tool_call) = entry
                && let Some(label) = background_shell_label(tool_call)
            {
                tasks.push(BackgroundShellTask {
                    session_id: self.session_id.clone(),
                    tool_call_id: tool_call.id.clone(),
                    label,
                });
            }
        }
        for child in &self.external_subagents.children {
            child
                .thread
                .read(cx)
                .collect_background_shell_tasks(tasks, cx);
        }
    }
}
