//! ADR-F50: approvals for programs that a remote project's settings would
//! start on the remote host — `lsp.<name>.binary` and `formatter.external`
//! from a worktree's settings or from the remote server's own settings file.
//!
//! The remote server resolves the definition and hashes what it runs on its
//! own file system ([`RemoteLaunchApprovals`], one per session), then asks the
//! IDE it serves. The IDE ([`RemoteLaunchRequests`], one per remote project)
//! binds the remote server's digest to the host it connected to and to what it
//! shows, and decides with the same approvals, notifications and database as
//! for local launches.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result};
use client::{TypedEnvelope, proto};
use collections::{HashMap, HashSet, IndexMap};
use futures::{
    StreamExt as _,
    channel::{mpsc, oneshot},
    future::{Either, select},
};
use gpui::{
    App, AppContext as _, AsyncApp, Context, Entity, EventEmitter, SharedString, Subscription,
    Task, TaskExt as _, WeakEntity,
};
use language::LanguageRegistry;
use lsp::LanguageServerName;
use remote::{ConnectionState, RemoteClient};
use rpc::AnyProtoClient;
use sha2::{Digest as _, Sha256};
use util::ResultExt as _;
use worktree::WorktreeId;

use crate::{
    Event, Project,
    project_launch_approvals::{
        ExecutionHost, LaunchApprovalDetails, LaunchApprovalKey, LaunchApprovalState,
        LaunchDecision, LaunchDefinitionFile, LaunchHost, LaunchKind, LaunchRequester, LaunchSlot,
        LaunchTarget, PendingLaunchGuard, ProjectLaunchApprovals, ProjectLaunchApprovalsEvent,
        UnboundWorktreeContent, canonical_json, escape_for_display,
    },
    worktree_store::WorktreeStore,
};

use proto::request_project_launch_approval::{DefinitionFile, ResolvedProgram, ServerSettings};

/// How long a language server's question waits for the IDE. The IDE answers
/// at once with what it knows; one from before ADR-F50 cannot read the
/// question and never answers, which only this ends.
pub(crate) const LANGUAGE_SERVER_ANSWER_TIMEOUT: Duration = Duration::from_secs(30);

/// A save waits for its formatter's answer, so it gives up sooner.
pub(crate) const FORMATTER_ANSWER_TIMEOUT: Duration = Duration::from_secs(5);

/// How often the IDE is asked again when a revocation it pushed overtook its
/// answer.
const MAX_ATTEMPTS: usize = 3;

const MAX_ARGUMENTS: usize = 1024;
const MAX_LIST_ENTRIES: usize = 1024;
const MAX_STRING_BYTES: usize = 32 * 1024;
const MAX_TOTAL_BYTES: usize = 256 * 1024;

/// Launches of one remote project that may wait for the user at once, each
/// counted once across edits of its definition ([`LaunchSlot`]): each one is
/// a notification, and a remote server could otherwise ask without end.
const MAX_PENDING_LAUNCHES: usize = 32;

/// How long a launch the remote server stopped waiting for still counts
/// against [`MAX_PENDING_LAUNCHES`], unless it asks about that launch again
/// (a restart), so that asking about ever new launches and withdrawing them
/// raises no more notifications than waiting would. Its notification is
/// remembered as long, so that asking about it again brings nothing up again.
const WITHDRAWN_COOLDOWN: Duration = Duration::from_secs(10 * 60);

/// How long a launch the remote server stopped waiting for stays in front of
/// the user: a restart withdraws a launch and asks about it again at once,
/// which should not take away what the user is looking at.
const WITHDRAWAL_GRACE: Duration = Duration::from_secs(5);

/// How often a language server the IDE had no room for asks again, besides
/// whenever the IDE decides something: the IDE's limit also counts launches
/// that stopped waiting a while ago, and nothing says when those run out.
const ROOM_RETRY_INTERVAL: Duration = Duration::from_secs(60);

/// Keys kept to pass later decisions on to the remote server. Approved ones
/// are never dropped, since a later revocation has to reach it.
const MAX_REMEMBERED_LAUNCHES: usize = 256;

const REVOCATION_WAITS_NOTIFICATION: &str = "remote-launch-approval-revocation";
fn revocation_waits_message() -> &'static str {
    i18n::t!("The remote host is disconnected, so the revocation reaches it when it reconnects")
}

/// Where in the project a launch belongs, which the remote server tells the
/// IDE along with the details.
#[derive(Clone, Debug)]
pub(crate) struct LaunchPlace {
    pub(crate) worktree_id: WorktreeId,
    pub(crate) working_directory: Option<PathBuf>,
}

/// The key the IDE decides under: the remote server's digest bound to the
/// host this IDE connected to, to the machine the remote server says it runs
/// on (which can only tell hosts further apart), and to everything the
/// approval UI shows, so that the same digest sent with other content needs
/// an approval of its own.
pub(crate) fn remote_launch_key(
    host: &LaunchHost,
    host_machine: Option<&str>,
    remote_key: &LaunchApprovalKey,
    shown: &serde_json::Value,
) -> LaunchApprovalKey {
    let key_material = serde_json::json!({
        "host": host.identity(),
        "host_machine": host_machine,
        "remote_key": remote_key.as_str(),
        "shown": shown,
    });
    let digest = Sha256::digest(canonical_json(&key_material).as_bytes());
    LaunchApprovalKey(format!("{digest:x}").into())
}

/// Everything the approval UI shows about `details`.
fn shown_content(
    details: &LaunchApprovalDetails,
    working_directory: Option<&str>,
) -> serde_json::Value {
    let (program, arguments) = match &details.target {
        LaunchTarget::Command { program, arguments } => (
            program.as_ref(),
            arguments.iter().map(AsRef::as_ref).collect::<Vec<&str>>(),
        ),
        LaunchTarget::Url(url) => (url.as_ref(), Vec::new()),
    };
    let definition_file = match &details.definition_file {
        LaunchDefinitionFile::Worktree(path) => serde_json::json!({ "worktree": path.as_ref() }),
        LaunchDefinitionFile::RemoteServerSettings => serde_json::json!("server_settings"),
        LaunchDefinitionFile::ResolvedOnRemoteHost => {
            serde_json::json!("resolved_on_remote_host")
        }
    };
    serde_json::json!({
        "kind": details.kind,
        "name": details.name.as_ref(),
        "program": program,
        "arguments": arguments,
        "environment_variable_names": details
            .environment_variable_names
            .iter()
            .map(AsRef::as_ref)
            .collect::<Vec<&str>>(),
        "code_loading_environment": details
            .code_loading_environment
            .iter()
            .map(|(name, value)| [name.as_ref(), value.as_ref()])
            .collect::<Vec<[&str; 2]>>(),
        "definition_file": definition_file,
        "worktree": details.worktree_abs_path.to_string_lossy(),
        "working_directory": working_directory,
        "referenced_worktree_files": details
            .referenced_worktree_files
            .iter()
            .map(AsRef::as_ref)
            .collect::<Vec<&str>>(),
        "unbound_worktree_content": details
            .unbound_worktree_content
            .iter()
            .map(|content| {
                let content = unbound_to_proto(content);
                serde_json::json!([content.kind, content.path, content.program])
            })
            .collect::<Vec<_>>(),
        "approval_is_session_only": details.approval_is_session_only,
        "replaces_user_entry": details.replaces_user_entry,
    })
}

fn parse_remote_key(key: &str) -> Result<LaunchApprovalKey> {
    anyhow::ensure!(
        key.len() == 64
            && key
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')),
        i18n::t!("The launch approval key from the remote host is malformed")
    );
    Ok(LaunchApprovalKey(key.to_string().into()))
}

fn request_strings(request: &proto::RequestProjectLaunchApproval) -> impl Iterator<Item = &str> {
    let definition_file = match &request.definition_file {
        Some(DefinitionFile::WorktreeSettingsFile(path)) => Some(path.as_str()),
        Some(DefinitionFile::ServerSettings(_) | DefinitionFile::ResolvedProgram(_)) | None => None,
    };
    [
        request.remote_key.as_str(),
        request.name.as_str(),
        request.program.as_str(),
        request.worktree_abs_path.as_str(),
    ]
    .into_iter()
    .chain(request.working_directory.as_deref())
    .chain(request.host_machine.as_deref())
    .chain(definition_file)
    .chain(request.arguments.iter().map(String::as_str))
    .chain(
        request
            .environment_variable_names
            .iter()
            .map(String::as_str),
    )
    .chain(
        request
            .code_loading_environment
            .iter()
            .flat_map(|variable| [variable.name.as_str(), variable.value.as_str()]),
    )
    .chain(request.referenced_worktree_files.iter().map(String::as_str))
    .chain(request.unbound_worktree_content.iter().flat_map(|content| {
        std::iter::once(content.path.as_str()).chain(content.program.as_deref())
    }))
}

/// The same limits on both ends: the remote server does not send what the
/// IDE would refuse to show.
pub(crate) fn check_request_limits(request: &proto::RequestProjectLaunchApproval) -> Result<()> {
    anyhow::ensure!(
        request.arguments.len() <= MAX_ARGUMENTS,
        i18n::t!(
            "The launch definition has too many arguments ({count}, at most {MAX_ARGUMENTS})",
            count = request.arguments.len()
        )
    );
    let list_lengths = [
        request.environment_variable_names.len(),
        request.code_loading_environment.len(),
        request.referenced_worktree_files.len(),
        request.unbound_worktree_content.len(),
    ];
    anyhow::ensure!(
        list_lengths
            .iter()
            .all(|length| *length <= MAX_LIST_ENTRIES),
        i18n::t!("The launch definition has too many entries (at most {MAX_LIST_ENTRIES})")
    );
    let mut total_bytes = 0usize;
    for text in request_strings(request) {
        anyhow::ensure!(
            text.len() <= MAX_STRING_BYTES,
            i18n::t!("The launch definition has a string longer than {MAX_STRING_BYTES} bytes")
        );
        total_bytes = total_bytes.saturating_add(text.len());
    }
    anyhow::ensure!(
        total_bytes <= MAX_TOTAL_BYTES,
        i18n::t!(
            "The launch definition is too large ({total_bytes} bytes, at most {MAX_TOTAL_BYTES} bytes)"
        )
    );
    Ok(())
}

/// What the remote server sends the IDE about `details`, which it resolved
/// on its own file system.
pub(crate) fn request_to_proto(
    details: &LaunchApprovalDetails,
    project_id: u64,
    place: &LaunchPlace,
    host_machine: Option<&str>,
) -> Result<proto::RequestProjectLaunchApproval> {
    let kind = match details.kind {
        LaunchKind::LanguageServerBinary => {
            proto::ProjectLaunchKind::LaunchKindLanguageServerBinary
        }
        LaunchKind::ExternalFormatter => proto::ProjectLaunchKind::LaunchKindExternalFormatter,
        LaunchKind::ContextServer => {
            anyhow::bail!(i18n::t!(
                "The remote host does not ask about launching MCP servers"
            ))
        }
    };
    let LaunchTarget::Command { program, arguments } = &details.target else {
        anyhow::bail!(i18n::t!(
            "The remote host does not ask about connecting to URLs"
        ));
    };
    let definition_file = match &details.definition_file {
        LaunchDefinitionFile::Worktree(path) => {
            DefinitionFile::WorktreeSettingsFile(path.to_string())
        }
        LaunchDefinitionFile::RemoteServerSettings => {
            DefinitionFile::ServerSettings(ServerSettings {})
        }
        LaunchDefinitionFile::ResolvedOnRemoteHost => {
            DefinitionFile::ResolvedProgram(ResolvedProgram {})
        }
    };
    let request = proto::RequestProjectLaunchApproval {
        project_id,
        remote_key: details.key.to_string(),
        kind: kind as i32,
        name: details.name.to_string(),
        program: program.to_string(),
        arguments: arguments.iter().map(ToString::to_string).collect(),
        environment_variable_names: details
            .environment_variable_names
            .iter()
            .map(ToString::to_string)
            .collect(),
        code_loading_environment: details
            .code_loading_environment
            .iter()
            .map(|(name, value)| proto::ProjectLaunchEnvironmentVariable {
                name: name.to_string(),
                value: value.to_string(),
            })
            .collect(),
        definition_file: Some(definition_file),
        worktree_id: place.worktree_id.to_proto(),
        worktree_abs_path: details.worktree_abs_path.to_string_lossy().into_owned(),
        working_directory: place
            .working_directory
            .as_ref()
            .map(|directory| directory.to_string_lossy().into_owned()),
        referenced_worktree_files: details
            .referenced_worktree_files
            .iter()
            .map(ToString::to_string)
            .collect(),
        unbound_worktree_content: details
            .unbound_worktree_content
            .iter()
            .map(unbound_to_proto)
            .collect(),
        approval_is_session_only: details.approval_is_session_only,
        replaces_user_entry: details.replaces_user_entry,
        host_machine: host_machine.map(str::to_string),
    };
    check_request_limits(&request)?;
    Ok(request)
}

fn unbound_to_proto(content: &UnboundWorktreeContent) -> proto::ProjectLaunchUnboundContent {
    use proto::ProjectLaunchUnboundKind as Kind;
    let (kind, path, program) = match content {
        UnboundWorktreeContent::WorkingDirectory {
            program,
            working_directory,
        } => (
            Kind::UnboundWorkingDirectory,
            working_directory.to_string(),
            Some(program.to_string()),
        ),
        UnboundWorktreeContent::File(path) => (Kind::UnboundFile, path.to_string(), None),
        UnboundWorktreeContent::Directory(path) => (Kind::UnboundDirectory, path.to_string(), None),
        UnboundWorktreeContent::ArgumentFile(path) => {
            (Kind::UnboundArgumentFile, path.to_string(), None)
        }
        UnboundWorktreeContent::Wrapper(program) => (
            Kind::UnboundWrapper,
            String::new(),
            Some(program.to_string()),
        ),
        UnboundWorktreeContent::RemoteFile(path) => {
            (Kind::UnboundRemoteFile, path.to_string(), None)
        }
        UnboundWorktreeContent::Unreadable(path) => {
            (Kind::UnboundUnreadable, path.to_string(), None)
        }
    };
    proto::ProjectLaunchUnboundContent {
        kind: kind as i32,
        path,
        program,
    }
}

/// A warning the IDE cannot show is a launch it cannot decide about.
fn unbound_from_proto(
    content: proto::ProjectLaunchUnboundContent,
) -> Result<UnboundWorktreeContent> {
    use proto::ProjectLaunchUnboundKind as Kind;
    let path = SharedString::from(content.path);
    let program = content.program.map(SharedString::from);
    let missing_program = i18n::t!("A warning from the remote host has no program name");
    Ok(match Kind::try_from(content.kind).ok() {
        Some(Kind::UnboundWorkingDirectory) => UnboundWorktreeContent::WorkingDirectory {
            program: program.context(missing_program)?,
            working_directory: path,
        },
        Some(Kind::UnboundFile) => UnboundWorktreeContent::File(path),
        Some(Kind::UnboundDirectory) => UnboundWorktreeContent::Directory(path),
        Some(Kind::UnboundArgumentFile) => UnboundWorktreeContent::ArgumentFile(path),
        Some(Kind::UnboundWrapper) => {
            UnboundWorktreeContent::Wrapper(program.context(missing_program)?)
        }
        Some(Kind::UnboundRemoteFile) => UnboundWorktreeContent::RemoteFile(path),
        Some(Kind::UnboundUnreadable) => UnboundWorktreeContent::Unreadable(path),
        Some(Kind::UnboundUnknown) | None => {
            anyhow::bail!(i18n::t!(
                "The remote host sent a kind of warning this IDE cannot show"
            ))
        }
    })
}

/// The details the IDE shows and decides about, for a request from the
/// remote server connected as `host`. `worktree_abs_path` is the IDE's own
/// view of the worktree, checked with [`check_launch_place`].
pub(crate) fn details_from_proto(
    request: proto::RequestProjectLaunchApproval,
    host: LaunchHost,
    worktree_abs_path: Arc<Path>,
) -> Result<LaunchApprovalDetails> {
    check_request_limits(&request)?;
    let remote_key = parse_remote_key(&request.remote_key)?;
    let kind = match proto::ProjectLaunchKind::try_from(request.kind).ok() {
        Some(proto::ProjectLaunchKind::LaunchKindLanguageServerBinary) => {
            LaunchKind::LanguageServerBinary
        }
        Some(proto::ProjectLaunchKind::LaunchKindExternalFormatter) => {
            LaunchKind::ExternalFormatter
        }
        Some(proto::ProjectLaunchKind::LaunchKindUnknown) | None => {
            anyhow::bail!(i18n::t!(
                "The remote host asked about a kind of launch this IDE does not know"
            ))
        }
    };
    anyhow::ensure!(
        !request.name.is_empty() && !request.program.is_empty(),
        i18n::t!("A launch request from the remote host has no name or program")
    );
    let definition_file = match request.definition_file.context(i18n::t!(
        "A launch request from the remote host does not say where it is defined"
    ))? {
        DefinitionFile::WorktreeSettingsFile(path) => LaunchDefinitionFile::Worktree(path.into()),
        DefinitionFile::ServerSettings(_) => LaunchDefinitionFile::RemoteServerSettings,
        DefinitionFile::ResolvedProgram(_) => LaunchDefinitionFile::ResolvedOnRemoteHost,
    };
    let unbound_worktree_content = request
        .unbound_worktree_content
        .into_iter()
        .map(unbound_from_proto)
        .collect::<Result<Vec<_>>>()?;
    let working_directory = request.working_directory;
    let host_machine = request.host_machine;
    let mut details = LaunchApprovalDetails {
        key: remote_key.clone(),
        kind,
        name: request.name.into(),
        target: LaunchTarget::Command {
            program: request.program.into(),
            arguments: request
                .arguments
                .into_iter()
                .map(SharedString::from)
                .collect(),
        },
        environment_variable_names: request
            .environment_variable_names
            .into_iter()
            .map(SharedString::from)
            .collect(),
        header_names: Vec::new(),
        code_loading_environment: request
            .code_loading_environment
            .into_iter()
            .map(|variable| (variable.name.into(), variable.value.into()))
            .collect(),
        execution_host: ExecutionHost::of(&host),
        host,
        definition_file,
        worktree_abs_path,
        referenced_worktree_files: request
            .referenced_worktree_files
            .into_iter()
            .map(SharedString::from)
            .collect(),
        unbound_worktree_content,
        approval_is_session_only: request.approval_is_session_only,
        replaces_user_entry: request.replaces_user_entry,
        state: LaunchApprovalState::Pending,
        store_error: None,
    };
    let shown = shown_content(&details, working_directory.as_deref());
    details.key = remote_launch_key(&details.host, host_machine.as_deref(), &remote_key, &shown);
    Ok(details)
}

/// The worktree the remote server names has to be the one this IDE knows
/// under that id, and the program has to start where the remote server
/// starts such programs: in the worktree, or next to a worktree that is a
/// single file.
pub(crate) fn check_launch_place(
    request: &proto::RequestProjectLaunchApproval,
    worktree_abs_path: &Path,
    worktree_is_a_file: bool,
) -> Result<()> {
    let known = worktree_abs_path.to_string_lossy();
    anyhow::ensure!(
        request.worktree_abs_path == known,
        i18n::t!(
            "The worktree the remote host asked about ({requested}) does not match the worktree this IDE has open under the same ID ({known})",
            requested = request.worktree_abs_path
        )
    );
    let Some(working_directory) = request.working_directory.as_deref() else {
        return Ok(());
    };
    let parent = worktree_is_a_file
        .then(|| {
            known
                .rfind(['/', '\\'])
                .and_then(|separator| known.get(..separator))
        })
        .flatten();
    let without_trailing_separator = |path: &str| path.trim_end_matches(['/', '\\']).to_string();
    let starts_in_place = without_trailing_separator(working_directory)
        == without_trailing_separator(&known)
        || parent.is_some_and(|parent| {
            without_trailing_separator(working_directory) == without_trailing_separator(parent)
        });
    anyhow::ensure!(
        starts_in_place,
        i18n::t!(
            "The working directory of the launch the remote host asked about ({working_directory}) is not where the worktree ({known}) is"
        )
    );
    Ok(())
}

/// The IDE's answer, as far as the remote server is concerned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RemoteAnswer {
    Approved,
    Denied,
    Pending,
    /// The IDE answered that nobody can decide.
    Unavailable(SharedString),
    /// The IDE has no room now for another launch to wait for the user.
    Deferred(SharedString),
    /// The IDE did not answer. `answers_seen` counts what it had sent by
    /// then, so that waiting for its next answer misses none.
    Unanswered {
        reason: SharedString,
        answers_seen: u64,
    },
}

impl RemoteAnswer {
    fn of(decision: LaunchDecision) -> Self {
        match decision {
            LaunchDecision::Approved => Self::Approved,
            LaunchDecision::Denied => Self::Denied,
        }
    }
}

fn answer_from_response(response: proto::RequestProjectLaunchApprovalResponse) -> RemoteAnswer {
    use proto::ProjectLaunchApprovalState as State;
    match State::try_from(response.state).ok() {
        Some(State::LaunchApprovalApproved) => RemoteAnswer::Approved,
        Some(State::LaunchApprovalDenied) => RemoteAnswer::Denied,
        Some(State::LaunchApprovalPending) => {
            if let Some(reason) = response.reason {
                log::warn!("the IDE cannot record a decision about this launch yet: {reason}");
            }
            RemoteAnswer::Pending
        }
        Some(State::LaunchApprovalDeferred) => RemoteAnswer::Deferred(
            response
                .reason
                .unwrap_or_else(|| {
                    i18n::t!("Too many launches are waiting for approval in the IDE").to_string()
                })
                .into(),
        ),
        Some(State::LaunchApprovalUnavailable) | None => RemoteAnswer::Unavailable(
            response
                .reason
                .unwrap_or_else(|| i18n::t!("The IDE cannot approve launches").to_string())
                .into(),
        ),
    }
}

/// The remote server's side (ADR-F50): asks the IDE of one session about the
/// launches that session's project needs, and keeps the answers for as long
/// as the session lasts. Nobody to ask — no IDE, an IDE without the handler,
/// or one that does not answer in time — means no launch.
pub struct RemoteLaunchApprovals {
    client: AnyProtoClient,
    project_id: u64,
    /// A digest of the machine this server runs on, which the IDE binds its
    /// keys to.
    host_machine: Option<String>,
    decisions: HashMap<LaunchApprovalKey, LaunchDecision>,
    /// Counts what the IDE pushed, so that an answer that was already on its
    /// way does not overwrite a newer decision or revocation.
    pushes_received: u64,
    last_push: HashMap<LaunchApprovalKey, u64>,
    pending_requesters: HashMap<LaunchApprovalKey, HashSet<LaunchRequester>>,
    decision_waiters: HashMap<LaunchApprovalKey, Vec<oneshot::Sender<LaunchDecision>>>,
    withdrawals: mpsc::UnboundedSender<(LaunchApprovalKey, LaunchRequester)>,
    /// How many withdrawals went out for each key: a pending answer that one
    /// of them may have taken back is asked about again rather than waited on.
    withdrawals_sent: HashMap<LaunchApprovalKey, u64>,
    /// Set when the IDE let a question go unanswered, so that formatters are
    /// skipped at once instead of each holding a save up. Language servers
    /// still ask, one question at a time for each launch. Anything the IDE
    /// sends, or a new connection, clears it.
    ide_does_not_answer: bool,
    /// The launches whose question the IDE let go unanswered, with which one
    /// it was: until the IDE answers it, or a new connection comes, they are
    /// not asked about again, so that restarts do not pile questions up at an
    /// IDE that never answers.
    unanswered_questions: HashMap<LaunchSlot, u64>,
    questions_timed_out: u64,
    /// How often the IDE showed that it answers.
    answers_seen: u64,
    answer_waiters: Vec<oneshot::Sender<()>>,
    /// Launches waiting for the IDE to have room for them.
    room_waiters: Vec<oneshot::Sender<()>>,
    room_retry: Option<Task<()>>,
    _process_withdrawals: Task<()>,
}

impl EventEmitter<ProjectLaunchApprovalsEvent> for RemoteLaunchApprovals {}

impl RemoteLaunchApprovals {
    /// `host_identity` tells this machine apart from others that the IDE may
    /// reach under the same name.
    pub fn new(
        client: AnyProtoClient,
        project_id: u64,
        host_identity: &str,
        cx: &mut Context<Self>,
    ) -> Self {
        let (withdrawals, mut withdrawal_requests) =
            mpsc::unbounded::<(LaunchApprovalKey, LaunchRequester)>();
        let process_withdrawals = cx.spawn(async move |this, cx| {
            while let Some((key, requester)) = withdrawal_requests.next().await {
                if this
                    .update(cx, |this, cx| this.withdraw_pending(&key, requester, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        let host_machine = (!host_identity.trim().is_empty())
            .then(|| format!("{:x}", Sha256::digest(host_identity.as_bytes())));
        Self {
            client,
            project_id,
            host_machine,
            decisions: HashMap::default(),
            pushes_received: 0,
            last_push: HashMap::default(),
            pending_requesters: HashMap::default(),
            decision_waiters: HashMap::default(),
            withdrawals,
            withdrawals_sent: HashMap::default(),
            ide_does_not_answer: false,
            unanswered_questions: HashMap::default(),
            questions_timed_out: 0,
            answers_seen: 0,
            answer_waiters: Vec::new(),
            room_waiters: Vec::new(),
            room_retry: None,
            _process_withdrawals: process_withdrawals,
        }
    }

    /// Registers the handlers for the decisions the IDE pushes to a session.
    pub fn init(client: &AnyProtoClient) {
        client.add_entity_request_handler(Self::handle_launch_approval_decided);
        client.add_entity_request_handler(Self::handle_launch_approval_revoked);
    }

    /// The IDE came back on a new connection, where it may answer, and may
    /// have room.
    pub fn connection_restored(&mut self) {
        self.unanswered_questions.clear();
        self.may_answer_again();
        self.room_may_have_come();
    }

    pub(crate) fn decision(&self, key: &LaunchApprovalKey) -> Option<LaunchDecision> {
        self.decisions.get(key).copied()
    }

    pub(crate) fn withdrawals_sent(&self, key: &LaunchApprovalKey) -> u64 {
        self.withdrawals_sent.get(key).copied().unwrap_or(0)
    }

    /// Asks the IDE about `details` unless it already decided in this
    /// session, waiting at most `timeout` for its answer.
    pub(crate) fn request_launch(
        &mut self,
        details: &LaunchApprovalDetails,
        place: &LaunchPlace,
        timeout: Duration,
        cx: &mut Context<Self>,
    ) -> Task<RemoteAnswer> {
        if let Some(decision) = self.decision(&details.key) {
            return Task::ready(RemoteAnswer::of(decision));
        }
        let slot = LaunchSlot::of(details);
        if self.unanswered_questions.contains_key(&slot)
            || (self.ide_does_not_answer && details.kind == LaunchKind::ExternalFormatter)
        {
            return Task::ready(RemoteAnswer::Unanswered {
                reason: i18n::t!(
                    "The IDE is not answering launch requests (asked again once the IDE answers or reconnects)"
                )
                .into(),
                answers_seen: self.answers_seen,
            });
        }
        let request = match request_to_proto(
            details,
            self.project_id,
            place,
            self.host_machine.as_deref(),
        ) {
            Ok(request) => request,
            Err(error) => {
                return Task::ready(RemoteAnswer::Unavailable(format!("{error:#}").into()));
            }
        };
        let key = details.key.clone();
        let client = self.client.clone();
        cx.spawn(async move |this, cx| {
            for _ in 0..MAX_ATTEMPTS {
                let Ok(sent_after) = this.read_with(cx, |this, _| this.pushes_received) else {
                    break;
                };
                let response = cx.background_spawn(client.request(request.clone()));
                let timer = cx.background_executor().timer(timeout);
                let response = match select(response, timer).await {
                    Either::Left((response, _)) => response,
                    // An IDE from before ADR-F50 cannot read the question and
                    // never answers it.
                    Either::Right(((), response)) => {
                        let Ok(answers_seen) = this.update(cx, |this, cx| {
                            this.wait_for_a_late_answer(slot.clone(), response, cx)
                        }) else {
                            break;
                        };
                        return RemoteAnswer::Unanswered {
                            reason: i18n::t!(
                                "The IDE did not answer within {seconds} seconds",
                                seconds = timeout.as_secs()
                            )
                            .into(),
                            answers_seen,
                        };
                    }
                };
                let answer = match response {
                    Ok(response) => answer_from_response(response),
                    // An IDE that can read the question but has no handler
                    // for it answers with an error.
                    Err(error) => RemoteAnswer::Unavailable(format!("{error:#}").into()),
                };
                match this.update(cx, |this, _| {
                    this.may_answer_again();
                    this.settle_answer(&key, sent_after, answer)
                }) {
                    Ok(Some(answer)) => return answer,
                    Ok(None) => continue,
                    Err(_) => break,
                }
            }
            RemoteAnswer::Unavailable(
                i18n::t!("Could not confirm the launch approval with the IDE").into(),
            )
        })
    }

    /// Skips formatters without asking until the IDE sends anything, and the
    /// launch in `slot` until it answers this very question. Returns how
    /// many answers had come so far.
    fn wait_for_a_late_answer(
        &mut self,
        slot: LaunchSlot,
        response: Task<Result<proto::RequestProjectLaunchApprovalResponse>>,
        cx: &mut Context<Self>,
    ) -> u64 {
        self.ide_does_not_answer = true;
        self.questions_timed_out += 1;
        let question = self.questions_timed_out;
        self.unanswered_questions.insert(slot.clone(), question);
        cx.spawn(async move |this, cx| {
            // Even an error shows that there is someone to answer.
            if let Err(error) = response.await {
                log::debug!("the IDE's late answer was an error: {error:#}");
            }
            this.update(cx, |this, _| {
                if this.unanswered_questions.get(&slot) == Some(&question) {
                    this.unanswered_questions.remove(&slot);
                }
                this.may_answer_again();
            })
            .log_err();
        })
        .detach();
        self.answers_seen
    }

    /// The IDE sent something, or came back on a new connection: questions
    /// are worth asking again.
    fn may_answer_again(&mut self) {
        self.ide_does_not_answer = false;
        self.answers_seen += 1;
        for waiter in self.answer_waiters.drain(..) {
            if waiter.send(()).is_err() {
                log::debug!("a launch stopped waiting for the IDE");
            }
        }
    }

    /// Resolves once the IDE sends anything beyond the first `answers_seen`
    /// answers.
    pub(crate) fn wait_until_the_ide_answers(
        &mut self,
        answers_seen: u64,
    ) -> oneshot::Receiver<()> {
        let (sender, receiver) = oneshot::channel();
        if self.answers_seen > answers_seen {
            if sender.send(()).is_err() {
                log::debug!("a launch stopped waiting for the IDE");
            }
            return receiver;
        }
        self.answer_waiters.retain(|waiter| !waiter.is_canceled());
        self.answer_waiters.push(sender);
        receiver
    }

    /// Resolves once the IDE may have room for another launch to wait for
    /// the user.
    pub(crate) fn wait_for_room(&mut self, cx: &mut Context<Self>) -> oneshot::Receiver<()> {
        let (sender, receiver) = oneshot::channel();
        self.room_waiters.retain(|waiter| !waiter.is_canceled());
        self.room_waiters.push(sender);
        if self.room_retry.is_none() {
            self.room_retry = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(ROOM_RETRY_INTERVAL).await;
                this.update(cx, |this, _| this.room_may_have_come())
                    .log_err();
            }));
        }
        receiver
    }

    /// The IDE decided something, came back, or had time to let the
    /// launches it counts run out. Its answers do not count: being told there
    /// is no room is an answer too, and waking the others on it would have
    /// them ask in circles.
    fn room_may_have_come(&mut self) {
        self.room_retry = None;
        for waiter in self.room_waiters.drain(..) {
            if waiter.send(()).is_err() {
                log::debug!("a launch stopped waiting for room at the IDE");
            }
        }
    }

    /// Takes in the IDE's answer unless a decision it pushed after the
    /// question went out is newer, in which case that decision stands. `None`
    /// when such a push took the decision back: the IDE is asked again.
    fn settle_answer(
        &mut self,
        key: &LaunchApprovalKey,
        sent_after: u64,
        answer: RemoteAnswer,
    ) -> Option<RemoteAnswer> {
        let pushed_since = self
            .last_push
            .get(key)
            .is_some_and(|pushed| *pushed > sent_after);
        if pushed_since {
            return self.decision(key).map(RemoteAnswer::of);
        }
        match &answer {
            RemoteAnswer::Approved => self.remember(key, LaunchDecision::Approved),
            RemoteAnswer::Denied => self.remember(key, LaunchDecision::Denied),
            RemoteAnswer::Pending
            | RemoteAnswer::Unavailable(_)
            | RemoteAnswer::Deferred(_)
            | RemoteAnswer::Unanswered { .. } => {}
        }
        Some(answer)
    }

    fn remember(&mut self, key: &LaunchApprovalKey, decision: LaunchDecision) {
        self.decisions.insert(key.clone(), decision);
        self.pending_requesters.remove(key);
        for waiter in self.decision_waiters.remove(key).into_iter().flatten() {
            if waiter.send(decision).is_err() {
                log::debug!("a launch stopped waiting before its decision arrived");
            }
        }
    }

    /// Resolves once the IDE decides about `key`, at once when it already
    /// has.
    pub(crate) fn wait_for_decision(
        &mut self,
        key: &LaunchApprovalKey,
    ) -> oneshot::Receiver<LaunchDecision> {
        let (sender, receiver) = oneshot::channel();
        if let Some(decision) = self.decision(key) {
            if sender.send(decision).is_err() {
                log::debug!("a launch stopped waiting before its decision arrived");
            }
            return receiver;
        }
        let waiters = self.decision_waiters.entry(key.clone()).or_default();
        waiters.retain(|waiter| !waiter.is_canceled());
        waiters.push(sender);
        receiver
    }

    pub(crate) fn keep_pending(
        &mut self,
        key: &LaunchApprovalKey,
        requester: impl Into<LaunchRequester>,
    ) {
        self.pending_requesters
            .entry(key.clone())
            .or_default()
            .insert(requester.into());
    }

    pub(crate) fn pending_launch_guard(
        &mut self,
        key: &LaunchApprovalKey,
        requester: impl Into<LaunchRequester>,
    ) -> PendingLaunchGuard {
        let requester = requester.into();
        self.keep_pending(key, requester);
        PendingLaunchGuard::new(key.clone(), requester, self.withdrawals.clone())
    }

    /// `requester` no longer waits for `key`. Once nobody does - a second
    /// wait by the same entity counts as another holder - the IDE is told, so
    /// that its notification about the launch goes away.
    pub(crate) fn withdraw_pending(
        &mut self,
        key: &LaunchApprovalKey,
        requester: impl Into<LaunchRequester>,
        cx: &mut Context<Self>,
    ) {
        let requester = requester.into();
        let Some(requesters) = self.pending_requesters.get_mut(key) else {
            return;
        };
        requesters.remove(&requester);
        if !requesters.is_empty() {
            return;
        }
        self.pending_requesters.remove(key);
        self.decision_waiters.remove(key);
        if self.decisions.contains_key(key) {
            return;
        }
        *self.withdrawals_sent.entry(key.clone()).or_default() += 1;
        let request = self.client.request(proto::WithdrawProjectLaunchApproval {
            project_id: self.project_id,
            remote_key: key.to_string(),
        });
        cx.background_spawn(async move {
            request.await?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn record_push(
        &mut self,
        key: LaunchApprovalKey,
        decision: Option<LaunchDecision>,
        cx: &mut Context<Self>,
    ) {
        self.may_answer_again();
        self.room_may_have_come();
        self.pushes_received += 1;
        self.last_push.insert(key.clone(), self.pushes_received);
        match decision {
            Some(decision) => {
                self.remember(&key, decision);
                cx.emit(ProjectLaunchApprovalsEvent::LaunchDecided { key, decision });
            }
            None => {
                self.decisions.remove(&key);
                cx.emit(ProjectLaunchApprovalsEvent::LaunchApprovalRevoked(key));
            }
        }
    }

    async fn handle_launch_approval_decided(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::ProjectLaunchApprovalDecided>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let key = parse_remote_key(&envelope.payload.remote_key)?;
        let decision = if envelope.payload.approved {
            LaunchDecision::Approved
        } else {
            LaunchDecision::Denied
        };
        this.update(&mut cx, |this, cx| {
            this.record_push(key, Some(decision), cx)
        });
        Ok(proto::Ack {})
    }

    async fn handle_launch_approval_revoked(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::ProjectLaunchApprovalRevoked>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let key = parse_remote_key(&envelope.payload.remote_key)?;
        this.update(&mut cx, |this, cx| this.record_push(key, None, cx));
        Ok(proto::Ack {})
    }
}

/// The IDE's side (ADR-F50), one per remote project: answers the remote
/// server's questions from the app-wide [`ProjectLaunchApprovals`], on the
/// project's behalf, and passes the user's later decisions about them back.
pub struct RemoteLaunchRequests {
    client: AnyProtoClient,
    project_id: u64,
    project: WeakEntity<Project>,
    remote_client: WeakEntity<RemoteClient>,
    worktree_store: WeakEntity<WorktreeStore>,
    languages: Arc<LanguageRegistry>,
    /// Each key asked about here, in the order they were last asked about.
    /// Revoked ones stay, so that a reconnection repeats the revocation.
    requested: IndexMap<LaunchApprovalKey, RequestedLaunch>,
    /// When the remote server last stopped waiting for a launch nobody
    /// decided, and which, for each such launch it has not asked about since.
    recent_withdrawals: HashMap<LaunchSlot, WithdrawnLaunch>,
    /// Withdrawals waiting out [`WITHDRAWAL_GRACE`].
    withdrawals_due: HashMap<LaunchApprovalKey, Task<()>>,
    connected: bool,
    _subscriptions: Vec<Subscription>,
}

struct RequestedLaunch {
    /// The remote server's key that the IDE's key stands for.
    remote_key: LaunchApprovalKey,
    slot: LaunchSlot,
}

struct WithdrawnLaunch {
    at: Instant,
    key: LaunchApprovalKey,
}

impl RemoteLaunchRequests {
    pub(crate) fn new(
        client: AnyProtoClient,
        project_id: u64,
        project: WeakEntity<Project>,
        remote_client: &Entity<RemoteClient>,
        worktree_store: &Entity<WorktreeStore>,
        languages: Arc<LanguageRegistry>,
        cx: &mut Context<Self>,
    ) -> Self {
        let requester = project.entity_id();
        let approvals = ProjectLaunchApprovals::global(cx);
        let subscriptions = vec![
            cx.subscribe(&approvals, Self::on_launch_approvals_event),
            cx.observe(remote_client, Self::on_connection_changed),
            cx.on_release(move |_, cx| {
                approvals.update(cx, |approvals, cx| {
                    approvals.withdraw_requester(requester, cx)
                });
            }),
        ];
        Self {
            client,
            project_id,
            project,
            remote_client: remote_client.downgrade(),
            worktree_store: worktree_store.downgrade(),
            languages,
            requested: IndexMap::default(),
            recent_withdrawals: HashMap::default(),
            withdrawals_due: HashMap::default(),
            connected: remote_client.read(cx).connection_state() == ConnectionState::Connected,
            _subscriptions: subscriptions,
        }
    }

    pub(crate) async fn handle_request_project_launch_approval(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::RequestProjectLaunchApproval>,
        mut cx: AsyncApp,
    ) -> Result<proto::RequestProjectLaunchApprovalResponse> {
        this.update(&mut cx, |this, cx| this.answer(envelope.payload, cx))
    }

    pub(crate) async fn handle_withdraw_project_launch_approval(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::WithdrawProjectLaunchApproval>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let remote_key = parse_remote_key(&envelope.payload.remote_key)?;
        this.update(&mut cx, |this, cx| this.withdraw(&remote_key, cx));
        Ok(proto::Ack {})
    }

    /// Answers at once with the current state; a decision made later is
    /// pushed from [`Self::on_launch_approvals_event`].
    fn answer(
        &mut self,
        request: proto::RequestProjectLaunchApproval,
        cx: &mut Context<Self>,
    ) -> Result<proto::RequestProjectLaunchApprovalResponse> {
        use proto::ProjectLaunchApprovalState as State;

        check_request_limits(&request)?;
        let remote_key = parse_remote_key(&request.remote_key)?;
        let worktree_store = self
            .worktree_store
            .upgrade()
            .context(i18n::t!("The project is closed"))?;
        let worktree = worktree_store
            .read(cx)
            .worktree_for_id(WorktreeId::from_proto(request.worktree_id), cx)
            .with_context(|| {
                i18n::t!(
                    "The worktree the remote host asked about (ID {id}) is not in this project",
                    id = request.worktree_id
                )
            })?;
        let (worktree_abs_path, worktree_is_a_file) = {
            let worktree = worktree.read(cx);
            (
                worktree.abs_path(),
                worktree.root_entry().is_some_and(|entry| entry.is_file()),
            )
        };
        check_launch_place(&request, &worktree_abs_path, worktree_is_a_file)?;
        let remote_client = self
            .remote_client
            .upgrade()
            .context(i18n::t!("The connection to the remote host is closed"))?;
        let host = LaunchHost::from_remote_connection(&remote_client.read(cx).connection_options());
        let details = details_from_proto(request, host, worktree_abs_path)?;

        let approvals = ProjectLaunchApprovals::global(cx);
        self.forget_expired_withdrawals(cx);
        if let Some((state, reason)) = self.refusal(&details, &approvals, cx) {
            log::warn!(
                "not asking the user about {:?} for the remote server: {reason}",
                details.name
            );
            return Ok(proto::RequestProjectLaunchApprovalResponse {
                state: state as i32,
                reason: Some(reason),
            });
        }
        let requester = self.project.entity_id();
        let slot = LaunchSlot::of(&details);
        let details = approvals.update(cx, |approvals, cx| {
            approvals.request_launch(details, requester, cx)
        });
        // Asked about again, a launch it stopped waiting for was restarted:
        // it stays in front of the user as it is.
        self.withdrawals_due.remove(&details.key);
        if let Some(withdrawn) = self.recent_withdrawals.remove(&slot)
            && withdrawn.key != details.key
        {
            approvals.update(cx, |approvals, _| {
                approvals.forget_notification(&withdrawn.key)
            });
        }
        self.remember_request(
            details.key.clone(),
            RequestedLaunch { remote_key, slot },
            &approvals,
            cx,
        );
        let state = match details.state {
            LaunchApprovalState::Pending => State::LaunchApprovalPending,
            LaunchApprovalState::Approved => State::LaunchApprovalApproved,
            LaunchApprovalState::Denied => State::LaunchApprovalDenied,
        };
        Ok(proto::RequestProjectLaunchApprovalResponse {
            state: state as i32,
            reason: details.store_error.map(|error| error.to_string()),
        })
    }

    /// Why a question from the remote server is not put to the user at all,
    /// with the state to answer it with.
    fn refusal(
        &self,
        details: &LaunchApprovalDetails,
        approvals: &Entity<ProjectLaunchApprovals>,
        cx: &App,
    ) -> Option<(proto::ProjectLaunchApprovalState, String)> {
        use proto::ProjectLaunchApprovalState as State;

        if details.kind == LaunchKind::LanguageServerBinary {
            let name = LanguageServerName(details.name.clone());
            if self.languages.adapter_for_name(&name).is_none()
                && !self.languages.is_lsp_adapter_available(&name)
            {
                return Some((
                    State::LaunchApprovalUnavailable,
                    i18n::t!(
                        "A launch of language server \"{name}\", which this IDE does not know, cannot be approved",
                        name = escape_for_display(&details.name)
                    ),
                ));
            }
        }
        let approvals = approvals.read(cx);
        if self.requested.contains_key(&details.key) {
            return None;
        }
        // Only an undecided key waits, and so raises a notification; a launch
        // that waits already, or did until a restart, keeps its room.
        if approvals.decision(&details.key).is_none() {
            let now = cx.background_executor().now();
            let mut waiting = self
                .requested
                .iter()
                .filter(|(key, _)| approvals.pending_launch(key).is_some())
                .map(|(_, requested)| &requested.slot)
                .collect::<HashSet<_>>();
            waiting.extend(
                self.recent_withdrawals
                    .iter()
                    .filter(|(_, withdrawn)| {
                        now.saturating_duration_since(withdrawn.at) < WITHDRAWN_COOLDOWN
                    })
                    .map(|(slot, _)| slot),
            );
            if !waiting.contains(&LaunchSlot::of(details)) && waiting.len() >= MAX_PENDING_LAUNCHES
            {
                return Some((
                    State::LaunchApprovalDeferred,
                    i18n::t!(
                        "Too many launches from the remote host are waiting for approval (at most {MAX_PENDING_LAUNCHES})"
                    ),
                ));
            }
        }
        let nothing_to_forget = self.requested.len() >= MAX_REMEMBERED_LAUNCHES
            && !self
                .requested
                .keys()
                .any(|key| Self::can_be_forgotten(approvals, key));
        nothing_to_forget.then(|| {
            (
                State::LaunchApprovalUnavailable,
                i18n::t!(
                    "Too many approved launches were asked about by the remote host (at most {MAX_REMEMBERED_LAUNCHES})"
                ),
            )
        })
    }

    /// Neither waiting nor approved: forgetting it keeps nothing running
    /// from hearing a revocation.
    fn can_be_forgotten(approvals: &ProjectLaunchApprovals, key: &LaunchApprovalKey) -> bool {
        approvals.pending_launch(key).is_none()
            && approvals.decision(key) != Some(LaunchDecision::Approved)
    }

    fn remember_request(
        &mut self,
        key: LaunchApprovalKey,
        requested: RequestedLaunch,
        approvals: &Entity<ProjectLaunchApprovals>,
        cx: &App,
    ) {
        // The last asked about comes last, which is what a reconnection
        // repeats for each remote key.
        self.requested.shift_remove(&key);
        self.requested.insert(key, requested);
        if self.requested.len() <= MAX_REMEMBERED_LAUNCHES {
            return;
        }
        let approvals = approvals.read(cx);
        let forgettable = self
            .requested
            .keys()
            .find(|key| Self::can_be_forgotten(approvals, key))
            .cloned();
        if let Some(key) = forgettable {
            log::info!("forgetting the oldest settled launch the remote server asked about");
            self.requested.shift_remove(&key);
        }
    }

    /// The remote server stopped waiting for `remote_key`. What the user sees
    /// of it stays for [`WITHDRAWAL_GRACE`], in case it is asked about again.
    fn withdraw(&mut self, remote_key: &LaunchApprovalKey, cx: &mut Context<Self>) {
        let approvals = ProjectLaunchApprovals::global(cx).read(cx);
        let undecided = self
            .requested
            .iter()
            .filter(|(key, requested)| {
                requested.remote_key == *remote_key && approvals.decision(key).is_none()
            })
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        for key in undecided {
            if self.withdrawals_due.contains_key(&key) {
                continue;
            }
            let dismissal = cx.spawn({
                let key = key.clone();
                async move |this, cx| {
                    cx.background_executor().timer(WITHDRAWAL_GRACE).await;
                    this.update(cx, |this, cx| this.dismiss_withdrawn(&key, cx))
                        .log_err();
                }
            });
            self.withdrawals_due.insert(key, dismissal);
        }
    }

    /// Takes away a launch the remote server stopped waiting for and did not
    /// ask about again. Its notification is remembered until the cooldown
    /// runs out, so that asking about it later brings nothing up again.
    fn dismiss_withdrawn(&mut self, key: &LaunchApprovalKey, cx: &mut Context<Self>) {
        self.withdrawals_due.remove(key);
        let Some(slot) = self
            .requested
            .get(key)
            .map(|requested| requested.slot.clone())
        else {
            return;
        };
        let approvals = ProjectLaunchApprovals::global(cx);
        let requester = self.project.entity_id();
        let decided = approvals.update(cx, |approvals, cx| {
            approvals.withdraw_pending(key, requester, cx);
            approvals.decision(key).is_some()
        });
        // A decided key stays, so that taking the decision back reaches the
        // remote server.
        if decided {
            return;
        }
        self.requested.shift_remove(key);
        let withdrawn = WithdrawnLaunch {
            at: cx.background_executor().now(),
            key: key.clone(),
        };
        if let Some(earlier) = self.recent_withdrawals.insert(slot, withdrawn)
            && earlier.key != *key
        {
            approvals.update(cx, |approvals, _| {
                approvals.forget_notification(&earlier.key)
            });
        }
        self.forget_expired_withdrawals(cx);
    }

    /// Forgets the withdrawn launches whose cooldown ran out, notifications
    /// included.
    fn forget_expired_withdrawals(&mut self, cx: &mut Context<Self>) {
        let now = cx.background_executor().now();
        let mut expired = Vec::new();
        self.recent_withdrawals.retain(|_, withdrawn| {
            let recent = now.saturating_duration_since(withdrawn.at) < WITHDRAWN_COOLDOWN;
            if !recent {
                expired.push(withdrawn.key.clone());
            }
            recent
        });
        if expired.is_empty() {
            return;
        }
        ProjectLaunchApprovals::global(cx).update(cx, |approvals, _| {
            for key in &expired {
                approvals.forget_notification(key);
            }
        });
    }

    fn on_launch_approvals_event(
        &mut self,
        approvals: Entity<ProjectLaunchApprovals>,
        event: &ProjectLaunchApprovalsEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            ProjectLaunchApprovalsEvent::LaunchDecided { key, decision } => {
                if let Some(remote_key) = self
                    .requested
                    .get(key)
                    .map(|requested| requested.remote_key.clone())
                {
                    self.push(&remote_key, Some(*decision == LaunchDecision::Approved), cx);
                }
            }
            ProjectLaunchApprovalsEvent::LaunchApprovalRevoked(key) => {
                if let Some(remote_key) = self
                    .requested
                    .get(key)
                    .map(|requested| requested.remote_key.clone())
                {
                    self.push(&remote_key, None, cx);
                }
            }
            // Nothing waits for these any more, and nobody decided them. Asked
            // about again, they deserve a notification again.
            ProjectLaunchApprovalsEvent::PendingLaunchReplaced { old: key, .. }
            | ProjectLaunchApprovalsEvent::PendingLaunchWithdrawn(key) => {
                self.withdrawals_due.remove(key);
                if self.requested.contains_key(key) && approvals.read(cx).decision(key).is_none() {
                    self.requested.shift_remove(key);
                    approvals.update(cx, |approvals, _| approvals.forget_notification(key));
                }
            }
            ProjectLaunchApprovalsEvent::PendingLaunchAppeared(_) => {}
        }
    }

    fn on_connection_changed(
        &mut self,
        remote_client: Entity<RemoteClient>,
        cx: &mut Context<Self>,
    ) {
        let connected = remote_client.read(cx).connection_state() == ConnectionState::Connected;
        let reconnected = connected && !self.connected;
        self.connected = connected;
        if reconnected {
            self.push_current_decisions(cx);
        }
    }

    /// After a reconnection the remote server hears again what was decided
    /// about the latest key for each of its own, in case a push was lost
    /// with the old connection.
    fn push_current_decisions(&mut self, cx: &mut Context<Self>) {
        let approvals = ProjectLaunchApprovals::global(cx);
        let pushes = {
            let approvals = approvals.read(cx);
            let mut repeated = HashSet::default();
            self.requested
                .iter()
                .rev()
                .filter(|(_, requested)| repeated.insert(requested.remote_key.clone()))
                .filter_map(|(key, requested)| {
                    let remote_key = requested.remote_key.clone();
                    match approvals.decision(key) {
                        Some(decision) => {
                            Some((remote_key, Some(decision == LaunchDecision::Approved)))
                        }
                        None if approvals.pending_launch(key).is_none() => Some((remote_key, None)),
                        None => None,
                    }
                })
                .collect::<Vec<_>>()
        };
        for (remote_key, approved) in pushes {
            self.push(&remote_key, approved, cx);
        }
    }

    /// Passes a decision on to the remote server; `None` takes it back.
    fn push(&self, remote_key: &LaunchApprovalKey, approved: Option<bool>, cx: &mut Context<Self>) {
        let project_id = self.project_id;
        let remote_key = remote_key.to_string();
        let pushed = match approved {
            Some(approved) => {
                let request = self.client.request(proto::ProjectLaunchApprovalDecided {
                    project_id,
                    remote_key,
                    approved,
                });
                cx.background_spawn(async move {
                    request.await?;
                    anyhow::Ok(())
                })
            }
            None => {
                let request = self.client.request(proto::ProjectLaunchApprovalRevoked {
                    project_id,
                    remote_key,
                });
                cx.background_spawn(async move {
                    request.await?;
                    anyhow::Ok(())
                })
            }
        };
        let revocation = approved.is_none();
        let connected = self.remote_client.upgrade().is_some_and(|remote_client| {
            remote_client.read(cx).connection_state() == ConnectionState::Connected
        });
        if revocation && !connected {
            self.tell_that_the_revocation_waits(cx);
        }
        cx.spawn(async move |this, cx| {
            if let Err(error) = pushed.await {
                log::warn!("could not pass a launch decision on to the remote server: {error:#}");
                if revocation {
                    this.update(cx, |this, cx| this.tell_that_the_revocation_waits(cx))
                        .log_err();
                }
            }
        })
        .detach();
    }

    fn tell_that_the_revocation_waits(&self, cx: &mut Context<Self>) {
        self.project
            .update(cx, |_, cx| {
                cx.emit(Event::Toast {
                    notification_id: REVOCATION_WAITS_NOTIFICATION.into(),
                    message: revocation_waits_message().to_string(),
                    link: None,
                })
            })
            .log_err();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{self, AtomicUsize};

    use futures::FutureExt as _;

    use super::*;

    fn remote_host(identity: &str) -> LaunchHost {
        LaunchHost::Remote {
            identity: identity.to_string().into(),
            display_name: identity.to_string().into(),
        }
    }

    fn place() -> LaunchPlace {
        LaunchPlace {
            worktree_id: WorktreeId::from_usize(3),
            working_directory: Some(PathBuf::from("/code/project1")),
        }
    }

    /// A launch as the remote server resolves it, with every kind of warning
    /// and characters that must not show up as themselves.
    fn remote_details() -> LaunchApprovalDetails {
        let mut details = LaunchApprovalDetails::for_tests(
            &"a".repeat(64),
            LaunchKind::LanguageServerBinary,
            "project\u{200B}_lsp",
            LaunchTarget::Command {
                program: "/code/project1/bin/lsp".into(),
                arguments: vec!["--stdio".into(), "\u{202E}sj.exe".into(), "".into()],
            },
        );
        details.environment_variable_names = vec!["API_TOKEN".into()];
        details.code_loading_environment =
            vec![("NODE_OPTIONS".into(), "--require=./hook\u{2066}.js".into())];
        details.definition_file =
            LaunchDefinitionFile::Worktree("/code/project1/.zed/settings.json".into());
        details.worktree_abs_path = Path::new("/code/project1").into();
        details.referenced_worktree_files = vec!["/code/project1/bin/lsp".into()];
        details.unbound_worktree_content = vec![
            UnboundWorktreeContent::WorkingDirectory {
                program: "node".into(),
                working_directory: "/code/project1".into(),
            },
            UnboundWorktreeContent::File("/code/project1/conf.toml".into()),
            UnboundWorktreeContent::Directory("/code/project1/pkg".into()),
            UnboundWorktreeContent::ArgumentFile("/code/project1/args.txt".into()),
            UnboundWorktreeContent::Wrapper("direnv".into()),
            UnboundWorktreeContent::RemoteFile("/code/project1/other.js".into()),
            UnboundWorktreeContent::Unreadable("/code/project1/locked.js".into()),
        ];
        details.approval_is_session_only = true;
        details.replaces_user_entry = true;
        details
    }

    fn remote_request() -> proto::RequestProjectLaunchApproval {
        request_to_proto(&remote_details(), 0, &place(), Some("machine")).unwrap()
    }

    #[test]
    fn test_details_survive_the_trip_to_the_ide() {
        let details = remote_details();
        let request = remote_request();
        assert_eq!(request.remote_key, details.key.as_str());
        assert_eq!(request.worktree_id, 3);
        assert_eq!(request.host_machine.as_deref(), Some("machine"));

        let host = remote_host("ssh://dev@build-01");
        let received =
            details_from_proto(request, host.clone(), details.worktree_abs_path.clone()).unwrap();
        assert_eq!(received.kind, details.kind);
        assert_eq!(received.name, details.name);
        assert_eq!(received.target, details.target);
        assert_eq!(
            received.environment_variable_names,
            details.environment_variable_names
        );
        assert_eq!(
            received.code_loading_environment,
            details.code_loading_environment
        );
        assert_eq!(received.definition_file, details.definition_file);
        assert_eq!(received.worktree_abs_path, details.worktree_abs_path);
        assert_eq!(
            received.referenced_worktree_files,
            details.referenced_worktree_files
        );
        assert_eq!(
            received.unbound_worktree_content,
            details.unbound_worktree_content
        );
        assert!(received.approval_is_session_only);
        assert!(received.replaces_user_entry);
        assert_eq!(received.host, host);
        assert_eq!(
            received.execution_host,
            ExecutionHost::Remote {
                display_name: "ssh://dev@build-01".into()
            }
        );
        assert_eq!(received.state, LaunchApprovalState::Pending);
        assert_eq!(
            received.key,
            remote_launch_key(
                &host,
                Some("machine"),
                &details.key,
                &shown_content(&received, Some("/code/project1"))
            )
        );
        assert_ne!(received.key, details.key);

        // Shown the way it runs: the characters arrive as they were and are
        // escaped only for display.
        assert_eq!(
            received.target.display().to_string(),
            "/code/project1/bin/lsp --stdio <U+202E>sj.exe \"\""
        );
        assert_eq!(escape_for_display(&received.name), "project<U+200B>_lsp");

        let mut url = remote_details();
        url.target = LaunchTarget::Url("https://example.com".into());
        assert!(request_to_proto(&url, 0, &place(), None).is_err());
        let mut context_server = remote_details();
        context_server.kind = LaunchKind::ContextServer;
        assert!(
            request_to_proto(&context_server, 0, &place(), None).is_err(),
            "MCP servers are not asked about this way"
        );
    }

    #[test]
    fn test_requests_beyond_the_limits_are_refused() {
        let request = remote_request();
        let host = remote_host("ssh://build-01");
        let root: Arc<Path> = Path::new("/code/project1").into();
        let accepts = |request: proto::RequestProjectLaunchApproval| {
            details_from_proto(request, host.clone(), root.clone()).is_ok()
        };
        assert!(accepts(request.clone()));

        let mut many_arguments = request.clone();
        many_arguments.arguments = vec!["-v".to_string(); MAX_ARGUMENTS];
        assert!(accepts(many_arguments.clone()));
        many_arguments.arguments.push("-v".to_string());
        assert!(!accepts(many_arguments));

        let mut long_argument = request.clone();
        long_argument.arguments = vec!["x".repeat(MAX_STRING_BYTES)];
        assert!(accepts(long_argument.clone()));
        long_argument.arguments = vec!["x".repeat(MAX_STRING_BYTES + 1)];
        assert!(!accepts(long_argument));

        let mut large = request.clone();
        large.arguments =
            vec!["x".repeat(MAX_STRING_BYTES); MAX_TOTAL_BYTES / MAX_STRING_BYTES - 1];
        assert!(accepts(large.clone()));
        large.arguments =
            vec!["x".repeat(MAX_STRING_BYTES); MAX_TOTAL_BYTES / MAX_STRING_BYTES + 1];
        assert!(!accepts(large));

        let mut many_warnings = request.clone();
        many_warnings.referenced_worktree_files = vec!["a".to_string(); MAX_LIST_ENTRIES + 1];
        assert!(!accepts(many_warnings));

        let mut malformed_key = request.clone();
        malformed_key.remote_key = "not-a-digest".to_string();
        assert!(!accepts(malformed_key));

        let mut unknown_kind = request.clone();
        unknown_kind.kind = proto::ProjectLaunchKind::LaunchKindUnknown as i32;
        assert!(!accepts(unknown_kind));

        let mut unknown_warning = request;
        unknown_warning.unbound_worktree_content = vec![proto::ProjectLaunchUnboundContent {
            kind: proto::ProjectLaunchUnboundKind::UnboundUnknown as i32,
            path: "/code/project1/x".to_string(),
            program: None,
        }];
        assert!(!accepts(unknown_warning), "a warning it cannot show");

        // The remote server does not send what the IDE would refuse.
        let mut too_many = remote_details();
        too_many.target = LaunchTarget::Command {
            program: "lsp".into(),
            arguments: vec!["-v".into(); MAX_ARGUMENTS + 1],
        };
        assert!(request_to_proto(&too_many, 0, &place(), None).is_err());
    }

    #[test]
    fn test_the_key_follows_the_host_the_machine_and_what_is_shown() {
        let root: Arc<Path> = Path::new("/code/project1").into();
        let key_of = |request: proto::RequestProjectLaunchApproval, host: &str| {
            details_from_proto(request, remote_host(host), root.clone())
                .unwrap()
                .key
        };
        let request = remote_request();
        assert_eq!(
            key_of(request.clone(), "ssh://build-01"),
            key_of(request.clone(), "ssh://build-01")
        );
        assert_ne!(
            key_of(request.clone(), "ssh://build-01"),
            key_of(request.clone(), "ssh://build-02"),
            "the same request from two hosts is decided twice"
        );

        let mut other_machine = request.clone();
        other_machine.host_machine = Some("another machine".to_string());
        assert_ne!(
            key_of(request.clone(), "ssh://build-01"),
            key_of(other_machine, "ssh://build-01"),
            "two machines behind one name are told apart"
        );

        // The same digest with other content is not what the user approved.
        let mut other_program = request.clone();
        other_program.program = "/code/project1/bin/other".to_string();
        let mut other_argument = request.clone();
        other_argument.arguments.push("--inspect".to_string());
        let mut other_file = request.clone();
        other_file.referenced_worktree_files.clear();
        let mut not_session_only = request.clone();
        not_session_only.approval_is_session_only = false;
        let mut other_directory = request.clone();
        other_directory.working_directory = None;
        for changed in [
            other_program,
            other_argument,
            other_file,
            not_session_only,
            other_directory,
        ] {
            assert_ne!(
                key_of(request.clone(), "ssh://build-01"),
                key_of(changed, "ssh://build-01")
            );
        }
    }

    #[test]
    fn test_a_worktree_the_ide_knows_differently_is_refused() {
        let request = remote_request();
        assert!(check_launch_place(&request, Path::new("/code/project1"), false).is_ok());
        assert!(
            check_launch_place(&request, Path::new("/code/other"), false).is_err(),
            "another worktree under the same id"
        );

        let mut elsewhere = request.clone();
        elsewhere.working_directory = Some("/tmp".to_string());
        assert!(check_launch_place(&elsewhere, Path::new("/code/project1"), false).is_err());

        // A formatter for a worktree that is one file starts next to it.
        let mut single_file = request;
        single_file.worktree_abs_path = "/code/notes.md".to_string();
        single_file.working_directory = Some("/code".to_string());
        assert!(check_launch_place(&single_file, Path::new("/code/notes.md"), true).is_ok());
        assert!(check_launch_place(&single_file, Path::new("/code/notes.md"), false).is_err());
    }

    #[test]
    fn test_answers_that_say_nothing_refuse_the_launch() {
        use proto::ProjectLaunchApprovalState as State;
        let answer = |state: i32, reason: Option<&str>| {
            answer_from_response(proto::RequestProjectLaunchApprovalResponse {
                state,
                reason: reason.map(str::to_string),
            })
        };
        assert_eq!(
            answer(State::LaunchApprovalApproved as i32, None),
            RemoteAnswer::Approved
        );
        assert_eq!(
            answer(State::LaunchApprovalPending as i32, Some("読み込み中")),
            RemoteAnswer::Pending
        );
        assert!(matches!(
            answer(State::LaunchApprovalUnavailable as i32, None),
            RemoteAnswer::Unavailable(_)
        ));
        assert!(matches!(
            answer(State::LaunchApprovalDeferred as i32, None),
            RemoteAnswer::Deferred(_)
        ));
        assert!(matches!(answer(42, None), RemoteAnswer::Unavailable(_)));
    }

    /// Counts the withdrawals that reach the IDE, and answers nothing: what
    /// the request comes back with is beside the point here.
    struct CountingProtoClient {
        handler_set: parking_lot::Mutex<rpc::ProtoMessageHandlerSet>,
        withdrawals: Arc<AtomicUsize>,
    }

    impl rpc::ProtoClient for CountingProtoClient {
        fn request(
            &self,
            envelope: proto::Envelope,
            _: &'static str,
        ) -> futures::future::BoxFuture<'static, Result<proto::Envelope>> {
            if matches!(
                envelope.payload,
                Some(proto::envelope::Payload::WithdrawProjectLaunchApproval(_))
            ) {
                self.withdrawals.fetch_add(1, atomic::Ordering::SeqCst);
            }
            futures::future::ready(Err(anyhow::anyhow!("this test has no IDE"))).boxed()
        }

        fn send(&self, _: proto::Envelope, _: &'static str) -> Result<()> {
            Ok(())
        }

        fn send_response(&self, _: proto::Envelope, _: &'static str) -> Result<()> {
            Ok(())
        }

        fn message_handler_set(&self) -> &parking_lot::Mutex<rpc::ProtoMessageHandlerSet> {
            &self.handler_set
        }

        fn is_via_collab(&self) -> bool {
            false
        }

        fn has_wsl_interop(&self) -> bool {
            false
        }
    }

    /// The remote server tells the IDE to take its notification away only once
    /// nothing on this side waits any more: a language server started twice
    /// waits twice for the same key from the same project entity (ADR-F50).
    #[gpui::test]
    async fn test_two_waits_hold_a_remote_launch_before_the_ide_is_told(
        cx: &mut gpui::TestAppContext,
    ) {
        let withdrawals = Arc::new(AtomicUsize::new(0));
        let client = AnyProtoClient::new(Arc::new(CountingProtoClient {
            handler_set: parking_lot::Mutex::default(),
            withdrawals: withdrawals.clone(),
        }));
        let approvals = cx.new(|cx| RemoteLaunchApprovals::new(client, 7, "build-01", cx));
        let key = LaunchApprovalKey::for_tests("shared");
        let store = cx.new(|_| ()).entity_id();
        let first_start = LaunchRequester::waiting(store);
        let second_start = LaunchRequester::waiting(store);

        approvals.update(cx, |approvals, _| {
            approvals.keep_pending(&key, first_start);
            approvals.keep_pending(&key, second_start);
        });
        approvals.update(cx, |approvals, cx| {
            approvals.withdraw_pending(&key, first_start, cx);
            assert_eq!(
                approvals.withdrawals_sent(&key),
                0,
                "the second start still waits for the same launch"
            );
        });
        cx.run_until_parked();
        assert_eq!(withdrawals.load(atomic::Ordering::SeqCst), 0);

        approvals.update(cx, |approvals, cx| {
            approvals.withdraw_pending(&key, second_start, cx);
            assert_eq!(approvals.withdrawals_sent(&key), 1);
        });
        cx.run_until_parked();
        assert_eq!(
            withdrawals.load(atomic::Ordering::SeqCst),
            1,
            "with nothing waiting the IDE is told once"
        );
    }
}
