//! ADR-F28: the two-pane transfer screen (local on the left, remote on the
//! right) that `terminal::OpenFileTransfer` opens.
//!
//! Authentication prompts are rendered *inside* this modal rather than as a
//! second workspace modal: the modal layer only keeps one modal at a time, so
//! a nested askpass dialog would push this one off the screen mid-transfer.

use std::{
    collections::HashSet,
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::Result;
use askpass::{AskPassDelegate, EncryptedPassword};
use editor::Editor;
use fs::Fs;
use futures::channel::oneshot;
use gpui::{
    AnyElement, AsyncApp, ClickEvent, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    Modifiers, Render, Subscription, Task, Window, uniform_list,
};
use terminal::terminal_macro::TerminalSessionStore;
use ui::{Divider, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{ModalView, Workspace};
use zeroize::Zeroize as _;

use crate::transfer_backend::{
    AskPassFactory, DirectoryEntry, EntryKind, RemoteTarget, TransferBackend, TransferProtocol,
    format_size, list_local_directory, make_local_directory, normalize_remote_dir, remote_join,
    remote_parent, remove_local, rename_local, validate_entry_name, validate_host, validate_user,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaneSide {
    Local,
    Remote,
}

impl PaneSide {
    fn label(self) -> &'static str {
        match self {
            PaneSide::Local => "ローカル",
            PaneSide::Remote => "リモート",
        }
    }
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
            JobStatus::Pending => "待機中",
            JobStatus::Running => "実行中",
            JobStatus::Done => "完了",
            JobStatus::Failed(_) => "失敗",
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
    local_directory: PathBuf,
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
        sender: Option<oneshot::Sender<EncryptedPassword>>,
    },
}

struct PendingPrompt {
    kind: PromptKind,
    title: SharedString,
    /// `None` for a confirmation, which only needs its two buttons.
    editor: Option<Entity<Editor>>,
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
    fs: Arc<dyn Fs>,
    store: Entity<TerminalSessionStore>,
    host: Entity<Editor>,
    port: Entity<Editor>,
    user: Entity<Editor>,
    password: Entity<Editor>,
    local_path: Entity<Editor>,
    remote_path: Entity<Editor>,
    protocol: TransferProtocol,
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
}

impl FileTransferModal {
    pub fn new(
        store: Entity<TerminalSessionStore>,
        fs: Arc<dyn Fs>,
        local_root: PathBuf,
        target: Option<RemoteTarget>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
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
        let host = single_line("ホスト名 / IP アドレス", window, cx);
        let user = single_line("ユーザー名", window, cx);
        let port = cx.new(|cx| Editor::single_line(window, cx));
        let password = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("パスワード", window, cx);
            editor.set_masked(true, cx);
            editor
        });
        let local_path = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text(local_root.to_string_lossy().to_string(), window, cx);
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

        let mut this = Self {
            fs,
            store,
            host,
            port,
            user,
            password,
            local_path,
            remote_path,
            protocol,
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
            pending_transfers: Vec::new(),
            draining: false,
            _local_work: None,
            _remote_work: None,
            _management: None,
            _queue: None,
        };
        this.refresh_local(cx);
        this
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
                .map_err(|_| SharedString::from("ポート番号が不正です。"))?
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
        cx.notify();
    }

    /// Answers SSH's prompts inside this modal. See the module comment for why
    /// this is not a nested workspace modal.
    fn askpass_factory(&self, window: &Window, cx: &Context<Self>) -> AskPassFactory {
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
                                this.begin_askpass_prompt(prompt, sender, cancellation, window, cx);
                            })
                            .log_err();
                        })
                        .log_err();
                },
            )
        })
    }

    fn begin_askpass_prompt(
        &mut self,
        prompt: String,
        sender: oneshot::Sender<EncryptedPassword>,
        cancellation: oneshot::Receiver<()>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The host-key question and a user-name request are not secrets, and
        // masking them makes them impossible to answer confidently.
        let is_secret = !(prompt.contains("yes/no") || prompt.contains("Username"));
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_masked(is_secret, cx);
            editor
        });
        window.focus(&editor.focus_handle(cx), cx);
        let cancellation = cx.spawn(async move |this, cx| {
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
        });
        self.prompt = Some(PendingPrompt {
            kind: PromptKind::AskPass {
                sender: Some(sender),
            },
            title: prompt.into(),
            editor: Some(editor),
            _cancellation: Some(cancellation),
        });
        cx.notify();
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
        let password = self
            .protocol
            .uses_password()
            .then(|| self.password.read(cx).text(cx))
            .filter(|password| !password.is_empty());
        self.backend = Some(Arc::new(TransferBackend::new(
            target.clone(),
            self.fs.clone(),
            self.askpass_factory(window, cx),
            password,
        )));
        self.connecting = true;
        self.status = Some(format!("{} に接続しています…", target.describe()).into());
        cx.notify();
        self.refresh_remote(cx);
    }

    fn disconnect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.backend = None;
        self.connecting = false;
        self._remote_work = None;
        self._management = None;
        self._queue = None;
        self.pending_transfers.clear();
        self.draining = false;
        for job in &mut self.jobs {
            if job.status.is_unfinished() {
                job.status = JobStatus::Failed("切断されたため中止しました。".into());
            }
        }
        self.remote = PaneState::default();
        self.prompt = None;
        self.password
            .update(cx, |editor, cx| editor.set_text("", window, cx));
        self.status = Some("切断しました。".into());
        cx.notify();
    }

    fn apply_saved_connection(
        &mut self,
        target: RemoteTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
        self.status = None;
        cx.notify();
    }

    // ---------------------------------------------------------------------
    // Listing
    // ---------------------------------------------------------------------

    fn local_directory(&self, cx: &App) -> PathBuf {
        PathBuf::from(self.local_path.read(cx).text(cx).trim())
    }

    fn remote_directory(&self, cx: &App) -> String {
        normalize_remote_dir(&self.remote_path.read(cx).text(cx))
    }

    fn refresh_local(&mut self, cx: &mut Context<Self>) {
        let directory = self.local_directory(cx);
        let fs = self.fs.clone();
        self.local.loading = true;
        cx.notify();
        self._local_work = Some(cx.spawn(async move |this, cx| {
            let entries = list_local_directory(&fs, &directory).await;
            this.update(cx, |this, cx| {
                match entries {
                    Ok(entries) => this.local.replace(entries),
                    Err(error) => this
                        .local
                        .fail(format!("フォルダーを開けませんでした: {error:#}").into()),
                }
                cx.notify();
            })
            .log_err();
        }));
    }

    fn refresh_remote(&mut self, cx: &mut Context<Self>) {
        let Some(backend) = self.backend.clone() else {
            self.status = Some("先に接続してください。".into());
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
                            format!("{} に接続しました。", backend.target().describe()).into(),
                        );
                    }
                    Err(error) => {
                        this.remote.fail(format!("{error:#}").into());
                        this.status = Some("リモートの一覧を取得できませんでした。".into());
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
                if let Some(parent) = directory.parent().map(Path::to_path_buf) {
                    self.navigate(side, parent.to_string_lossy().to_string(), window, cx);
                }
            }
            PaneSide::Remote => {
                let parent = remote_parent(&self.remote_directory(cx));
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
                let path = self.local_directory(cx).join(&entry.name);
                self.navigate(side, path.to_string_lossy().to_string(), window, cx);
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
            editor,
            _cancellation: None,
        });
        cx.notify();
    }

    fn start_new_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let side = self.active_side;
        self.open_prompt(
            PromptKind::NewFolder(side),
            format!("{}に作成するフォルダー名", side.label()),
            Some(String::new()),
            window,
            cx,
        );
    }

    fn start_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let side = self.active_side;
        let selected = self.pane(side).selected_entries();
        if selected.len() > 1 {
            self.status = Some("名前の変更は 1 件ずつ行ってください。".into());
            cx.notify();
            return;
        }
        let Some(entry) = selected.into_iter().next() else {
            self.status = Some("名前を変更する項目を選んでください。".into());
            cx.notify();
            return;
        };
        self.open_prompt(
            PromptKind::Rename {
                side,
                from: entry.name.clone(),
            },
            format!("「{}」の新しい名前", entry.name),
            Some(entry.name),
            window,
            cx,
        );
    }

    fn start_delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let side = self.active_side;
        let selected = self.pane(side).selected_entries();
        if selected.is_empty() {
            self.status = Some("削除する項目を選んでください。".into());
            cx.notify();
            return;
        }
        let names = selected
            .iter()
            .map(|entry| entry.name.clone())
            .collect::<Vec<_>>()
            .join("、");
        self.open_prompt(
            PromptKind::Delete {
                side,
                victims: selected
                    .into_iter()
                    .map(|entry| (entry.name, entry.kind))
                    .collect(),
            },
            format!("{}の「{names}」を削除しますか?", side.label()),
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
        let Some(prompt) = self.prompt.take() else {
            return;
        };
        let answer = prompt.editor.as_ref().map(|editor| {
            editor.update(cx, |editor, cx| {
                let text = editor.text(cx);
                editor.clear(window, cx);
                text
            })
        });
        match prompt.kind {
            PromptKind::AskPass { mut sender } => {
                let mut answer = answer.unwrap_or_default();
                if let Some(sender) = sender.take() {
                    match EncryptedPassword::try_from(answer.as_ref()) {
                        Ok(password) => {
                            sender.send(password).ok();
                        }
                        Err(error) => {
                            self.status = Some(format!("入力を渡せませんでした: {error}").into())
                        }
                    }
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
            self.status = Some("先に接続してください。".into());
            cx.notify();
            return;
        }
        let fs = self.fs.clone();
        let backend = self.backend.clone();
        let local_directory = self.local_directory(cx);
        let remote_directory = self.remote_directory(cx);
        self.pane_mut(side).loading = true;
        self.status = None;
        cx.notify();

        let work = cx.spawn(async move |this, cx| {
            let result = match side {
                PaneSide::Local => operation.run_locally(&fs, &local_directory).await,
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
                    this.status = Some(format!("操作に失敗しました:\n{error:#}").into());
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
            self.status = Some("先に接続してください。".into());
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
                    "アップロードする項目をローカル側で選んでください。"
                } else {
                    "ダウンロードする項目をリモート側で選んでください。"
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
                        "アップロード"
                    } else {
                        "ダウンロード"
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

enum ManagementOperation {
    NewFolder { name: String },
    Rename { from: String, to: String },
    Delete { victims: Vec<(String, EntryKind)> },
}

impl ManagementOperation {
    async fn run_locally(&self, fs: &Arc<dyn Fs>, directory: &Path) -> Result<()> {
        match self {
            ManagementOperation::NewFolder { name } => {
                make_local_directory(fs, &directory.join(name)).await
            }
            ManagementOperation::Rename { from, to } => {
                rename_local(fs, &directory.join(from), &directory.join(to)).await
            }
            ManagementOperation::Delete { victims } => {
                for (name, kind) in victims {
                    remove_local(fs, &directory.join(name), *kind).await?;
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
    fn render_connection_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let connected = self.backend.is_some();
        let protocol = self.protocol;
        let saved = self.render_saved_connections(cx);
        v_flex()
            .gap_1()
            .child(
                h_flex()
                    .gap_1()
                    .flex_wrap()
                    .children(TransferProtocol::ALL.map(|candidate| {
                        Button::new(SharedString::from(candidate.label()), candidate.label())
                            .toggle_state(protocol == candidate)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.set_protocol(candidate, window, cx);
                            }))
                    }))
                    .child(Divider::vertical())
                    .child(div().w(rems(13.)).child(self.host.clone()))
                    .child(
                        Label::new("ポート")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(div().w(rems(4.)).child(self.port.clone()))
                    .child(div().w(rems(8.)).child(self.user.clone()))
                    .when(protocol.uses_password(), |this| {
                        this.child(div().w(rems(8.)).child(self.password.clone()))
                    })
                    .child(
                        Button::new("connect", if connected { "再接続" } else { "接続" }).on_click(
                            cx.listener(|this, _, window, cx| {
                                this.connect(window, cx);
                            }),
                        ),
                    )
                    .child(
                        Button::new("disconnect", "切断")
                            .disabled(self.backend.is_none())
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.disconnect(window, cx);
                            })),
                    ),
            )
            .children(saved)
            .into_any_element()
    }

    /// The saved quick-connect entries, so a host can be picked instead of
    /// typed. Hidden once connected, to keep the bar short.
    fn render_saved_connections(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.backend.is_some() {
            return None;
        }
        let connections = self.store.read(cx).connections().to_vec();
        if connections.is_empty() {
            return None;
        }
        let mut row = h_flex().gap_1().flex_wrap().child(
            Label::new("保存済みの接続先")
                .size(LabelSize::Small)
                .color(Color::Muted),
        );
        for (index, connection) in connections.into_iter().enumerate() {
            let target = crate::target_from_saved_connection(&connection);
            let detail = target.describe();
            row = row.child(
                Button::new(("saved", index), connection.name.clone())
                    .label_size(LabelSize::Small)
                    .tooltip(Tooltip::text(detail))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.apply_saved_connection(target.clone(), window, cx);
                    })),
            );
        }
        Some(row.into_any_element())
    }

    fn render_pane(&self, side: PaneSide, cx: &mut Context<Self>) -> AnyElement {
        let pane = self.pane(side);
        let is_active = self.active_side == side;
        let (path_editor, list_id) = match side {
            PaneSide::Local => (self.local_path.clone(), "local-entries"),
            PaneSide::Remote => (self.remote_path.clone(), "remote-entries"),
        };
        let entry_count = pane.entries.len();
        let loading = pane.loading;
        let message = pane.message.clone();

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
                    .child(div().flex_1().min_w_0().child(path_editor))
                    .child(
                        IconButton::new(("go-up", side as usize), IconName::ArrowUp)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("上のフォルダーへ"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.go_up(side, window, cx);
                            })),
                    )
                    .child(
                        IconButton::new(("reload", side as usize), IconName::RotateCw)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("更新"))
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
                            .child(Label::new("読み込み中…").color(Color::Muted))
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
                            .child(Label::new("項目がありません").color(Color::Muted))
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
                Label::new(format!("操作対象: {}", self.active_side.label()))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                Button::new("new-folder", "フォルダー作成")
                    .start_icon(Icon::new(IconName::FolderAdd))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.start_new_folder(window, cx);
                    })),
            )
            .child(
                Button::new("rename", "名前変更")
                    .start_icon(Icon::new(IconName::Pencil))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.start_rename(window, cx);
                    })),
            )
            .child(
                Button::new("delete", "削除")
                    .start_icon(Icon::new(IconName::Trash))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.start_delete(window, cx);
                    })),
            )
            .child(Divider::vertical())
            .child(
                Button::new("upload", "アップロード")
                    .end_icon(Icon::new(IconName::ArrowRight))
                    .tooltip(Tooltip::text("ローカルで選んだ項目をリモートへ送ります"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.start_transfer(true, cx);
                    })),
            )
            .child(
                Button::new("download", "ダウンロード")
                    .start_icon(Icon::new(IconName::ArrowLeft))
                    .tooltip(Tooltip::text(
                        "リモートで選んだ項目をローカルへ取り込みます",
                    ))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.start_transfer(false, cx);
                    })),
            )
            .into_any_element()
    }

    fn render_prompt(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let prompt = self.prompt.as_ref()?;
        Some(
            v_flex()
                .gap_1()
                .p_1()
                .border_1()
                .rounded_sm()
                .border_color(cx.theme().colors().border_focused)
                .child(Label::new(prompt.title.clone()))
                .children(prompt.editor.clone())
                .child(
                    h_flex()
                        .gap_1()
                        .child(Button::new("prompt-confirm", "OK").on_click(cx.listener(
                            |this, _, window, cx| {
                                this.confirm_prompt(window, cx);
                            },
                        )))
                        .child(
                            Button::new("prompt-cancel", "キャンセル").on_click(cx.listener(
                                |this, _, window, cx| {
                                    this.cancel_prompt(window, cx);
                                },
                            )),
                        ),
                )
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
                            Label::new("進行状況")
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .child(
                            Button::new("clear-jobs", "終わった行を消す")
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let connection_bar = self.render_connection_bar(cx);
        let local = self.render_pane(PaneSide::Local, cx);
        let remote = self.render_pane(PaneSide::Remote, cx);
        let operations = self.render_operations(cx);
        let prompt = self.render_prompt(cx);
        let jobs = self.render_jobs(cx);

        v_flex()
            .key_context("FileTransfer")
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
                if this.prompt.is_some() {
                    this.confirm_prompt(window, cx);
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
            .child(
                h_flex()
                    .gap_1()
                    .child(Icon::new(IconName::Server).size(IconSize::Small))
                    .child(Headline::new("ファイル転送").size(HeadlineSize::XSmall)),
            )
            .child(connection_bar)
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
pub fn open_file_transfer(
    workspace: &mut Workspace,
    store: Entity<TerminalSessionStore>,
    target: Option<RemoteTarget>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let fs = workspace.app_state().fs.clone();
    let local_root = workspace
        .project()
        .read(cx)
        .visible_worktrees(cx)
        .next()
        .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
        .unwrap_or_else(|| util::paths::home_dir().clone());
    workspace.toggle_modal(window, cx, |window, cx| {
        FileTransferModal::new(store, fs, local_root, target, window, cx)
    });
}

#[cfg(test)]
impl FileTransferModal {
    pub(crate) fn form_state(&self, cx: &App) -> (TransferProtocol, String, String, String) {
        (
            self.protocol,
            self.host.read(cx).text(cx),
            self.port.read(cx).text(cx),
            self.user.read(cx).text(cx),
        )
    }

    pub(crate) fn local_directory_for_test(&self, cx: &App) -> PathBuf {
        self.local_directory(cx)
    }
}
