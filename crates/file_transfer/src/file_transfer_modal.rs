//! ADR-F28: the two-pane transfer screen (local on the left, remote on the
//! right) that `terminal::OpenFileTransfer` opens.
//!
//! Authentication prompts are rendered *inside* this modal rather than as a
//! second workspace modal: the modal layer only keeps one modal at a time, so
//! a nested askpass dialog would push this one off the screen mid-transfer.

use std::{collections::HashSet, ops::Range, path::PathBuf, sync::Arc};

use anyhow::Result;
use askpass::{AskPassDelegate, EncryptedPassword, IKnowWhatIAmDoingAndIHaveReadTheDocs};
use editor::Editor;
use fs::Fs;
use futures::channel::oneshot;
use gpui::{
    AnyElement, AsyncApp, ClickEvent, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    Modifiers, Render, Subscription, Task, Window, actions, uniform_list,
};
use project::{Project, remote_exec::TransferAskPass};
use settings::SettingsStore;
use terminal::{
    quick_connect_credentials::{QuickConnectCredentials, StoredPassword},
    terminal_macro::{ConnectionProtocol, TerminalSessionStore, normalize_folder},
};
use ui::{
    Checkbox, Divider, InputFrameSize, ToggleState, Tooltip, input_frame, input_frame_sized,
    prelude::*,
};
use util::ResultExt as _;
use workspace::{ModalView, Workspace};
use zeroize::Zeroize as _;

use crate::{
    connection_presets::{
        ConnectionPreset, PresetGroup, PresetSource, can_move_connection, move_connection,
        preset_connection, preset_groups,
    },
    transfer_backend::{
        AskPassAnswerMode, AskPassFactory, DirectoryEntry, EntryKind, ExecHost, HOST_KEY_ACCEPTED,
        HOST_KEY_REFUSED, IdeExecHost, RemoteExecHost, RemoteTarget, StoredPasswordUse,
        TransferBackend, TransferProtocol, askpass_answer_mode, credential_lookups,
        decide_stored_password_use, format_size, normalize_remote_dir, remote_join,
        validate_entry_name, validate_host, validate_user,
    },
};

/// The credential key a remembered password was found under, so re-saving it
/// updates the entry that is actually in use — quick connect's own, when that
/// is where it came from (ADR-F29).
type CredentialKey = (ConnectionProtocol, u16);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaneSide {
    Local,
    Remote,
}

impl PaneSide {
    fn label(self) -> &'static str {
        match self {
            PaneSide::Local => i18n::t!("Local"),
            PaneSide::Remote => i18n::t!("Remote"),
        }
    }
}

actions!(
    file_transfer,
    [
        /// Moves to the next field.
        FocusNextField,
        /// Moves to the previous field.
        FocusPreviousField,
    ]
);

/// Tab order of the modal's text fields (ADR-F28 追記). The connection bar
/// comes first because it is always rendered; the 接続先 pane's two fields
/// only exist while it is open, and the panes' path fields come last.
///
/// These place the fields inside the modal's own tab group for anything that
/// walks the window's tab stops (a click on the modal frame followed by Tab,
/// say). Tab itself is served by [`FileTransferModal::focus_adjacent_field`],
/// which cycles [`FileTransferModal::tab_order_fields`] instead, so that it
/// wraps back into the modal rather than out to whatever the rest of the
/// window painted.
const HOST_FIELD_TAB_INDEX: isize = 0;
const PORT_FIELD_TAB_INDEX: isize = 1;
const USER_FIELD_TAB_INDEX: isize = 2;
const PASSWORD_FIELD_TAB_INDEX: isize = 3;
const PRESET_NAME_FIELD_TAB_INDEX: isize = 4;
const PRESET_FOLDER_FIELD_TAB_INDEX: isize = 5;
const LOCAL_PATH_FIELD_TAB_INDEX: isize = 6;
const REMOTE_PATH_FIELD_TAB_INDEX: isize = 7;
const PROMPT_FIELD_TAB_INDEX: isize = 8;

/// Which machine the left-hand pane — and the transfer programs — belong to.
///
/// Offered only for SSH remote projects, where it defaults to the remote host:
/// the project's terminals and paths are over there, so a path such as
/// `/home/ubuntu` has to be looked up on the remote rather than on the Windows
/// IDE, where it does not exist (ADR-F28).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalSide {
    IdeHost,
    RemoteHost,
}

/// What [`open_file_transfer`] found out about the two candidate hosts, before
/// the modal turns them into [`ExecHost`]s of its own (which it must do itself,
/// because they carry callbacks that need a handle to the modal).
pub struct TransferHostOptions {
    pub fs: Arc<dyn Fs>,
    /// Where the pane starts when the IDE's own machine is the local side.
    pub ide_root: PathBuf,
    pub remote: Option<RemoteHostOptions>,
}

pub struct RemoteHostOptions {
    pub project: Entity<Project>,
    /// `RemoteConnectionOptions::display_name`, shown in the side selector.
    pub display_name: String,
    /// Where the pane starts on the remote host: the active terminal's working
    /// directory if it reported one, else the project's first worktree.
    pub root: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum JobStatus {
    Pending,
    Running,
    Done,
    Failed(SharedString),
}

impl JobStatus {
    fn label(&self) -> &'static str {
        match self {
            JobStatus::Pending => i18n::t!("Queued"),
            JobStatus::Running => i18n::t!("Running"),
            JobStatus::Done => i18n::t!("Done"),
            JobStatus::Failed(_) => i18n::t!("Failed"),
        }
    }

    fn is_unfinished(&self) -> bool {
        matches!(self, JobStatus::Pending | JobStatus::Running)
    }
}

struct TransferJob {
    id: usize,
    label: SharedString,
    status: JobStatus,
}

/// One queued transfer. The queue is drained a job at a time so the askpass
/// prompts of consecutive SSH processes cannot overlap.
#[derive(Clone, Debug)]
struct QueuedTransfer {
    id: usize,
    name: String,
    is_directory: bool,
    upload: bool,
    local_directory: String,
    remote_directory: String,
}

enum PromptKind {
    NewFolder(PaneSide),
    Rename {
        side: PaneSide,
        from: String,
    },
    Delete {
        side: PaneSide,
        victims: Vec<(String, EntryKind)>,
    },
    /// An SSH prompt (passphrase, password or the first host-key
    /// confirmation) relayed from the askpass helper.
    AskPass {
        responder: Option<AskPassResponder>,
    },
}

/// What 「パスワードを記憶する」 asks of the credential store once a connection
/// has been started.
enum PasswordMemory {
    Store {
        key: CredentialKey,
        user: Option<String>,
        password: String,
    },
    Forget {
        key: CredentialKey,
    },
}

enum AskPassResponder {
    /// A prompt from a process this IDE started itself.
    Encrypted(oneshot::Sender<EncryptedPassword>),
    /// A prompt relayed from the remote host, which answers over its own RPC
    /// channel and so wants the plain text (the same trade-off `AskPassRequest`
    /// already makes for git over SSH).
    Plain(oneshot::Sender<String>),
}

/// What the buttons under a prompt say. A host key question is a yes/no
/// decision, so it gets 「はい」「いいえ」 and no text field at all (第 12 便).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PromptButtons {
    OkCancel,
    YesNo,
}

struct PendingPrompt {
    kind: PromptKind,
    title: SharedString,
    /// Shown verbatim in a monospace block under the title: the host key
    /// fingerprint, which is only useful if it can be compared character by
    /// character.
    detail: Option<SharedString>,
    /// `None` for a confirmation, which only needs its two buttons.
    editor: Option<Entity<Editor>>,
    buttons: PromptButtons,
    _cancellation: Option<Task<()>>,
}

#[derive(Default)]
struct PaneState {
    entries: Vec<DirectoryEntry>,
    selection: HashSet<usize>,
    /// Anchor for shift-click ranges.
    anchor: Option<usize>,
    loading: bool,
    message: Option<SharedString>,
}

impl PaneState {
    fn selected_entries(&self) -> Vec<DirectoryEntry> {
        let mut indices: Vec<_> = self.selection.iter().copied().collect();
        indices.sort_unstable();
        indices
            .into_iter()
            .filter_map(|index| self.entries.get(index).cloned())
            .collect()
    }

    fn replace(&mut self, entries: Vec<DirectoryEntry>) {
        self.entries = entries;
        self.selection.clear();
        self.anchor = None;
        self.loading = false;
        self.message = None;
    }

    fn fail(&mut self, message: SharedString) {
        self.entries.clear();
        self.selection.clear();
        self.anchor = None;
        self.loading = false;
        self.message = Some(message);
    }
}

pub struct FileTransferModal {
    ide_host: Arc<dyn ExecHost>,
    /// `None` unless the project is connected to an SSH host.
    remote_host: Option<Arc<dyn ExecHost>>,
    local_side: LocalSide,
    store: Entity<TerminalSessionStore>,
    host: Entity<Editor>,
    port: Entity<Editor>,
    user: Entity<Editor>,
    password: Entity<Editor>,
    local_path: Entity<Editor>,
    remote_path: Entity<Editor>,
    /// Name and folder a 「接続先を保存」 writes the current form to. Only shown
    /// while the preset list is open.
    preset_name: Entity<Editor>,
    preset_folder: Entity<Editor>,
    protocol: TransferProtocol,
    /// Whether the preset list under the connection bar is open.
    presets_open: bool,
    /// Whether the password in the bar is written to the OS credential store,
    /// shared with quick connect (ADR-F29).
    remember_password: bool,
    /// Reads the quick-connect passwords, so a host the user already saved a
    /// password for does not ask for it a second time (ADR-F28 追記).
    credentials: QuickConnectCredentials,
    /// The password remembered for the target now in the form, if any.
    stored_password: Option<StoredPassword>,
    /// Which credential key it came from, so saving over it updates that entry
    /// rather than adding a second one.
    stored_password_key: Option<CredentialKey>,
    /// One shot per connection attempt: `ssh` asking for the password again
    /// means the one already offered was refused. Reset by [`Self::connect`].
    password_answered: bool,
    backend: Option<Arc<TransferBackend>>,
    connecting: bool,
    local: PaneState,
    remote: PaneState,
    active_side: PaneSide,
    jobs: Vec<TransferJob>,
    next_job_id: usize,
    prompt: Option<PendingPrompt>,
    status: Option<SharedString>,
    focus_handle: FocusHandle,
    _store_observed: Subscription,
    /// Stops the remote host's transfer commands when this screen goes away:
    /// unlike this process's own children, they do not die with their task.
    _released: Subscription,
    /// Transfers waiting for the driver in `_queue`, which owns them one at a
    /// time. Kept on the modal rather than moved into the driver so a second
    /// press of アップロード appends to the run in progress instead of
    /// replacing (and cancelling) it.
    pending_transfers: Vec<QueuedTransfer>,
    draining: bool,
    _local_work: Option<Task<()>>,
    _remote_work: Option<Task<()>>,
    _management: Option<Task<()>>,
    _queue: Option<Task<()>>,
    /// The credential lookup, which has to finish before a connection can use
    /// what it found.
    _connect: Option<Task<()>>,
    /// The same lookup, run to fill the form in rather than to connect. Its
    /// own slot so that editing the form mid-connect cannot cancel the
    /// connection.
    _credentials_work: Option<Task<()>>,
    /// Writes to `terminal_sessions.json` made from the preset list (saving a
    /// preset, moving one). Kept so a modal that goes away mid-write does not
    /// leave the file half-written.
    _preset_work: Option<Task<()>>,
}

impl FileTransferModal {
    pub fn new(
        store: Entity<TerminalSessionStore>,
        hosts: TransferHostOptions,
        target: Option<RemoteTarget>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let ide_host: Arc<dyn ExecHost> = Arc::new(IdeExecHost::new(
            hosts.fs,
            hosts.ide_root.to_string_lossy().into_owned(),
            Self::askpass_factory(window, cx),
        ));
        let remote_host = hosts.remote.map(|remote| {
            Arc::new(RemoteExecHost::new(
                remote.project.downgrade(),
                &remote.display_name,
                remote.root,
                Self::transfer_askpass(window, cx),
            )) as Arc<dyn ExecHost>
        });
        // The remote host is the default when there is one: that is where the
        // project the user is looking at actually lives (ADR-F28).
        let local_side = if remote_host.is_some() {
            LocalSide::RemoteHost
        } else {
            LocalSide::IdeHost
        };
        let local_root = remote_host.as_ref().unwrap_or(&ide_host).root_directory();

        let single_line = |placeholder: &'static str, window: &mut Window, cx: &mut App| {
            cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_placeholder_text(placeholder, window, cx);
                editor
            })
        };

        let protocol = target
            .as_ref()
            .map(|target| target.protocol)
            .unwrap_or_default();
        let host = single_line(i18n::t!("Host name / IP address"), window, cx);
        let user = single_line(i18n::t!("User name"), window, cx);
        let port = cx.new(|cx| Editor::single_line(window, cx));
        let password = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            // Optional: an SSH host that takes a key never asks for one, and
            // the field only matters when it does (第 12 便).
            editor.set_placeholder_text(i18n::t!("Password (optional)"), window, cx);
            editor.set_masked(true, cx);
            editor
        });
        let preset_name = single_line(i18n::t!("Connection name"), window, cx);
        let preset_folder = single_line(i18n::t!("Folder (e.g. Offices/Head Office)"), window, cx);
        let local_path = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text(local_root, window, cx);
            editor
        });
        let remote_path = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text("/", window, cx);
            editor
        });

        match &target {
            Some(target) => {
                host.update(cx, |editor, cx| {
                    editor.set_text(target.host.clone(), window, cx)
                });
                port.update(cx, |editor, cx| {
                    editor.set_text(target.port.to_string(), window, cx)
                });
                user.update(cx, |editor, cx| {
                    editor.set_text(target.user.clone().unwrap_or_default(), window, cx)
                });
            }
            None => port.update(cx, |editor, cx| {
                editor.set_text(protocol.default_port().to_string(), window, cx)
            }),
        }

        window.focus(&host.focus_handle(cx), cx);
        // The saved-connection row is read on every render and the file lands
        // asynchronously.
        let store_observed = cx.observe(&store, |_, _, cx| cx.notify());
        let released = cx.on_release(|this, cx| {
            if let Some(remote_host) = this.remote_host.as_ref() {
                remote_host.cancel_running(cx);
            }
        });

        let mut this = Self {
            ide_host,
            remote_host,
            local_side,
            store,
            host,
            port,
            user,
            password,
            local_path,
            remote_path,
            preset_name,
            preset_folder,
            protocol,
            presets_open: false,
            remember_password: false,
            credentials: QuickConnectCredentials::new(zed_credentials_provider::global(cx)),
            stored_password: None,
            stored_password_key: None,
            password_answered: false,
            backend: None,
            connecting: false,
            local: PaneState::default(),
            remote: PaneState::default(),
            active_side: PaneSide::Local,
            jobs: Vec::new(),
            next_job_id: 0,
            prompt: None,
            status: None,
            focus_handle: cx.focus_handle(),
            _store_observed: store_observed,
            _released: released,
            pending_transfers: Vec::new(),
            draining: false,
            _local_work: None,
            _remote_work: None,
            _management: None,
            _queue: None,
            _connect: None,
            _credentials_work: None,
            _preset_work: None,
        };
        this.refresh_local(cx);
        this.refresh_stored_password(window, cx);
        this
    }

    // ---------------------------------------------------------------------
    // Remembered passwords
    // ---------------------------------------------------------------------

    /// Looks up the password remembered for the target now in the form. Run
    /// whenever the target can have changed, so that an FTP password field is
    /// already filled in by the time the user looks at it (ADR-F28 追記).
    fn refresh_stored_password(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Ok(target) = self.form_target(cx) else {
            self.stored_password = None;
            self.stored_password_key = None;
            return;
        };
        let credentials = self.credentials.clone();
        self._credentials_work = Some(cx.spawn_in(window, async move |this, cx| {
            let stored = lookup_stored_password(&credentials, &target, cx).await;
            this.update_in(cx, |this, window, cx| {
                this.apply_stored_password(stored, true, window, cx);
            })
            .log_err();
        }));
    }

    /// Takes in what the credential store had for the current target.
    ///
    /// `adopt_remember` follows the checkbox to what was found, which is right
    /// when the target itself changed and wrong while connecting: there the
    /// box is the instruction the user just gave.
    fn apply_stored_password(
        &mut self,
        stored: Option<(CredentialKey, StoredPassword)>,
        adopt_remember: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (key, stored) = match stored {
            Some((key, stored)) => (Some(key), Some(stored)),
            None => (None, None),
        };
        self.stored_password_key = key;
        self.stored_password = stored;
        if adopt_remember {
            // A remembered password is offered to the target it was saved for,
            // so the box starts ticked: unticking it and connecting is how the
            // user says "stop keeping this".
            self.remember_password = self.stored_password.is_some();
        }
        cx.notify();
        // Only FTP carries its password in the form; SSH never shows the
        // secret, because it goes straight to the askpass socket. An edited
        // field is left alone: the user is correcting a password that failed.
        if !self.protocol.uses_password() || !self.password.read(cx).text(cx).is_empty() {
            return;
        }
        let Some(stored) = self.stored_password.clone() else {
            return;
        };
        match stored.decrypt(IKnowWhatIAmDoingAndIHaveReadTheDocs) {
            Ok(mut plain) => {
                self.password.update(cx, |editor, cx| {
                    editor.set_text(plain.as_str(), window, cx);
                });
                plain.zeroize();
                cx.notify();
            }
            Err(error) => log::warn!("could not decrypt the saved password: {error:#}"),
        }
    }

    /// What the password field holds. Not trimmed: spaces are part of a
    /// password.
    fn typed_password(&self, cx: &App) -> Option<String> {
        let password = self.password.read(cx).text(cx);
        (!password.is_empty()).then_some(password)
    }

    /// Whether the password in hand — typed into the bar, or remembered — may
    /// answer `prompt`, consuming the one attempt it gets. Prompts arrive one
    /// at a time on the foreground, so the flag needs no synchronization.
    fn take_password_use(&mut self, prompt: &str, cx: &App) -> StoredPasswordUse {
        if self.typed_password(cx).is_none() && self.stored_password.is_none() {
            return StoredPasswordUse::AskUser;
        }
        let decision = decide_stored_password_use(prompt, self.password_answered);
        if decision == StoredPasswordUse::Answer {
            self.password_answered = true;
        }
        decision
    }

    /// Hands the password in hand to whoever asked, the typed one first: it is
    /// the correction the user just made to a remembered password that failed.
    /// Gives the responder back when no secret can be produced, so the caller
    /// can prompt instead.
    fn send_password(&self, responder: AskPassResponder, cx: &App) -> Result<(), AskPassResponder> {
        let Some(mut typed) = self.typed_password(cx) else {
            return self.send_stored_password(responder);
        };
        let sent = match responder {
            AskPassResponder::Encrypted(sender) => {
                match EncryptedPassword::try_from(typed.as_str()) {
                    Ok(password) => {
                        sender.send(password).ok();
                        Ok(())
                    }
                    Err(error) => {
                        log::warn!("could not pass on the entered password: {error:#}");
                        Err(AskPassResponder::Encrypted(sender))
                    }
                }
            }
            AskPassResponder::Plain(sender) => {
                sender.send(typed.clone()).ok();
                Ok(())
            }
        };
        typed.zeroize();
        sent
    }

    /// Hands the remembered password to whoever asked. Gives the responder back
    /// when the secret cannot be produced, so the caller can prompt instead.
    fn send_stored_password(&self, responder: AskPassResponder) -> Result<(), AskPassResponder> {
        let Some(stored) = self.stored_password.clone() else {
            return Err(responder);
        };
        match responder {
            AskPassResponder::Encrypted(sender) => {
                sender.send(stored).ok();
                Ok(())
            }
            // The remote host answers over its own RPC channel, which wants
            // the plain text (see `AskPassResponder`).
            AskPassResponder::Plain(sender) => {
                match stored.decrypt(IKnowWhatIAmDoingAndIHaveReadTheDocs) {
                    Ok(mut plain) => {
                        sender.send(std::mem::take(&mut plain)).ok();
                        Ok(())
                    }
                    Err(error) => {
                        log::warn!("could not decrypt the saved password: {error:#}");
                        Err(AskPassResponder::Plain(sender))
                    }
                }
            }
        }
    }

    /// What 「パスワードを記憶する」 asks the credential store to do for the
    /// target being connected to. `None` when there is nothing to change.
    ///
    /// The key is the one the password was found under when there is one, so a
    /// correction updates quick connect's own entry instead of leaving the old
    /// password beside a new one; otherwise the most specific key
    /// [`credential_lookups`] would look in.
    fn password_memory_intent(&self, target: &RemoteTarget, cx: &App) -> Option<PasswordMemory> {
        let key = self.stored_password_key.or_else(|| {
            credential_lookups(target.protocol, target.port)
                .into_iter()
                .next()
        })?;
        match (self.remember_password, self.typed_password(cx)) {
            (true, Some(password)) => Some(PasswordMemory::Store {
                key,
                user: target.user.clone(),
                password,
            }),
            // Ticked with an empty field keeps whatever is already stored, so
            // the port can be corrected without retyping the password.
            (true, None) => None,
            (false, _) => self
                .stored_password
                .is_some()
                .then_some(PasswordMemory::Forget { key }),
        }
    }

    /// Drops the password remembered for the target in the form, on the
    /// 「パスワードを消す」 button.
    fn forget_password(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Ok(target) = self.form_target(cx) else {
            return;
        };
        let Some(key) = self.stored_password_key else {
            return;
        };
        self.remember_password = false;
        let credentials = self.credentials.clone();
        self._credentials_work = Some(cx.spawn_in(window, async move |this, cx| {
            let result = credentials
                .forget_target(key.0, &target.host, key.1, cx)
                .await;
            this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(()) => {
                        this.stored_password = None;
                        this.stored_password_key = None;
                        this.status = Some(i18n::t!("Removed the saved password.").into());
                        if this.protocol.uses_password() {
                            this.password
                                .update(cx, |editor, cx| editor.set_text("", window, cx));
                        }
                    }
                    Err(error) => this.status = Some(format!("{error}").into()),
                }
                cx.notify();
            })
            .log_err();
        }));
    }

    // ---------------------------------------------------------------------
    // Connection
    // ---------------------------------------------------------------------

    fn form_target(&self, cx: &App) -> Result<RemoteTarget, SharedString> {
        let host = self.host.read(cx).text(cx).trim().to_string();
        validate_host(&host).map_err(SharedString::from)?;
        let user = self.user.read(cx).text(cx).trim().to_string();
        validate_user(&user).map_err(SharedString::from)?;
        let port_text = self.port.read(cx).text(cx).trim().to_string();
        let port = if port_text.is_empty() {
            self.protocol.default_port()
        } else {
            port_text
                .parse::<u16>()
                .map_err(|_| SharedString::from(i18n::t!("The port number is invalid.")))?
        };
        Ok(RemoteTarget {
            protocol: self.protocol,
            host,
            port,
            user: (!user.is_empty()).then_some(user),
        })
    }

    fn set_protocol(
        &mut self,
        protocol: TransferProtocol,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.protocol == protocol {
            return;
        }
        let previous_default = self.protocol.default_port().to_string();
        self.protocol = protocol;
        // A hand-typed port survives a protocol switch; only an untouched
        // default is rewritten.
        if self.port.read(cx).text(cx).trim() == previous_default {
            self.port.update(cx, |editor, cx| {
                editor.set_text(protocol.default_port().to_string(), window, cx);
            });
        }
        // A different protocol means a different credential key.
        self.refresh_stored_password(window, cx);
        cx.notify();
    }

    /// Answers SSH's prompts inside this modal. See the module comment for why
    /// this is not a nested workspace modal.
    fn askpass_factory(window: &Window, cx: &Context<Self>) -> AskPassFactory {
        let this = cx.weak_entity();
        let window = window.window_handle();
        Arc::new(move |cx: &mut AsyncApp| {
            let this = this.clone();
            AskPassDelegate::new_with_cancellation(
                cx,
                move |prompt, sender, cancellation, cx: &mut AsyncApp| {
                    let this = this.clone();
                    window
                        .update(cx, |_, window, cx| {
                            this.update(cx, |this, cx| {
                                this.begin_askpass_prompt(
                                    prompt,
                                    AskPassResponder::Encrypted(sender),
                                    Some(cancellation),
                                    window,
                                    cx,
                                );
                            })
                            .log_err();
                        })
                        .log_err();
                },
            )
        })
    }

    /// The same prompt, for a process running on the remote host: the host
    /// relays OpenSSH's question over the project's RPC channel and waits for
    /// the answer (ADR-F28).
    fn transfer_askpass(window: &Window, cx: &Context<Self>) -> TransferAskPass {
        let this = cx.weak_entity();
        let window = window.window_handle();
        Arc::new(move |prompt: String, cx: &mut AsyncApp| {
            let this = this.clone();
            let (sender, receiver) = oneshot::channel::<String>();
            window
                .update(cx, |_, window, cx| {
                    this.update(cx, |this, cx| {
                        this.begin_askpass_prompt(
                            prompt,
                            AskPassResponder::Plain(sender),
                            None,
                            window,
                            cx,
                        );
                    })
                    .log_err();
                })
                .log_err();
            // A dropped sender — the window is gone, or the user cancelled the
            // prompt — resolves to `None`, which declines over there.
            cx.background_spawn(async move { receiver.await.ok() })
        })
    }

    fn begin_askpass_prompt(
        &mut self,
        prompt: String,
        responder: AskPassResponder,
        cancellation: Option<oneshot::Receiver<()>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The password in hand — typed into the bar or remembered — answers the
        // first password question silently. Every other question — a host key
        // above all — still goes to the user (ADR-F28 追記).
        let responder = match self.take_password_use(&prompt, cx) {
            StoredPasswordUse::Answer => match self.send_password(responder, cx) {
                Ok(()) => return,
                Err(responder) => responder,
            },
            StoredPasswordUse::Rejected => {
                self.status = Some(
                    if self.typed_password(cx).is_some() {
                        i18n::t!("The password you entered was rejected. Enter it again.")
                    } else {
                        i18n::t!("The saved password was rejected. Enter it again.")
                    }
                    .into(),
                );
                // Emptied so the next attempt cannot spend itself on the same
                // refused secret, and so the prompt below is the only way in.
                self.password
                    .update(cx, |editor, cx| editor.set_text("", window, cx));
                responder
            }
            StoredPasswordUse::AskUser => responder,
        };

        let mode = askpass_answer_mode(&prompt);
        if mode == AskPassAnswerMode::Confirm {
            // The fingerprint is the whole content of the question, so it is
            // shown verbatim under a title that says what is being decided.
            self.prompt = Some(PendingPrompt {
                kind: PromptKind::AskPass {
                    responder: Some(responder),
                },
                title: i18n::t!("This host's key has not been seen before. Continue connecting?")
                    .into(),
                detail: Some(prompt.into()),
                editor: None,
                buttons: PromptButtons::YesNo,
                _cancellation: cancellation
                    .map(|cancellation| Self::watch_askpass_cancellation(cancellation, cx)),
            });
            window.focus(&self.focus_handle, cx);
            cx.notify();
            return;
        }

        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_masked(mode == AskPassAnswerMode::Secret, cx);
            editor
        });
        window.focus(&editor.focus_handle(cx), cx);
        self.prompt = Some(PendingPrompt {
            kind: PromptKind::AskPass {
                responder: Some(responder),
            },
            title: prompt.into(),
            detail: None,
            editor: Some(editor),
            buttons: PromptButtons::OkCancel,
            _cancellation: cancellation
                .map(|cancellation| Self::watch_askpass_cancellation(cancellation, cx)),
        });
        cx.notify();
    }

    /// Takes the prompt down when the process that asked has gone away, so a
    /// dead question cannot be answered into a closed socket.
    fn watch_askpass_cancellation(
        cancellation: oneshot::Receiver<()>,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        cx.spawn(async move |this, cx| {
            cancellation.await.ok();
            this.update(cx, |this, cx| {
                if matches!(
                    this.prompt.as_ref().map(|prompt| &prompt.kind),
                    Some(PromptKind::AskPass { .. })
                ) {
                    this.prompt = None;
                    cx.notify();
                }
            })
            .log_err();
        })
    }

    fn connect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let target = match self.form_target(cx) {
            Ok(target) => target,
            Err(message) => {
                self.status = Some(message);
                cx.notify();
                return;
            }
        };
        // Every attempt gets one go at the password in hand.
        self.password_answered = false;
        self.connecting = true;
        self.status = Some(i18n::t!("Connecting to {target}…", target = target.describe()).into());
        cx.notify();

        // Looked up here as well as on every form change, so a hand-typed host
        // is covered and not just one picked out of the saved list.
        let credentials = self.credentials.clone();
        let lookup_target = target.clone();
        self._connect = Some(cx.spawn_in(window, async move |this, cx| {
            let stored = lookup_stored_password(&credentials, &lookup_target, cx).await;
            let Ok(memory) = this.update_in(cx, |this, window, cx| {
                this.apply_stored_password(stored, false, window, cx);
                let memory = this.password_memory_intent(&lookup_target, cx);
                this.start_backend(target, cx);
                memory
            }) else {
                return;
            };
            let Some(mut memory) = memory else {
                return;
            };
            // Run after the connection has been started: remembering a
            // password must never delay (or block) the transfer itself.
            let result = match &mut memory {
                PasswordMemory::Store {
                    key,
                    user,
                    password,
                } => {
                    let stored = credentials
                        .store_target(
                            key.0,
                            &lookup_target.host,
                            key.1,
                            user.as_deref(),
                            password,
                            cx,
                        )
                        .await;
                    password.zeroize();
                    stored
                }
                PasswordMemory::Forget { key } => {
                    credentials
                        .forget_target(key.0, &lookup_target.host, key.1, cx)
                        .await
                }
            };
            this.update(cx, |this, cx| {
                match result {
                    Ok(()) => {
                        if matches!(memory, PasswordMemory::Forget { .. }) {
                            this.stored_password = None;
                            this.stored_password_key = None;
                        }
                    }
                    Err(error) => this.status = Some(format!("{error}").into()),
                }
                cx.notify();
            })
            .log_err();
        }));
    }

    fn start_backend(&mut self, target: RemoteTarget, cx: &mut Context<Self>) {
        let password = self
            .protocol
            .uses_password()
            .then(|| self.password.read(cx).text(cx))
            .filter(|password| !password.is_empty());
        self.backend = Some(Arc::new(TransferBackend::new(
            target,
            self.local_host().clone(),
            password,
        )));
        cx.notify();
        self.refresh_remote(cx);
    }

    fn disconnect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Dropping the tasks below kills this process's own children, but a
        // command running on the remote host has to be told (ADR-F28).
        self.local_host().clone().cancel_running(cx);
        self.backend = None;
        self.connecting = false;
        self._remote_work = None;
        self._management = None;
        self._queue = None;
        self.pending_transfers.clear();
        self.draining = false;
        for job in &mut self.jobs {
            if job.status.is_unfinished() {
                job.status = JobStatus::Failed(
                    i18n::t!("Stopped because the connection was closed.").into(),
                );
            }
        }
        self.remote = PaneState::default();
        self.prompt = None;
        self.password
            .update(cx, |editor, cx| editor.set_text("", window, cx));
        self.status = Some(i18n::t!("Disconnected.").into());
        cx.notify();
    }

    /// Fills the connection bar in from a preset, including the name and folder
    /// a later 「接続先を保存」 would write back to.
    fn apply_preset(
        &mut self,
        preset: &ConnectionPreset,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.preset_name.update(cx, |editor, cx| {
            editor.set_text(preset.name.to_string(), window, cx)
        });
        self.preset_folder.update(cx, |editor, cx| {
            editor.set_text(
                preset
                    .folder
                    .as_ref()
                    .map(|folder| folder.to_string())
                    .unwrap_or_default(),
                window,
                cx,
            )
        });
        // Picking a target is what the list is for, so it closes behind the
        // choice rather than pushing the two panes down until dismissed.
        self.presets_open = false;
        self.apply_target(preset.target.clone(), window, cx);
    }

    fn apply_target(&mut self, target: RemoteTarget, window: &mut Window, cx: &mut Context<Self>) {
        self.protocol = target.protocol;
        self.host.update(cx, |editor, cx| {
            editor.set_text(target.host.clone(), window, cx)
        });
        self.port.update(cx, |editor, cx| {
            editor.set_text(target.port.to_string(), window, cx)
        });
        self.user.update(cx, |editor, cx| {
            editor.set_text(target.user.clone().unwrap_or_default(), window, cx)
        });
        // The saved entry may have a remembered password; a stale one from the
        // previous target must not be offered for this one.
        self.password
            .update(cx, |editor, cx| editor.set_text("", window, cx));
        self.refresh_stored_password(window, cx);
        self.status = None;
        cx.notify();
    }

    // ---------------------------------------------------------------------
    // Presets (the 接続先 list)
    // ---------------------------------------------------------------------

    /// The Remote SSH hosts from the settings file, which the picker offers
    /// beside the quick connect entries.
    fn ssh_connections(cx: &App) -> Vec<settings::SshConnection> {
        cx.try_global::<SettingsStore>()
            .and_then(|store| store.merged_settings().remote.ssh_connections.clone())
            .unwrap_or_default()
    }

    fn preset_groups(&self, cx: &App) -> Vec<PresetGroup> {
        preset_groups(
            self.store.read(cx).connections(),
            &Self::ssh_connections(cx),
        )
    }

    /// Writes the connection bar's contents back to `terminal_sessions.json`
    /// as a quick connect entry, so the terminal and the transfer screen share
    /// one list (ADR-F28 追記).
    fn save_preset(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Writing before the first read of the file would truncate it to
        // whatever little the store happens to hold, the same trap the quick
        // connect modal guards against.
        if !self.store.read(cx).is_loaded() {
            self.status =
                Some(i18n::t!("The list of saved connections has not loaded yet.").into());
            cx.notify();
            return;
        }
        let target = match self.form_target(cx) {
            Ok(target) => target,
            Err(message) => {
                self.status = Some(message);
                cx.notify();
                return;
            }
        };
        let name = self.preset_name.read(cx).text(cx).trim().to_string();
        let name = if name.is_empty() {
            target.host.clone()
        } else {
            name
        };
        let folder = normalize_folder(&self.preset_folder.read(cx).text(cx));
        let remember = self.remember_password;
        let write = self.store.update(cx, |store, cx| {
            let mut file = store.file().clone();
            let existing = file
                .connections
                .iter()
                .position(|candidate| candidate.name == name);
            let connection = preset_connection(
                existing.and_then(|index| file.connections.get(index)),
                name.clone(),
                folder,
                &target,
                remember,
            );
            match existing {
                Some(index) => file.connections[index] = connection,
                None => file.connections.push(connection),
            }
            store.write(file, cx)
        });
        self.preset_name
            .update(cx, |editor, cx| editor.set_text(name.clone(), window, cx));
        self._preset_work = Some(cx.spawn(async move |this, cx| {
            let result = write.await;
            this.update(cx, |this, cx| {
                this.status = Some(match result {
                    Ok(()) => i18n::t!("Saved the connection \"{name}\".").into(),
                    Err(error) => i18n::t!("Could not save the connection: {error}").into(),
                });
                cx.notify();
            })
            .log_err();
        }));
    }

    /// Moves a saved connection one row up or down inside its folder. The order
    /// lives in the file, so the quick connect modal shows the same one.
    fn move_preset(&mut self, index: usize, up: bool, cx: &mut Context<Self>) {
        if !self.store.read(cx).is_loaded() {
            self.status =
                Some(i18n::t!("The list of saved connections has not loaded yet.").into());
            cx.notify();
            return;
        }
        let Some(connections) = move_connection(self.store.read(cx).connections(), index, up)
        else {
            return;
        };
        let write = self.store.update(cx, |store, cx| {
            let mut file = store.file().clone();
            file.connections = connections;
            store.write(file, cx)
        });
        self._preset_work = Some(cx.spawn(async move |this, cx| {
            if let Err(error) = write.await {
                this.update(cx, |this, cx| {
                    this.status = Some(i18n::t!("Could not save the order: {error}").into());
                    cx.notify();
                })
                .log_err();
            }
        }));
    }

    // ---------------------------------------------------------------------
    // Listing
    // ---------------------------------------------------------------------

    /// The machine the left-hand pane and the transfer programs belong to.
    fn local_host(&self) -> &Arc<dyn ExecHost> {
        match (self.local_side, self.remote_host.as_ref()) {
            (LocalSide::RemoteHost, Some(remote_host)) => remote_host,
            _ => &self.ide_host,
        }
    }

    fn local_directory(&self, cx: &App) -> String {
        self.local_path.read(cx).text(cx).trim().to_string()
    }

    fn remote_directory(&self, cx: &App) -> String {
        normalize_remote_dir(&self.remote_path.read(cx).text(cx))
    }

    /// Re-roots the left-hand pane on the other machine. The transfer engine
    /// runs there too, so a live connection is re-established from the new host
    /// rather than carried over.
    fn set_local_side(&mut self, side: LocalSide, window: &mut Window, cx: &mut Context<Self>) {
        if self.local_side == side || self.remote_host.is_none() {
            return;
        }
        self.local_host().clone().cancel_running(cx);
        self.local_side = side;
        let root = self.local_host().root_directory();
        self.local_path
            .update(cx, |editor, cx| editor.set_text(root, window, cx));
        self.refresh_local(cx);
        if self.backend.is_some() {
            self.connect(window, cx);
        }
        cx.notify();
    }

    fn refresh_local(&mut self, cx: &mut Context<Self>) {
        let directory = self.local_directory(cx);
        let host = self.local_host().clone();
        self.local.loading = true;
        cx.notify();
        self._local_work = Some(cx.spawn(async move |this, cx| {
            let entries = host.list_directory(directory, cx).await;
            this.update(cx, |this, cx| {
                match entries {
                    Ok(entries) => this.local.replace(entries),
                    Err(error) => this
                        .local
                        .fail(i18n::t!("Could not open the folder: {error:#}").into()),
                }
                cx.notify();
            })
            .log_err();
        }));
    }

    fn refresh_remote(&mut self, cx: &mut Context<Self>) {
        let Some(backend) = self.backend.clone() else {
            self.status = Some(i18n::t!("Connect first.").into());
            cx.notify();
            return;
        };
        let directory = self.remote_directory(cx);
        self.remote.loading = true;
        cx.notify();
        self._remote_work = Some(cx.spawn(async move |this, cx| {
            let entries = backend.list(&directory, cx).await;
            this.update(cx, |this, cx| {
                this.connecting = false;
                match entries {
                    Ok(entries) => {
                        this.remote.replace(entries);
                        this.status = Some(
                            i18n::t!(
                                "Connected to {target}.",
                                target = backend.target().describe()
                            )
                            .into(),
                        );
                    }
                    Err(error) => {
                        this.remote.fail(format!("{error:#}").into());
                        this.status = Some(i18n::t!("Could not list the remote folder.").into());
                    }
                }
                cx.notify();
            })
            .log_err();
        }));
    }

    /// Which pane's path field currently has focus, if any.
    fn focused_path_side(&self, window: &Window, cx: &App) -> Option<PaneSide> {
        if self
            .local_path
            .focus_handle(cx)
            .contains_focused(window, cx)
        {
            Some(PaneSide::Local)
        } else if self
            .remote_path
            .focus_handle(cx)
            .contains_focused(window, cx)
        {
            Some(PaneSide::Remote)
        } else {
            None
        }
    }

    /// The text fields Tab walks, in the order they are drawn. Fields that are
    /// not rendered right now (the 接続先 pane's two, the prompt's one) are
    /// left out, so Tab never lands somewhere the user cannot see.
    fn tab_order_fields(&self) -> Vec<Entity<Editor>> {
        let mut fields = vec![
            self.host.clone(),
            self.port.clone(),
            self.user.clone(),
            self.password.clone(),
        ];
        if self.presets_open {
            fields.push(self.preset_name.clone());
            fields.push(self.preset_folder.clone());
        }
        fields.push(self.local_path.clone());
        fields.push(self.remote_path.clone());
        if let Some(editor) = self
            .prompt
            .as_ref()
            .and_then(|prompt| prompt.editor.clone())
        {
            fields.push(editor);
        }
        fields
    }

    /// Moves focus one field along [`Self::tab_order_fields`], wrapping at both
    /// ends.
    ///
    /// Deliberately not `window.focus_next`: that walks the *window's* tab-stop
    /// map, which every `tab_index(0isize)` button in the status bar joins —
    /// the same index as ホスト. The status bar paints after the modal layer,
    /// so those buttons sorted between ホスト and ポート, and Tab out of ホスト
    /// left the modal for a dock button with no visible focus ring where Tab
    /// was no longer bound at all. The tab group on the root element separates
    /// the two numberings again, but a window-wide walk still runs off the end
    /// of the group rather than wrapping back to ホスト, so the modal cycles
    /// its own fields instead (第 13 便).
    fn focus_adjacent_field(&self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let fields = self.tab_order_fields();
        let last = match fields.len().checked_sub(1) {
            Some(last) => last,
            None => return,
        };
        let focused = fields
            .iter()
            .position(|editor| editor.focus_handle(cx).contains_focused(window, cx));
        let target = match focused {
            Some(index) if forward => (index + 1) % fields.len(),
            Some(index) => (index + last) % fields.len(),
            None if forward => 0,
            None => last,
        };
        let Some(handle) = fields.get(target).map(|editor| editor.focus_handle(cx)) else {
            return;
        };
        window.focus(&handle, cx);
    }

    fn refresh(&mut self, side: PaneSide, cx: &mut Context<Self>) {
        match side {
            PaneSide::Local => self.refresh_local(cx),
            PaneSide::Remote => self.refresh_remote(cx),
        }
    }

    fn navigate(
        &mut self,
        side: PaneSide,
        path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match side {
            PaneSide::Local => {
                self.local_path
                    .update(cx, |editor, cx| editor.set_text(path, window, cx));
                self.refresh_local(cx);
            }
            PaneSide::Remote => {
                self.remote_path
                    .update(cx, |editor, cx| editor.set_text(path, window, cx));
                self.refresh_remote(cx);
            }
        }
    }

    fn go_up(&mut self, side: PaneSide, window: &mut Window, cx: &mut Context<Self>) {
        match side {
            PaneSide::Local => {
                let directory = self.local_directory(cx);
                if let Some(parent) = self.local_host().parent(&directory) {
                    self.navigate(side, parent, window, cx);
                }
            }
            PaneSide::Remote => {
                let parent = crate::transfer_backend::remote_parent(&self.remote_directory(cx));
                self.navigate(side, parent, window, cx);
            }
        }
    }

    fn open_entry(
        &mut self,
        side: PaneSide,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self.pane(side).entries.get(index).cloned() else {
            return;
        };
        // A symlink is followed the way a directory is; the far side decides
        // whether that works.
        if entry.kind == EntryKind::File {
            return;
        }
        match side {
            PaneSide::Local => {
                let path = self
                    .local_host()
                    .join(&self.local_directory(cx), &entry.name);
                self.navigate(side, path, window, cx);
            }
            PaneSide::Remote => {
                let path = remote_join(&self.remote_directory(cx), &entry.name);
                self.navigate(side, path, window, cx);
            }
        }
    }

    fn pane(&self, side: PaneSide) -> &PaneState {
        match side {
            PaneSide::Local => &self.local,
            PaneSide::Remote => &self.remote,
        }
    }

    fn pane_mut(&mut self, side: PaneSide) -> &mut PaneState {
        match side {
            PaneSide::Local => &mut self.local,
            PaneSide::Remote => &mut self.remote,
        }
    }

    fn select(
        &mut self,
        side: PaneSide,
        index: usize,
        modifiers: Modifiers,
        cx: &mut Context<Self>,
    ) {
        self.active_side = side;
        let pane = self.pane_mut(side);
        if index >= pane.entries.len() {
            return;
        }
        match (modifiers.shift, modifiers.secondary(), pane.anchor) {
            (true, _, Some(anchor)) => {
                let (start, end) = if anchor <= index {
                    (anchor, index)
                } else {
                    (index, anchor)
                };
                pane.selection = (start..=end).collect();
            }
            (false, true, _) => {
                if !pane.selection.insert(index) {
                    pane.selection.remove(&index);
                }
                pane.anchor = Some(index);
            }
            _ => {
                pane.selection = HashSet::from([index]);
                pane.anchor = Some(index);
            }
        }
        cx.notify();
    }

    // ---------------------------------------------------------------------
    // Prompts (new folder / rename / delete confirmation)
    // ---------------------------------------------------------------------

    fn open_prompt(
        &mut self,
        kind: PromptKind,
        title: impl Into<SharedString>,
        initial_text: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let editor = initial_text.map(|text| {
            let editor = cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_text(text, window, cx);
                editor
            });
            window.focus(&editor.focus_handle(cx), cx);
            editor
        });
        self.prompt = Some(PendingPrompt {
            kind,
            title: title.into(),
            detail: None,
            editor,
            buttons: PromptButtons::OkCancel,
            _cancellation: None,
        });
        cx.notify();
    }

    fn start_new_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let side = self.active_side;
        self.open_prompt(
            PromptKind::NewFolder(side),
            i18n::t!("Name of the new folder ({side})", side = side.label()),
            Some(String::new()),
            window,
            cx,
        );
    }

    fn start_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let side = self.active_side;
        let selected = self.pane(side).selected_entries();
        if selected.len() > 1 {
            self.status = Some(i18n::t!("Rename one item at a time.").into());
            cx.notify();
            return;
        }
        let Some(entry) = selected.into_iter().next() else {
            self.status = Some(i18n::t!("Select the item to rename.").into());
            cx.notify();
            return;
        };
        self.open_prompt(
            PromptKind::Rename {
                side,
                from: entry.name.clone(),
            },
            i18n::t!("New name for \"{name}\"", name = entry.name),
            Some(entry.name),
            window,
            cx,
        );
    }

    fn start_delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let side = self.active_side;
        let selected = self.pane(side).selected_entries();
        if selected.is_empty() {
            self.status = Some(i18n::t!("Select the items to delete.").into());
            cx.notify();
            return;
        }
        let names = selected
            .iter()
            .map(|entry| entry.name.clone())
            .collect::<Vec<_>>()
            .join(i18n::tc!("list separator", ", "));
        self.open_prompt(
            PromptKind::Delete {
                side,
                victims: selected
                    .into_iter()
                    .map(|entry| (entry.name, entry.kind))
                    .collect(),
            },
            i18n::t!("Delete \"{names}\" ({side})?", side = side.label()),
            None,
            window,
            cx,
        );
    }

    fn cancel_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.prompt = None;
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    fn confirm_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.answer_prompt(None, window, cx);
    }

    /// Answers the pending prompt. `answer` overrides the text field, which is
    /// how the host key question's 「はい」「いいえ」 buttons reply without one.
    fn answer_prompt(&mut self, answer: Option<&str>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(prompt) = self.prompt.take() else {
            return;
        };
        let answer = answer.map(str::to_string).or_else(|| {
            prompt.editor.as_ref().map(|editor| {
                editor.update(cx, |editor, cx| {
                    let text = editor.text(cx);
                    editor.clear(window, cx);
                    text
                })
            })
        });
        match prompt.kind {
            PromptKind::AskPass { mut responder } => {
                let mut answer = answer.unwrap_or_default();
                match responder.take() {
                    Some(AskPassResponder::Encrypted(sender)) => {
                        match EncryptedPassword::try_from(answer.as_ref()) {
                            Ok(password) => {
                                sender.send(password).ok();
                            }
                            Err(error) => {
                                self.status =
                                    Some(i18n::t!("Could not pass on the input: {error}").into())
                            }
                        }
                    }
                    Some(AskPassResponder::Plain(sender)) => {
                        sender.send(std::mem::take(&mut answer)).ok();
                    }
                    None => {}
                }
                answer.zeroize();
            }
            PromptKind::NewFolder(side) => {
                let name = answer.unwrap_or_default().trim().to_string();
                if let Err(message) = validate_entry_name(&name) {
                    self.status = Some(message.into());
                    cx.notify();
                    return;
                }
                self.run_management(side, ManagementOperation::NewFolder { name }, cx);
            }
            PromptKind::Rename { side, from } => {
                let name = answer.unwrap_or_default().trim().to_string();
                if let Err(message) = validate_entry_name(&name) {
                    self.status = Some(message.into());
                    cx.notify();
                    return;
                }
                self.run_management(side, ManagementOperation::Rename { from, to: name }, cx);
            }
            PromptKind::Delete { side, victims } => {
                self.run_management(side, ManagementOperation::Delete { victims }, cx);
            }
        }
        // Focus goes back to the modal itself so its keys work again.
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    fn run_management(
        &mut self,
        side: PaneSide,
        operation: ManagementOperation,
        cx: &mut Context<Self>,
    ) {
        if side == PaneSide::Remote && self.backend.is_none() {
            self.status = Some(i18n::t!("Connect first.").into());
            cx.notify();
            return;
        }
        let host = self.local_host().clone();
        let backend = self.backend.clone();
        let local_directory = self.local_directory(cx);
        let remote_directory = self.remote_directory(cx);
        self.pane_mut(side).loading = true;
        self.status = None;
        cx.notify();

        let work = cx.spawn(async move |this, cx| {
            let result = match side {
                PaneSide::Local => operation.run_on_host(&host, &local_directory, cx).await,
                PaneSide::Remote => match backend {
                    Some(backend) => {
                        operation
                            .run_remotely(&backend, &remote_directory, cx)
                            .await
                    }
                    None => Ok(()),
                },
            };
            this.update(cx, |this, cx| {
                if let Err(error) = result {
                    this.pane_mut(side).loading = false;
                    this.status = Some(i18n::t!("The operation failed:\n{error:#}").into());
                }
                this.refresh(side, cx);
            })
            .log_err();
        });
        // Its own slot, so navigating a pane while a delete is running does not
        // cancel the delete halfway through.
        self._management = Some(work);
    }

    // ---------------------------------------------------------------------
    // Transfers
    // ---------------------------------------------------------------------

    fn start_transfer(&mut self, upload: bool, cx: &mut Context<Self>) {
        if self.backend.is_none() {
            self.status = Some(i18n::t!("Connect first.").into());
            cx.notify();
            return;
        }
        let side = if upload {
            PaneSide::Local
        } else {
            PaneSide::Remote
        };
        let selected = self.pane(side).selected_entries();
        if selected.is_empty() {
            self.status = Some(
                if upload {
                    i18n::t!("Select the items to upload on the local side.")
                } else {
                    i18n::t!("Select the items to download on the remote side.")
                }
                .into(),
            );
            cx.notify();
            return;
        }
        let local_directory = self.local_directory(cx);
        let remote_directory = self.remote_directory(cx);
        for entry in selected {
            let id = self.next_job_id;
            self.next_job_id += 1;
            self.jobs.push(TransferJob {
                id,
                label: format!(
                    "{}: {}",
                    if upload {
                        i18n::t!("Upload")
                    } else {
                        i18n::t!("Download")
                    },
                    entry.name
                )
                .into(),
                status: JobStatus::Pending,
            });
            self.pending_transfers.push(QueuedTransfer {
                id,
                name: entry.name,
                is_directory: entry.kind.is_directory(),
                upload,
                local_directory: local_directory.clone(),
                remote_directory: remote_directory.clone(),
            });
        }
        self.status = None;
        cx.notify();
        self.drain_queue(cx);
    }

    /// Runs the queued transfers one at a time. ADR-F28 allows two at once,
    /// but consecutive SSH processes would then race for the askpass prompt
    /// and the user would have no way to tell which one is asking.
    fn drain_queue(&mut self, cx: &mut Context<Self>) {
        let Some(backend) = self.backend.clone() else {
            return;
        };
        if self.draining {
            return;
        }
        self.draining = true;
        self._queue = Some(cx.spawn(async move |this, cx| {
            loop {
                let Ok(Some(job)) = this.update(cx, |this, _| {
                    (!this.pending_transfers.is_empty()).then(|| this.pending_transfers.remove(0))
                }) else {
                    break;
                };
                if this
                    .update(cx, |this, cx| {
                        Self::set_job_status(&mut this.jobs, job.id, JobStatus::Running);
                        cx.notify();
                    })
                    .is_err()
                {
                    return;
                }
                let result = if job.upload {
                    backend
                        .upload(
                            &job.local_directory,
                            &job.name,
                            job.is_directory,
                            &job.remote_directory,
                            cx,
                        )
                        .await
                } else {
                    backend
                        .download(
                            &job.remote_directory,
                            &job.name,
                            job.is_directory,
                            &job.local_directory,
                            cx,
                        )
                        .await
                };
                let status = match result {
                    Ok(()) => JobStatus::Done,
                    Err(error) => JobStatus::Failed(format!("{error:#}").into()),
                };
                if this
                    .update(cx, |this, cx| {
                        Self::set_job_status(&mut this.jobs, job.id, status);
                        cx.notify();
                    })
                    .is_err()
                {
                    return;
                }
            }
            this.update(cx, |this, cx| {
                this.draining = false;
                this.refresh_local(cx);
                this.refresh_remote(cx);
            })
            .log_err();
        }));
    }

    fn set_job_status(jobs: &mut [TransferJob], id: usize, status: JobStatus) {
        if let Some(job) = jobs.iter_mut().find(|job| job.id == id) {
            job.status = status;
        }
    }

    fn clear_finished_jobs(&mut self, cx: &mut Context<Self>) {
        self.jobs.retain(|job| job.status.is_unfinished());
        cx.notify();
    }
}

/// The quick-connect password remembered for `target`, trying each key
/// [`credential_lookups`] offers in turn, together with the key it came from.
/// The user name has to match, so a password saved for `ubuntu` is never
/// offered when connecting as `root`.
async fn lookup_stored_password(
    credentials: &QuickConnectCredentials,
    target: &RemoteTarget,
    cx: &AsyncApp,
) -> Option<(CredentialKey, StoredPassword)> {
    let user = target.user.as_deref();
    for (protocol, port) in credential_lookups(target.protocol, target.port) {
        if let Some(password) = credentials
            .lookup_target(protocol, &target.host, port, user, cx)
            .await
        {
            return Some(((protocol, port), password));
        }
    }
    None
}

enum ManagementOperation {
    NewFolder { name: String },
    Rename { from: String, to: String },
    Delete { victims: Vec<(String, EntryKind)> },
}

impl ManagementOperation {
    async fn run_on_host(
        &self,
        host: &Arc<dyn ExecHost>,
        directory: &str,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        match self {
            ManagementOperation::NewFolder { name } => {
                host.make_directory(host.join(directory, name), cx).await
            }
            ManagementOperation::Rename { from, to } => {
                host.rename(host.join(directory, from), host.join(directory, to), cx)
                    .await
            }
            ManagementOperation::Delete { victims } => {
                for (name, kind) in victims {
                    host.remove(host.join(directory, name), *kind, cx).await?;
                }
                Ok(())
            }
        }
    }

    async fn run_remotely(
        &self,
        backend: &Arc<TransferBackend>,
        directory: &str,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        match self {
            ManagementOperation::NewFolder { name } => {
                backend
                    .make_directory(&remote_join(directory, name), cx)
                    .await
            }
            ManagementOperation::Rename { from, to } => {
                backend
                    .rename(
                        &remote_join(directory, from),
                        &remote_join(directory, to),
                        cx,
                    )
                    .await
            }
            ManagementOperation::Delete { victims } => {
                for (name, kind) in victims {
                    backend
                        .remove(&remote_join(directory, name), *kind, cx)
                        .await?;
                }
                Ok(())
            }
        }
    }
}

// -------------------------------------------------------------------------
// Rendering
// -------------------------------------------------------------------------

impl FileTransferModal {
    fn render_connection_bar(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let connected = self.backend.is_some();
        let protocol = self.protocol;
        let focused = |editor: &Entity<Editor>, cx: &App| -> bool {
            editor.focus_handle(cx).contains_focused(window, cx)
        };
        let (host_focused, port_focused, user_focused, password_focused) = (
            focused(&self.host, cx),
            focused(&self.port, cx),
            focused(&self.user, cx),
            focused(&self.password, cx),
        );
        v_flex()
            .gap_1()
            .child(
                h_flex()
                    .gap_1()
                    .flex_wrap()
                    .child(
                        Button::new("presets", i18n::t!("Connections"))
                            .end_icon(Icon::new(if self.presets_open {
                                IconName::ChevronUp
                            } else {
                                IconName::ChevronDown
                            }))
                            .toggle_state(self.presets_open)
                            .tooltip(Tooltip::text(i18n::t!(
                                "Opens the list of saved connections and Remote SSH hosts"
                            )))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.presets_open = !this.presets_open;
                                cx.notify();
                            })),
                    )
                    .child(Divider::vertical())
                    .children(TransferProtocol::ALL.map(|candidate| {
                        Button::new(SharedString::from(candidate.label()), candidate.label())
                            .toggle_state(protocol == candidate)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.set_protocol(candidate, window, cx);
                            }))
                    }))
                    .child(Divider::vertical())
                    .child(
                        input_frame_sized(InputFrameSize::Compact, host_focused, cx)
                            .track_focus(
                                &self
                                    .host
                                    .focus_handle(cx)
                                    .tab_index(HOST_FIELD_TAB_INDEX)
                                    .tab_stop(true),
                            )
                            .w(rems(13.))
                            .child(self.host.clone()),
                    )
                    .child(
                        Label::new(i18n::t!("Port"))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        input_frame_sized(InputFrameSize::Compact, port_focused, cx)
                            .track_focus(
                                &self
                                    .port
                                    .focus_handle(cx)
                                    .tab_index(PORT_FIELD_TAB_INDEX)
                                    .tab_stop(true),
                            )
                            .w(rems(4.))
                            .child(self.port.clone()),
                    )
                    .child(
                        input_frame_sized(InputFrameSize::Compact, user_focused, cx)
                            .track_focus(
                                &self
                                    .user
                                    .focus_handle(cx)
                                    .tab_index(USER_FIELD_TAB_INDEX)
                                    .tab_stop(true),
                            )
                            .w(rems(8.))
                            .child(self.user.clone()),
                    )
                    // Always present, whatever the protocol: SFTP and SCP hand
                    // it to OpenSSH's askpass prompt rather than to a command
                    // line, which is exactly where the user expected to type it
                    // (第 12 便).
                    .child(
                        input_frame_sized(InputFrameSize::Compact, password_focused, cx)
                            .track_focus(
                                &self
                                    .password
                                    .focus_handle(cx)
                                    .tab_index(PASSWORD_FIELD_TAB_INDEX)
                                    .tab_stop(true),
                            )
                            .w(rems(10.))
                            .child(self.password.clone()),
                    )
                    .child(
                        Button::new(
                            "connect",
                            if connected {
                                i18n::t!("Reconnect")
                            } else {
                                i18n::t!("Connect")
                            },
                        )
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.connect(window, cx);
                        })),
                    )
                    .child(
                        Button::new("disconnect", i18n::t!("Disconnect"))
                            .disabled(self.backend.is_none())
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.disconnect(window, cx);
                            })),
                    ),
            )
            .child(self.render_password_memory(cx))
            .children(self.render_presets(window, cx))
            .into_any_element()
    }

    /// 「パスワードを記憶する」 and what is already remembered for this target.
    fn render_password_memory(&self, cx: &mut Context<Self>) -> AnyElement {
        let stored = self.stored_password.is_some();
        h_flex()
            .gap_2()
            .flex_wrap()
            .child(
                Checkbox::new("remember-password", self.remember_password.into())
                    .label(i18n::t!("Remember password"))
                    .tooltip(Tooltip::text(i18n::t!(
                        "The password is stored in the operating system's credential manager \
                             and shared with Quick Connect"
                    )))
                    .on_click(cx.listener(|this, state: &ToggleState, _, cx| {
                        let checked = *state == ToggleState::Selected;
                        if this.remember_password != checked {
                            this.remember_password = checked;
                            cx.notify();
                        }
                    })),
            )
            .when(stored, |this| {
                this.child(
                    Label::new(i18n::t!("Saved"))
                        .size(LabelSize::XSmall)
                        .color(Color::Accent),
                )
                .child(
                    IconButton::new("forget-password", IconName::LockOff)
                        .icon_size(IconSize::Small)
                        .tooltip(Tooltip::text(i18n::t!(
                            "Removes the saved password of this connection"
                        )))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.forget_password(window, cx);
                        })),
                )
            })
            .into_any_element()
    }

    /// The 接続先 list: the quick connect entries grouped by folder, the Remote
    /// SSH hosts from the settings file, and the form that writes the bar back
    /// as a quick connect entry (ADR-F28 追記).
    fn render_presets(&self, window: &Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.presets_open {
            return None;
        }
        let groups = self.preset_groups(cx);
        let connections = self.store.read(cx).connections().to_vec();
        let name_focused = self
            .preset_name
            .focus_handle(cx)
            .contains_focused(window, cx);
        let folder_focused = self
            .preset_folder
            .focus_handle(cx)
            .contains_focused(window, cx);

        let mut list = v_flex()
            .id("connection-presets")
            .max_h(rems(12.))
            .overflow_y_scroll()
            .gap_0p5();
        if groups.is_empty() {
            list = list.child(
                Label::new(i18n::t!("No saved connections yet."))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            );
        }
        let mut row_id = 0;
        for group in groups {
            if let Some(label) = group.label.clone() {
                list = list.child(Label::new(label).size(LabelSize::Small).color(Color::Muted));
            }
            for preset in group.presets {
                let moves = match preset.source {
                    PresetSource::QuickConnect { index } => (
                        can_move_connection(&connections, index, true),
                        can_move_connection(&connections, index, false),
                    ),
                    PresetSource::RemoteSsh => (false, false),
                };
                list = list.child(self.render_preset_row(preset, row_id, moves, cx));
                row_id += 1;
            }
        }

        Some(
            v_flex()
                .gap_1()
                .p_1()
                .border_1()
                .rounded_sm()
                .border_color(cx.theme().colors().border)
                .child(list)
                .child(
                    h_flex()
                        .gap_1()
                        .flex_wrap()
                        .child(
                            Label::new(i18n::t!("Save this connection"))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .child(
                            input_frame_sized(InputFrameSize::Compact, name_focused, cx)
                                .track_focus(
                                    &self
                                        .preset_name
                                        .focus_handle(cx)
                                        .tab_index(PRESET_NAME_FIELD_TAB_INDEX)
                                        .tab_stop(true),
                                )
                                .w(rems(12.))
                                .child(self.preset_name.clone()),
                        )
                        .child(
                            input_frame_sized(InputFrameSize::Compact, folder_focused, cx)
                                .track_focus(
                                    &self
                                        .preset_folder
                                        .focus_handle(cx)
                                        .tab_index(PRESET_FOLDER_FIELD_TAB_INDEX)
                                        .tab_stop(true),
                                )
                                .w(rems(14.))
                                .child(self.preset_folder.clone()),
                        )
                        .child(
                            Button::new("save-preset", i18n::t!("Save"))
                                .start_icon(Icon::new(IconName::Plus))
                                .tooltip(Tooltip::text(i18n::t!(
                                    "Writes to the list shared with Quick Connect"
                                )))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.save_preset(window, cx);
                                })),
                        ),
                )
                .into_any_element(),
        )
    }

    /// One row of the 接続先 list. `row_id` is its position in the whole list,
    /// which is what the element ids are keyed on: a Remote SSH host has no
    /// index of its own to use. `moves` says whether 上へ / 下へ would get
    /// anywhere, which depends on the folder rather than the file position.
    fn render_preset_row(
        &self,
        preset: ConnectionPreset,
        row_id: usize,
        moves: (bool, bool),
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (can_move_up, can_move_down) = moves;
        let index = match preset.source {
            PresetSource::QuickConnect { index } => Some(index),
            PresetSource::RemoteSsh => None,
        };
        let applied = preset.clone();
        h_flex()
            .w_full()
            .gap_1()
            .justify_between()
            .child(
                div().flex_1().min_w_0().child(
                    Button::new(("preset", row_id), preset.name.clone())
                        .full_width()
                        .label_size(LabelSize::Small)
                        .tooltip(Tooltip::text(preset.detail.clone()))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.apply_preset(&applied, window, cx);
                        })),
                ),
            )
            .child(
                Label::new(preset.detail)
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .truncate(),
            )
            .when_some(index, |this, index| {
                // Only the quick connect entries have an order of their own;
                // the Remote SSH hosts are the settings file's.
                this.child(
                    IconButton::new(("preset-up", row_id), IconName::ArrowUp)
                        .icon_size(IconSize::Small)
                        .disabled(!can_move_up)
                        .tooltip(Tooltip::text(i18n::t!(
                            "Moves up one place within the same folder"
                        )))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.move_preset(index, true, cx);
                        })),
                )
                .child(
                    IconButton::new(("preset-down", row_id), IconName::ArrowDown)
                        .icon_size(IconSize::Small)
                        .disabled(!can_move_down)
                        .tooltip(Tooltip::text(i18n::t!(
                            "Moves down one place within the same folder"
                        )))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.move_preset(index, false, cx);
                        })),
                )
            })
            .into_any_element()
    }

    /// 「ローカル側」 selector, shown only when the project is connected to an
    /// SSH host and there is therefore a choice to make (ADR-F28). It sits in
    /// its own row above the panes rather than inside the left pane's header,
    /// so that the two panes' path fields still line up.
    fn render_local_side_selector(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let remote_host = self.remote_host.as_ref()?;
        let choices = [
            (LocalSide::RemoteHost, remote_host.label()),
            (LocalSide::IdeHost, self.ide_host.label()),
        ];
        let mut row = h_flex().gap_1().flex_wrap().child(
            Label::new(i18n::t!("Local side"))
                .size(LabelSize::Small)
                .color(Color::Muted),
        );
        for (side, label) in choices {
            row = row.child(
                Button::new(("local-side", side as usize), label)
                    .label_size(LabelSize::Small)
                    .toggle_state(self.local_side == side)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.set_local_side(side, window, cx);
                    })),
            );
        }
        Some(row.into_any_element())
    }

    fn render_pane(&self, side: PaneSide, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let pane = self.pane(side);
        let is_active = self.active_side == side;
        let (path_editor, list_id, path_tab_index) = match side {
            PaneSide::Local => (
                self.local_path.clone(),
                "local-entries",
                LOCAL_PATH_FIELD_TAB_INDEX,
            ),
            PaneSide::Remote => (
                self.remote_path.clone(),
                "remote-entries",
                REMOTE_PATH_FIELD_TAB_INDEX,
            ),
        };
        let entry_count = pane.entries.len();
        let loading = pane.loading;
        let message = pane.message.clone();
        let path_focused = path_editor.focus_handle(cx).contains_focused(window, cx);

        v_flex()
            .flex_1()
            .min_w_0()
            .gap_1()
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Label::new(side.label())
                            .size(LabelSize::Small)
                            .color(if is_active {
                                Color::Default
                            } else {
                                Color::Muted
                            }),
                    )
                    .child(
                        input_frame_sized(InputFrameSize::Compact, path_focused, cx)
                            .track_focus(
                                &path_editor
                                    .focus_handle(cx)
                                    .tab_index(path_tab_index)
                                    .tab_stop(true),
                            )
                            .flex_1()
                            .min_w_0()
                            .child(path_editor),
                    )
                    .child(
                        IconButton::new(("go-up", side as usize), IconName::ArrowUp)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text(i18n::t!("Go to Parent Folder")))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.go_up(side, window, cx);
                            })),
                    )
                    .child(
                        IconButton::new(("reload", side as usize), IconName::RotateCw)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text(i18n::tc!("file transfer", "Refresh")))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.refresh(side, cx);
                            })),
                    ),
            )
            .child(
                v_flex()
                    .h(rems(20.))
                    .w_full()
                    .border_1()
                    .rounded_sm()
                    .border_color(if is_active {
                        cx.theme().colors().border_focused
                    } else {
                        cx.theme().colors().border
                    })
                    .child(if loading {
                        div()
                            .p_2()
                            .child(Label::new(i18n::t!("Loading…")).color(Color::Muted))
                            .into_any_element()
                    } else if let Some(message) = message {
                        div()
                            .id(("pane-error", side as usize))
                            .p_2()
                            .size_full()
                            .overflow_y_scroll()
                            .child(Label::new(message).color(Color::Error))
                            .into_any_element()
                    } else if entry_count == 0 {
                        div()
                            .p_2()
                            .child(Label::new(i18n::t!("No items")).color(Color::Muted))
                            .into_any_element()
                    } else {
                        uniform_list(
                            list_id,
                            entry_count,
                            cx.processor(move |this, range: Range<usize>, _window, cx| {
                                let rows = range
                                    .filter_map(|index| {
                                        let pane = this.pane(side);
                                        Some((
                                            index,
                                            pane.entries.get(index)?.clone(),
                                            pane.selection.contains(&index),
                                        ))
                                    })
                                    .collect::<Vec<_>>();
                                rows.into_iter()
                                    .map(|(index, entry, selected)| {
                                        this.render_entry_row(side, index, &entry, selected, cx)
                                    })
                                    .collect::<Vec<_>>()
                            }),
                        )
                        .size_full()
                        .into_any_element()
                    }),
            )
            .into_any_element()
    }

    fn render_entry_row(
        &self,
        side: PaneSide,
        index: usize,
        entry: &DirectoryEntry,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let icon = match entry.kind {
            EntryKind::Directory => IconName::Folder,
            EntryKind::Symlink => IconName::FolderShare,
            EntryKind::File => IconName::File,
        };
        let background = if selected {
            cx.theme().colors().element_selected
        } else {
            cx.theme().colors().ghost_element_background
        };
        let hover_background = cx.theme().colors().ghost_element_hover;
        h_flex()
            .id((
                match side {
                    PaneSide::Local => "local-entry",
                    PaneSide::Remote => "remote-entry",
                },
                index,
            ))
            .w_full()
            .px_1()
            .gap_1()
            .bg(background)
            .hover(move |style| style.bg(hover_background))
            .child(Icon::new(icon).size(IconSize::XSmall).color(Color::Muted))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_x_hidden()
                    .child(Label::new(entry.name.clone()).size(LabelSize::Small)),
            )
            .child(
                div().w(rems(5.)).child(
                    Label::new(format_size(entry.size))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
            )
            .child(
                div().w(rems(8.)).child(
                    Label::new(entry.modified.clone().unwrap_or_default())
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
            )
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                this.select(side, index, event.modifiers(), cx);
                if event.click_count() > 1 {
                    this.open_entry(side, index, window, cx);
                }
            }))
            .into_any_element()
    }

    fn render_operations(&self, cx: &mut Context<Self>) -> AnyElement {
        h_flex()
            .gap_1()
            .flex_wrap()
            .child(
                Label::new(i18n::t!("Target: {side}", side = self.active_side.label()))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                Button::new("new-folder", i18n::tc!("file transfer", "New Folder"))
                    .start_icon(Icon::new(IconName::FolderAdd))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.start_new_folder(window, cx);
                    })),
            )
            .child(
                Button::new("rename", i18n::tc!("file transfer", "Rename"))
                    .start_icon(Icon::new(IconName::Pencil))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.start_rename(window, cx);
                    })),
            )
            .child(
                Button::new("delete", i18n::t!("Delete"))
                    .start_icon(Icon::new(IconName::Trash))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.start_delete(window, cx);
                    })),
            )
            .child(Divider::vertical())
            .child(
                Button::new("upload", i18n::t!("Upload"))
                    .end_icon(Icon::new(IconName::ArrowRight))
                    .tooltip(Tooltip::text(i18n::t!(
                        "Sends the items selected on the local side to the remote"
                    )))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.start_transfer(true, cx);
                    })),
            )
            .child(
                Button::new("download", i18n::t!("Download"))
                    .start_icon(Icon::new(IconName::ArrowLeft))
                    .tooltip(Tooltip::text(i18n::t!(
                        "Brings the items selected on the remote side to the local side"
                    )))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.start_transfer(false, cx);
                    })),
            )
            .into_any_element()
    }

    fn render_prompt(&self, window: &Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let prompt = self.prompt.as_ref()?;
        let editor = prompt.editor.clone().map(|editor| {
            input_frame(editor.focus_handle(cx).contains_focused(window, cx), cx)
                .track_focus(
                    &editor
                        .focus_handle(cx)
                        .tab_index(PROMPT_FIELD_TAB_INDEX)
                        .tab_stop(true),
                )
                .w_full()
                .child(editor)
        });
        // The fingerprint has to be read character by character, which a
        // proportional font makes needlessly hard.
        let detail = prompt.detail.clone().map(|detail| {
            div()
                .id("prompt-detail")
                .w_full()
                .max_h(rems(7.))
                .overflow_y_scroll()
                .p_1()
                .rounded_sm()
                .bg(cx.theme().colors().editor_background)
                .font_buffer(cx)
                .text_size(TextSize::Small.rems(cx))
                .child(detail)
        });
        let buttons = match prompt.buttons {
            PromptButtons::OkCancel => h_flex()
                .gap_1()
                .child(Button::new("prompt-confirm", "OK").on_click(cx.listener(
                    |this, _, window, cx| {
                        this.confirm_prompt(window, cx);
                    },
                )))
                .child(
                    Button::new("prompt-cancel", i18n::t!("Cancel")).on_click(cx.listener(
                        |this, _, window, cx| {
                            this.cancel_prompt(window, cx);
                        },
                    )),
                ),
            PromptButtons::YesNo => h_flex()
                .gap_1()
                .child(
                    Button::new("prompt-yes", i18n::t!("Yes"))
                        .tooltip(Tooltip::text(
                            i18n::t!("Accepts this key and continues connecting (the key is recorded in known_hosts)"),
                        ))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.answer_prompt(Some(HOST_KEY_ACCEPTED), window, cx);
                        })),
                )
                .child(
                    Button::new("prompt-no", i18n::t!("No"))
                        .tooltip(Tooltip::text(i18n::t!("Stops connecting")))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.answer_prompt(Some(HOST_KEY_REFUSED), window, cx);
                        })),
                ),
        };
        Some(
            v_flex()
                .gap_1()
                .p_1()
                .border_1()
                .rounded_sm()
                .border_color(cx.theme().colors().border_focused)
                .child(Label::new(prompt.title.clone()))
                .children(detail)
                .children(editor)
                .child(buttons)
                .into_any_element(),
        )
    }

    fn render_jobs(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.jobs.is_empty() {
            return None;
        }
        let mut list = v_flex()
            .id("transfer-jobs")
            .max_h(rems(7.))
            .overflow_y_scroll()
            .gap_0p5();
        for job in &self.jobs {
            let (icon, color, detail) = match &job.status {
                JobStatus::Pending => (IconName::ArrowCircle, Color::Muted, None),
                JobStatus::Running => (IconName::ArrowCircle, Color::Accent, None),
                JobStatus::Done => (IconName::Check, Color::Success, None),
                JobStatus::Failed(message) => {
                    (IconName::XCircle, Color::Error, Some(message.clone()))
                }
            };
            list = list.child(
                v_flex()
                    .gap_0p5()
                    .child(
                        h_flex()
                            .gap_1()
                            .child(Icon::new(icon).size(IconSize::XSmall).color(color))
                            .child(Label::new(job.label.clone()).size(LabelSize::Small))
                            .child(
                                Label::new(job.status.label())
                                    .size(LabelSize::Small)
                                    .color(color),
                            ),
                    )
                    .when_some(detail, |this, detail| {
                        this.child(
                            div().pl_4().child(
                                Label::new(detail)
                                    .size(LabelSize::Small)
                                    .color(Color::Error),
                            ),
                        )
                    }),
            );
        }
        Some(
            v_flex()
                .gap_0p5()
                .child(
                    h_flex()
                        .gap_1()
                        .child(
                            Label::new(i18n::t!("Progress"))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .child(
                            Button::new("clear-jobs", i18n::t!("Clear Finished"))
                                .label_size(LabelSize::Small)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.clear_finished_jobs(cx);
                                })),
                        ),
                )
                .child(list)
                .into_any_element(),
        )
    }
}

impl Render for FileTransferModal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let connection_bar = self.render_connection_bar(window, cx);
        let local_side_selector = self.render_local_side_selector(cx);
        let local = self.render_pane(PaneSide::Local, window, cx);
        let remote = self.render_pane(PaneSide::Remote, window, cx);
        let operations = self.render_operations(cx);
        let prompt = self.render_prompt(window, cx);
        let jobs = self.render_jobs(cx);

        v_flex()
            .key_context("FileTransfer")
            // A tab group keeps the field indices above in a numbering of their
            // own. Without it they share the window-wide order with the status
            // bar's `tab_index(0isize)` buttons, which paint after the modal
            // layer and so sorted in between ホスト and ポート (第 13 便).
            .tab_group()
            .track_focus(&self.focus_handle)
            .elevation_3(cx)
            .w(rems(66.))
            .p_2()
            .gap_2()
            .on_action(cx.listener(|this, _: &menu::Cancel, window, cx| {
                if this.prompt.is_some() {
                    this.cancel_prompt(window, cx);
                } else {
                    cx.emit(DismissEvent);
                }
            }))
            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| {
                if let Some(prompt) = this.prompt.as_ref() {
                    // A host key is accepted by pressing 「はい」 and nothing
                    // else: Enter must not decide it by accident.
                    if prompt.buttons == PromptButtons::OkCancel {
                        this.confirm_prompt(window, cx);
                    }
                    return;
                }
                // Enter in a path field means "go there"; anywhere else it
                // means "connect" (or re-list the pane last clicked in).
                if let Some(side) = this.focused_path_side(window, cx) {
                    this.refresh(side, cx);
                } else if this.backend.is_none() {
                    this.connect(window, cx);
                } else {
                    let side = this.active_side;
                    this.refresh(side, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &FocusNextField, window, cx| {
                this.focus_adjacent_field(true, window, cx);
                cx.stop_propagation();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &FocusPreviousField, window, cx| {
                this.focus_adjacent_field(false, window, cx);
                cx.stop_propagation();
                cx.notify();
            }))
            .child(
                h_flex()
                    .gap_1()
                    .child(Icon::new(IconName::Server).size(IconSize::Small))
                    .child(Headline::new(i18n::t!("File Transfer")).size(HeadlineSize::XSmall)),
            )
            .child(connection_bar)
            .children(local_side_selector)
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .items_start()
                    .child(local)
                    .child(remote),
            )
            .child(operations)
            .children(prompt)
            .when_some(self.status.clone(), |this, status| {
                this.child(
                    Label::new(status)
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
            .children(jobs)
    }
}

impl EventEmitter<DismissEvent> for FileTransferModal {}

impl Focusable for FileTransferModal {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ModalView for FileTransferModal {
    /// The modal owns live `sftp` / `curl` children and an askpass session;
    /// keeping a hidden copy around for `ReopenLastPicker` would keep those
    /// alive long after the user closed the screen.
    fn stash_when_dismissed(&self) -> bool {
        false
    }
}

/// Opens (or re-opens) the screen on `workspace`, pre-filled with `target`.
///
/// For an SSH remote project both machines are offered as the "local" side and
/// the remote one wins by default, since that is where the project the user is
/// looking at lives (ADR-F28). The IDE's own root then falls back to the home
/// directory: the visible worktree is a path on the remote host and means
/// nothing here.
pub fn open_file_transfer(
    workspace: &mut Workspace,
    store: Entity<TerminalSessionStore>,
    target: Option<RemoteTarget>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let fs = workspace.app_state().fs.clone();
    let project = workspace.project().clone();
    let first_worktree_root = project
        .read(cx)
        .visible_worktrees(cx)
        .next()
        .map(|worktree| worktree.read(cx).abs_path().to_path_buf());
    let connection_options = project
        .read(cx)
        .is_via_remote_server()
        .then(|| project.read(cx).remote_connection_options(cx))
        .flatten();
    let remote = connection_options.map(|options| RemoteHostOptions {
        project: project.clone(),
        display_name: options.display_name(),
        root: crate::active_remote_working_directory(workspace, window, cx)
            .or_else(|| first_worktree_root.clone())
            .map(|root| root.to_string_lossy().into_owned())
            .unwrap_or_else(|| "/".to_string()),
    });
    let ide_root = match &remote {
        Some(_) => util::paths::home_dir().clone(),
        None => first_worktree_root.unwrap_or_else(|| util::paths::home_dir().clone()),
    };
    let hosts = TransferHostOptions {
        fs,
        ide_root,
        remote,
    };
    workspace.toggle_modal(window, cx, |window, cx| {
        FileTransferModal::new(store, hosts, target, window, cx)
    });
}

#[cfg(test)]
impl FileTransferModal {
    /// The picker's rows as `フォルダー/名前`, in the order they are drawn.
    pub(crate) fn preset_rows_for_test(&self, cx: &App) -> Vec<String> {
        self.preset_groups(cx)
            .into_iter()
            .flat_map(|group| {
                let label = group
                    .label
                    .map(|label| label.to_string())
                    .unwrap_or_default();
                group
                    .presets
                    .into_iter()
                    .map(move |preset| format!("{label}/{}", preset.name))
            })
            .collect()
    }

    fn preset_named(&self, name: &str, cx: &App) -> Option<ConnectionPreset> {
        self.preset_groups(cx)
            .into_iter()
            .flat_map(|group| group.presets)
            .find(|preset| preset.name.as_ref() == name)
    }

    pub(crate) fn apply_preset_for_test(
        &mut self,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(preset) = self.preset_named(name, cx) else {
            return false;
        };
        self.apply_preset(&preset, window, cx);
        true
    }

    pub(crate) fn move_preset_for_test(
        &mut self,
        name: &str,
        up: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(PresetSource::QuickConnect { index }) =
            self.preset_named(name, cx).map(|preset| preset.source)
        else {
            return false;
        };
        self.move_preset(index, up, cx);
        true
    }

    pub(crate) fn save_preset_for_test(
        &mut self,
        name: &str,
        folder: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.preset_name
            .update(cx, |editor, cx| editor.set_text(name, window, cx));
        self.preset_folder
            .update(cx, |editor, cx| editor.set_text(folder, window, cx));
        self.save_preset(window, cx);
    }

    pub(crate) fn form_state(&self, cx: &App) -> (TransferProtocol, String, String, String) {
        (
            self.protocol,
            self.host.read(cx).text(cx),
            self.port.read(cx).text(cx),
            self.user.read(cx).text(cx),
        )
    }

    pub(crate) fn local_directory_for_test(&self, cx: &App) -> String {
        self.local_directory(cx)
    }

    pub(crate) fn focus_host_for_test(&self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.host.focus_handle(cx), cx);
    }

    /// Which of the tab-stop fields currently has focus, by name, so a Tab
    /// navigation test can assert on it without reaching into private fields
    /// from outside this module.
    pub(crate) fn focused_field_for_test(&self, window: &Window, cx: &App) -> Option<&'static str> {
        let is_focused = |editor: &Entity<Editor>| editor.focus_handle(cx).is_focused(window);
        if is_focused(&self.host) {
            Some("host")
        } else if is_focused(&self.port) {
            Some("port")
        } else if is_focused(&self.user) {
            Some("user")
        } else if is_focused(&self.password) {
            Some("password")
        } else if is_focused(&self.preset_name) {
            Some("preset_name")
        } else if is_focused(&self.preset_folder) {
            Some("preset_folder")
        } else if is_focused(&self.local_path) {
            Some("local_path")
        } else if is_focused(&self.remote_path) {
            Some("remote_path")
        } else {
            None
        }
    }
}
