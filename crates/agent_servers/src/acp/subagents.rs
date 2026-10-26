//! ADR-F97: Claude's subagents over the ACP subagents RFD.
//!
//! The adapter sends them only to a client that declares
//! `clientCapabilities.subagents`, which the schema crate does not have yet, so
//! the declaration is added to the `initialize` request's JSON here. The two
//! updates the RFD adds to `session/update` (`subagent_update` on the parent's
//! session, `session_message` on the child's) are not variants of
//! `SessionUpdate` either, and the schema crate fails to read a
//! `session/update` that carries one. So `session/update` is read here first:
//! those two kinds by this module's own types, everything else as the
//! `SessionNotification` it always was.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::{Rc, Weak};

use acp_thread::{AcpThread, ExternalSubagentReport, ExternalSubagentStatus};
use agent_client_protocol::{JsonRpcMessage, JsonRpcNotification, JsonRpcRequest, UntypedMessage};
use collections::HashMap;
use gpui::{AsyncApp, Entity, EntityId};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use util::ResultExt as _;

use super::{AcpSession, ClientContext, acp, handle_session_notification};
use crate::CLAUDE_AGENT_ID;
use project::AgentId;

const SESSION_UPDATE_METHOD: &str = "session/update";
const INITIALIZE_METHOD: &str = "initialize";
const CLIENT_CAPABILITIES_KEY: &str = "clientCapabilities";
const SUBAGENTS_CAPABILITY_KEY: &str = "subagents";

/// Updates held for a child that has not been announced yet. The adapter
/// announces a child before anything of it, so these stay small; the limits
/// only keep a misbehaving agent from growing them without end.
pub(super) const MAX_HELD_SESSIONS: usize = 16;
pub(super) const MAX_HELD_UPDATES_PER_SESSION: usize = 256;
/// The sessions remembered as having had a thread here. Only a closed one's
/// late updates are told apart by it, so the oldest can be forgotten.
pub(super) const MAX_SEEN_SESSIONS: usize = 1024;

/// Whether this agent is told that the client shows subagents. Only Claude for
/// now: what other agents do with the declaration is not known.
pub(super) fn declares_subagents(agent_id: &AgentId) -> bool {
    agent_id.as_ref() == CLAUDE_AGENT_ID
}

/// Client capabilities the schema crate's `ClientCapabilities` cannot carry.
pub(super) fn extra_client_capabilities_for_agent(agent_id: &AgentId) -> Map<String, Value> {
    let mut extra = Map::new();
    if declares_subagents(agent_id) {
        extra.insert(
            SUBAGENTS_CAPABILITY_KEY.to_string(),
            Value::Object(Map::new()),
        );
    }
    extra
}

/// `initialize`, with client capabilities added next to the ones of the
/// schema's type. One the type already has is left as the type wrote it.
#[derive(Clone, Debug)]
pub(super) struct InitializeWithExtraCapabilities {
    pub request: acp::InitializeRequest,
    pub extra_capabilities: Map<String, Value>,
}

impl InitializeWithExtraCapabilities {
    pub(super) fn to_value(&self) -> Result<Value, acp::Error> {
        let mut value =
            serde_json::to_value(&self.request).map_err(acp::Error::into_internal_error)?;
        if self.extra_capabilities.is_empty() {
            return Ok(value);
        }
        let Some(params) = value.as_object_mut() else {
            return Err(acp::Error::internal_error().data("initialize is not an object"));
        };
        let capabilities = params
            .entry(CLIENT_CAPABILITIES_KEY)
            .or_insert_with(|| Value::Object(Map::new()));
        let Some(capabilities) = capabilities.as_object_mut() else {
            return Err(acp::Error::internal_error().data("clientCapabilities is not an object"));
        };
        for (key, extra) in &self.extra_capabilities {
            capabilities
                .entry(key.clone())
                .or_insert_with(|| extra.clone());
        }
        Ok(value)
    }
}

impl JsonRpcMessage for InitializeWithExtraCapabilities {
    fn matches_method(method: &str) -> bool {
        method == INITIALIZE_METHOD
    }

    fn method(&self) -> &str {
        INITIALIZE_METHOD
    }

    fn to_untyped_message(&self) -> Result<UntypedMessage, acp::Error> {
        UntypedMessage::new(INITIALIZE_METHOD, self.to_value()?)
    }

    fn parse_message(method: &str, params: &impl Serialize) -> Result<Self, acp::Error> {
        if method != INITIALIZE_METHOD {
            return Err(acp::Error::method_not_found());
        }
        Ok(Self {
            request: agent_client_protocol::util::json_cast_params(params)?,
            extra_capabilities: Map::new(),
        })
    }
}

impl JsonRpcRequest for InitializeWithExtraCapabilities {
    type Response = acp::InitializeResponse;
}

/// A `session/update`: one of the RFD's two kinds, or any other.
#[derive(Clone, Debug)]
pub(super) enum IncomingSessionUpdate {
    Session(acp::SessionNotification),
    Subagent(SubagentNotification),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SubagentNotification {
    pub session_id: acp::SessionId,
    pub update: SubagentSessionUpdate,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "sessionUpdate", rename_all = "snake_case")]
pub(super) enum SubagentSessionUpdate {
    /// On the parent's session: a child, announced or with a new work state.
    SubagentUpdate(SubagentUpdate),
    /// On the child's session: a message it was sent.
    SessionMessage(SessionMessage),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SubagentUpdate {
    pub session_id: acp::SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub state: SubagentWorkState,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SubagentWorkState {
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<SubagentError>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct SubagentError {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<i64>,
    #[serde(default)]
    pub message: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SessionMessage {
    pub message_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_session_id: Option<acp::SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipient_session_id: Option<acp::SessionId>,
    #[serde(default)]
    pub content: Vec<acp::ContentBlock>,
}

impl SubagentWorkState {
    fn status(&self) -> ExternalSubagentStatus {
        match self.state.as_str() {
            "running" => ExternalSubagentStatus::Running,
            "requires_action" => ExternalSubagentStatus::AwaitingUser,
            "idle" => match self.stop_reason.as_deref() {
                Some("cancelled") => ExternalSubagentStatus::Cancelled,
                Some("error") => ExternalSubagentStatus::Failed,
                _ if self.error.is_some() => ExternalSubagentStatus::Failed,
                _ => ExternalSubagentStatus::Idle,
            },
            _ => ExternalSubagentStatus::Unknown,
        }
    }
}

impl SubagentUpdate {
    fn into_report(self) -> ExternalSubagentReport {
        let status = self.state.status();
        let nonblank = |text: Option<String>| text.filter(|text| !text.trim().is_empty());
        ExternalSubagentReport {
            session_id: self.session_id,
            title: nonblank(self.title),
            description: nonblank(self.description),
            status,
            error: nonblank(self.state.error.map(|error| error.message)),
        }
    }
}

fn is_subagent_update_kind(params: &Value) -> bool {
    matches!(
        params
            .get("update")
            .and_then(|update| update.get("sessionUpdate"))
            .and_then(Value::as_str),
        Some("subagent_update" | "session_message")
    )
}

impl JsonRpcMessage for IncomingSessionUpdate {
    fn matches_method(method: &str) -> bool {
        method == SESSION_UPDATE_METHOD
    }

    fn method(&self) -> &str {
        SESSION_UPDATE_METHOD
    }

    fn to_untyped_message(&self) -> Result<UntypedMessage, acp::Error> {
        match self {
            Self::Session(notification) => UntypedMessage::new(SESSION_UPDATE_METHOD, notification),
            Self::Subagent(notification) => {
                UntypedMessage::new(SESSION_UPDATE_METHOD, notification)
            }
        }
    }

    fn parse_message(method: &str, params: &impl Serialize) -> Result<Self, acp::Error> {
        if method != SESSION_UPDATE_METHOD {
            return Err(acp::Error::method_not_found());
        }
        let params = serde_json::to_value(params).map_err(acp::Error::into_internal_error)?;
        if is_subagent_update_kind(&params) {
            agent_client_protocol::util::json_cast_params(params).map(Self::Subagent)
        } else {
            agent_client_protocol::util::json_cast_params(params).map(Self::Session)
        }
    }
}

impl JsonRpcNotification for IncomingSessionUpdate {}

pub(super) enum HeldUpdate {
    Session(acp::SessionNotification),
    Message(acp::SessionId, SessionMessage),
    /// A grandchild reported on a child that is not announced yet.
    Subagent(acp::SessionId, SubagentUpdate),
}

/// The routing of the children's sessions of one connection.
#[derive(Default)]
pub(super) struct SubagentRouting {
    /// Whether the agent was told that the client shows subagents. Only then
    /// are its children read, and updates of an unknown session held for a
    /// child that may be announced next: an agent that was not told has no
    /// children, and its unknown sessions are only ever late updates of
    /// closed ones.
    declared: bool,
    held: RefCell<Vec<(acp::SessionId, Vec<HeldUpdate>)>>,
    /// Sessions that had a thread here. One of them without a thread now was
    /// closed, or its thread went away with its parent's, so what still comes
    /// for it is late, not early: it is dropped rather than held. Unless its
    /// parent (or an ancestor's) was reloaded since: the reloaded thread is
    /// sent the child's history again, and the child is announced to it anew.
    seen: RefCell<SeenSessions>,
}

#[derive(Default)]
struct SeenSessions {
    /// A child's parent as it was when the child's thread was made.
    parents: HashMap<acp::SessionId, Option<SeenParent>>,
    oldest_first: VecDeque<acp::SessionId>,
}

struct SeenParent {
    session_id: acp::SessionId,
    thread_id: EntityId,
}

impl SeenSessions {
    fn remember(&mut self, session_id: &acp::SessionId, parent: Option<SeenParent>) {
        match self.parents.get_mut(session_id) {
            Some(recorded) => {
                if parent.is_some() {
                    *recorded = parent;
                }
                return;
            }
            None => {
                self.parents.insert(session_id.clone(), parent);
            }
        }
        self.oldest_first.push_back(session_id.clone());
        if self.oldest_first.len() > MAX_SEEN_SESSIONS
            && let Some(oldest) = self.oldest_first.pop_front()
        {
            self.parents.remove(&oldest);
        }
    }

    /// Whether what comes for `session_id`, which has no thread now, is a
    /// late update of a closed session rather than an early one of a child
    /// that is to be announced to a reloaded parent.
    fn is_late(
        &self,
        session_id: &acp::SessionId,
        live_thread_id: impl Fn(&acp::SessionId) -> Option<EntityId>,
    ) -> bool {
        let Some(mut parent) = self.parents.get(session_id) else {
            return false;
        };
        // The ancestors form no cycle as the agent reports them, but the walk
        // is bounded all the same.
        for _ in 0..self.parents.len() {
            let Some(SeenParent {
                session_id,
                thread_id,
            }) = parent
            else {
                return true;
            };
            if live_thread_id(session_id).is_some_and(|live| live != *thread_id) {
                return false;
            }
            let Some(grandparent) = self.parents.get(session_id) else {
                return true;
            };
            parent = grandparent;
        }
        true
    }
}

impl SubagentRouting {
    pub(super) fn for_agent(agent_id: &AgentId) -> Self {
        Self {
            declared: declares_subagents(agent_id),
            ..Self::default()
        }
    }

    #[cfg(test)]
    pub(super) fn holding() -> Self {
        Self {
            declared: true,
            ..Self::default()
        }
    }

    /// Remembers that `session_id` has a thread here.
    pub(super) fn note_live(&self, session_id: &acp::SessionId) {
        self.seen.borrow_mut().remember(session_id, None);
    }

    /// Remembers that `session_id` has a thread here, made by the thread
    /// `parent_thread_id` of the session `parent_session_id`.
    pub(super) fn note_child(
        &self,
        session_id: &acp::SessionId,
        parent_session_id: &acp::SessionId,
        parent_thread_id: EntityId,
    ) {
        self.seen.borrow_mut().remember(
            session_id,
            Some(SeenParent {
                session_id: parent_session_id.clone(),
                thread_id: parent_thread_id,
            }),
        );
    }

    /// Holds an update of a session without a thread, for the child it may
    /// be announced as next. Returns `false` for one that is not held: from
    /// an agent that has no children, or for a session that already had its
    /// thread and lost it. `live_thread_id` names the thread a session has
    /// now, if any.
    pub(super) fn hold(
        &self,
        session_id: &acp::SessionId,
        update: HeldUpdate,
        live_thread_id: impl Fn(&acp::SessionId) -> Option<EntityId>,
    ) -> bool {
        if !self.declared {
            return false;
        }
        if self.seen.borrow().is_late(session_id, live_thread_id) {
            log::debug!("acp: did not hold a late update of the closed session {session_id}");
            return false;
        }
        let mut held = self.held.borrow_mut();
        let index = match held.iter().position(|(id, _)| id == session_id) {
            Some(index) => index,
            None => {
                if held.len() >= MAX_HELD_SESSIONS {
                    let (dropped, _) = held.remove(0);
                    log::warn!(
                        "acp: dropped the held updates of session {dropped}, which no subagent announcement claimed"
                    );
                }
                held.push((session_id.clone(), Vec::new()));
                held.len() - 1
            }
        };
        let Some((_, updates)) = held.get_mut(index) else {
            return false;
        };
        if updates.len() >= MAX_HELD_UPDATES_PER_SESSION {
            log::warn!("acp: dropped an update of the unannounced session {session_id}");
            return true;
        }
        updates.push(update);
        true
    }

    fn take(&self, session_id: &acp::SessionId) -> Vec<HeldUpdate> {
        let mut held = self.held.borrow_mut();
        match held.iter().position(|(id, _)| id == session_id) {
            Some(index) => held.remove(index).1,
            None => Vec::new(),
        }
    }

    #[cfg(test)]
    pub(super) fn held_count(&self, session_id: &acp::SessionId) -> usize {
        self.held
            .borrow()
            .iter()
            .find(|(id, _)| id == session_id)
            .map_or(0, |(_, updates)| updates.len())
    }
}

fn session_is_known(ctx: &ClientContext, session_id: &acp::SessionId) -> bool {
    ctx.sessions.borrow().contains_key(session_id)
}

fn live_thread(ctx: &ClientContext, session_id: &acp::SessionId) -> Option<Entity<AcpThread>> {
    ctx.sessions
        .borrow()
        .get(session_id)
        .and_then(|session| session.thread.upgrade())
}

fn live_thread_id(ctx: &ClientContext, session_id: &acp::SessionId) -> Option<EntityId> {
    live_thread(ctx, session_id).map(|thread| thread.entity_id())
}

pub(super) fn handle_incoming_session_update(
    notification: IncomingSessionUpdate,
    cx: &mut AsyncApp,
    ctx: &ClientContext,
) {
    match notification {
        IncomingSessionUpdate::Session(notification) => {
            route_session_notification(notification, cx, ctx)
        }
        // An agent that was not told about subagents gets no children from
        // them: it is read as before the RFD, when these did not parse.
        IncomingSessionUpdate::Subagent(notification) if !ctx.subagents.declared => {
            log::debug!(
                "acp: dropped a subagent update of session {} from an agent that was not told about subagents",
                notification.session_id
            );
        }
        IncomingSessionUpdate::Subagent(SubagentNotification {
            session_id,
            update: SubagentSessionUpdate::SubagentUpdate(update),
        }) => handle_subagent_update(session_id, update, cx, ctx),
        IncomingSessionUpdate::Subagent(SubagentNotification {
            session_id,
            update: SubagentSessionUpdate::SessionMessage(message),
        }) => handle_session_message(session_id, message, cx, ctx),
    }
}

fn route_session_notification(
    notification: acp::SessionNotification,
    cx: &mut AsyncApp,
    ctx: &ClientContext,
) {
    let session_id = notification.session_id.clone();
    if !session_is_known(ctx, &session_id)
        && ctx.subagents.hold(
            &session_id,
            HeldUpdate::Session(notification.clone()),
            |id| live_thread_id(ctx, id),
        )
    {
        log::debug!("acp: held an update of session {session_id} until it is announced");
        return;
    }
    let tool_call_id = match &notification.update {
        acp::SessionUpdate::ToolCall(tool_call) => Some(tool_call.tool_call_id.clone()),
        acp::SessionUpdate::ToolCallUpdate(update) => Some(update.tool_call_id.clone()),
        _ => None,
    };
    handle_session_notification(notification, cx, ctx);
    let Some(thread) = live_thread(ctx, &session_id) else {
        return;
    };
    ctx.subagents.note_live(&session_id);
    // A child's first message can come before the call that launched it.
    if let Some(tool_call_id) = tool_call_id {
        thread.update(cx, |thread, cx| {
            thread.show_external_subagent_on_card(&tool_call_id, cx);
        });
    }
}

fn handle_subagent_update(
    parent_session_id: acp::SessionId,
    update: SubagentUpdate,
    cx: &mut AsyncApp,
    ctx: &ClientContext,
) {
    let Some(parent) = live_thread(ctx, &parent_session_id) else {
        if !ctx.subagents.hold(
            &parent_session_id,
            HeldUpdate::Subagent(parent_session_id.clone(), update),
            |id| live_thread_id(ctx, id),
        ) {
            log::warn!(
                "acp: dropped a subagent of the session {parent_session_id}, which has no thread"
            );
        }
        return;
    };
    ctx.subagents.note_live(&parent_session_id);
    let report = update.into_report();
    let child_session_id = report.session_id.clone();
    let new_child = parent.update(cx, |parent, cx| {
        parent.apply_external_subagent_report(report, cx)
    });
    let Some(child) = new_child else {
        return;
    };
    register_child_session(
        ctx,
        &parent_session_id,
        parent.entity_id(),
        child_session_id.clone(),
        &child,
        cx,
    );
    for held in ctx.subagents.take(&child_session_id) {
        match held {
            HeldUpdate::Session(notification) => route_session_notification(notification, cx, ctx),
            HeldUpdate::Message(session_id, message) => {
                handle_session_message(session_id, message, cx, ctx)
            }
            HeldUpdate::Subagent(session_id, update) => {
                handle_subagent_update(session_id, update, cx, ctx)
            }
        }
    }
}

/// The child's session goes to its thread like any other session of the
/// connection, and goes away with the thread. It is not closed on the agent:
/// the client was given no say over a child.
///
/// A reloaded parent makes a new thread for a child it had before, and the
/// child's session moves to it from the thread the parent's previous thread
/// made, which may not have gone away yet (or has, without its session being
/// let go of yet). A session open as a thread of its own stays where it is.
fn register_child_session(
    ctx: &ClientContext,
    parent_session_id: &acp::SessionId,
    parent_thread_id: EntityId,
    session_id: acp::SessionId,
    thread: &Entity<AcpThread>,
    cx: &mut AsyncApp,
) {
    let current_thread = live_thread(ctx, &session_id);
    if let Some(current_thread) = current_thread
        && current_thread.entity_id() != thread.entity_id()
        && current_thread.read_with(cx, |current_thread, _| {
            current_thread.parent_session_id() != Some(parent_session_id)
        })
    {
        log::warn!(
            "acp: the subagent session {session_id} is open as another thread, which keeps its updates"
        );
        return;
    }
    let sessions: Weak<RefCell<HashMap<acp::SessionId, AcpSession>>> = Rc::downgrade(&ctx.sessions);
    let release_subscription = cx.update(|cx| {
        cx.observe_release(thread, {
            let session_id = session_id.clone();
            let thread_id = thread.entity_id();
            move |_, _| {
                if let Some(sessions) = sessions.upgrade() {
                    let mut sessions = sessions.borrow_mut();
                    if sessions
                        .get(&session_id)
                        .is_some_and(|session| session.thread.entity_id() == thread_id)
                    {
                        sessions.remove(&session_id);
                    }
                }
            }
        })
    });
    let replaced = ctx.sessions.borrow_mut().insert(
        session_id.clone(),
        AcpSession {
            thread: thread.downgrade(),
            suppress_abort_err: false,
            session_modes: None,
            config_options: None,
            default_config_task: None,
            _release_subscription: release_subscription,
        },
    );
    if replaced.is_some() {
        log::debug!("acp: the subagent session {session_id} moved to its reloaded parent's thread");
    }
    ctx.subagents
        .note_child(&session_id, parent_session_id, parent_thread_id);
}

fn handle_session_message(
    session_id: acp::SessionId,
    mut message: SessionMessage,
    cx: &mut AsyncApp,
    ctx: &ClientContext,
) {
    let Some(child) = live_thread(ctx, &session_id) else {
        if !ctx.subagents.hold(
            &session_id,
            HeldUpdate::Message(session_id.clone(), message),
            |id| live_thread_id(ctx, id),
        ) {
            log::warn!("acp: dropped a message to the session {session_id}, which has no thread");
        }
        return;
    };
    // ADR-F49 decision 23: like any other text the agent shows.
    if let Some(paths) = &ctx.paths {
        message.content = paths.message_content_to_host(message.content);
    }
    let message_id = message.message_id.clone();
    let sender_session_id = message.sender_session_id.clone();
    let shown = child.update(cx, |child, cx| {
        child.receive_external_subagent_message(message_id.clone(), message.content, cx)
    });
    if shown.log_err().is_none() {
        return;
    }
    let Some(parent_session_id) =
        child.read_with(cx, |child, _| child.parent_session_id().cloned())
    else {
        return;
    };
    // Only the parent's own call launched the child; a message another
    // session sent it (SendMessage) is no launch of the parent's.
    if sender_session_id
        .as_ref()
        .is_some_and(|sender| sender != &parent_session_id)
    {
        return;
    }
    if let Some(parent) = live_thread(ctx, &parent_session_id) {
        parent.update(cx, |parent, cx| {
            parent.link_external_subagent_tool_call(
                &session_id,
                acp::ToolCallId::new(message_id),
                cx,
            );
        });
    }
}
