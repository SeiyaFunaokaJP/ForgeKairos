use auto_update::DismissMessage;
use editor::Editor;
use extension_host::{ExtensionOperation, ExtensionStore};
use futures::StreamExt;
use gpui::{
    App, Context, Entity, EventEmitter, InteractiveElement as _, ParentElement as _, Render,
    SharedString, Styled, Task, WeakEntity, Window, actions,
};
use http_client::RequestHost;
use language::{
    BinaryStatus, LanguageRegistry, LanguageServerId, LanguageServerName,
    LanguageServerStatusUpdate, ServerHealth,
};
use lsp::LanguageServerSelector;
use project::{
    LanguageServerProgress, LspStoreEvent, ProgressToken, Project, ProjectEnvironmentEvent,
    git_store::{GitStoreEvent, Repository},
};
use smallvec::SmallVec;
use std::{
    cmp::Reverse,
    collections::{HashMap, HashSet},
    fmt::Write,
    sync::Arc,
    time::{Duration, Instant},
};
use ui::{ContextMenu, PopoverMenu, PopoverMenuHandle, Tooltip, prelude::*};
use util::{ResultExt as _, truncate_and_trailoff};
use workspace::{
    StatusItemView, Workspace,
    item::ItemHandle,
    notifications::{NotificationId, simple_message_notification::MessageNotification},
};

const GIT_OPERATION_DELAY: Duration = Duration::from_millis(0);
pub const DEFERRED_SCAN_MESSAGE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the ADR-F22 consent flow waits for the allowlist file watch to
/// report the hosts it just wrote before retrying the download. Same shape as
/// the ADR-F16 update flow: the write is committed either way, so a slow watch
/// costs a retry, not correctness.
const ALLOWLIST_RELOAD_POLL_INTERVAL: Duration = Duration::from_millis(100);
const ALLOWLIST_RELOAD_ATTEMPTS: usize = 50;

actions!(
    activity_indicator,
    [
        /// Displays error messages from language servers in the status bar.
        ShowErrorMessage
    ]
);

pub enum Event {
    ShowStatus {
        server_name: LanguageServerName,
        status: SharedString,
    },
}

pub struct ActivityIndicator {
    statuses: Vec<ServerStatus>,
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    context_menu_handle: PopoverMenuHandle<ContextMenu>,
    fs_jobs: Vec<fs::JobInfo>,
    deferred_scan_message: DeferredScanMessage,
    /// Hosts the ADR-F22 consent prompt has already offered, per language
    /// server. Keyed by host rather than being a plain "already asked" flag so
    /// that a download refused at a *further* hop than the one already
    /// consented to can still ask — the set of hosts is finite, so this cannot
    /// loop, and a repeat of the same denial stays silent.
    egress_consent_offered: HashMap<LanguageServerName, HashSet<String>>,
    egress_consent_task: Option<Task<()>>,
    /// Servers whose `Failed` arrived while the binary was being fetched, as
    /// opposed to one that failed to *run*. `BinaryStatus::Failed` carries no
    /// such distinction, and the two need different wording: "実行に失敗しました"
    /// on a download that never produced a binary sends the user looking at the
    /// wrong thing.
    fetch_failures: HashSet<LanguageServerName>,
}

#[derive(Default)]
enum DeferredScanMessage {
    #[default]
    Undetected,
    Pending,
    Shown {
        _dismiss_timer: Task<()>,
    },
    Dismissed,
}

#[derive(Debug)]
struct ServerStatus {
    name: LanguageServerName,
    status: LanguageServerStatusUpdate,
}

struct PendingWork<'a> {
    language_server_id: LanguageServerId,
    progress_token: &'a ProgressToken,
    progress: &'a LanguageServerProgress,
}

enum ActivityIcon {
    LoadingSpinner,
    Icon(IconName),
}

struct Content {
    icon: ActivityIcon,
    message: String,
    on_click:
        Option<Arc<dyn Fn(&mut ActivityIndicator, &mut Window, &mut Context<ActivityIndicator>)>>,
    tooltip_message: Option<String>,
}

/// Names the ADR-F22 consent notification, one per language server.
struct LspEgressConsent;

/// Of `hosts`, the ones the running allowlist still refuses, in the order the
/// denial reported them and without repeats.
///
/// A process with no policy installed at all (a test harness) is treated as
/// refusing everything, which is what the decorator does there too.
fn hosts_still_denied(hosts: &[String], cx: &App) -> Vec<String> {
    let policy = cx
        .try_global::<egress_policy::EgressAllowlistGlobal>()
        .map(|global| global.0.snapshot());
    let mut seen = HashSet::new();
    hosts
        .iter()
        .filter(|host| {
            policy.as_ref().is_none_or(|policy| {
                RequestHost::parse(host).is_none_or(|host| !policy.allows(&host))
            })
        })
        .filter(|host| seen.insert(host.to_ascii_lowercase()))
        .cloned()
        .collect()
}

/// Whether the ADR-F22 consent prompt should be raised for `hosts`, recording
/// them against `server_name` when it should.
///
/// `false` for an empty set (nothing left to ask for) and for a set whose every
/// host has already been offered for this server, which is what keeps a server
/// that retries on every buffer from producing a stream of notifications. A set
/// containing even one host not yet offered goes through: that is a *different*
/// denial — typically a redirect hop past the one already consented to — and
/// leaving it silent would strand the user with no way forward.
fn take_hosts_to_offer(
    offered: &mut HashMap<LanguageServerName, HashSet<String>>,
    server_name: &LanguageServerName,
    hosts: &[String],
) -> bool {
    if hosts.is_empty() {
        return false;
    }
    let already_offered = offered.entry(server_name.clone()).or_default();
    let anything_new = hosts.iter().any(|host| !already_offered.contains(host));
    already_offered.extend(hosts.iter().cloned());
    anything_new
}

/// The one-line "you can sidestep the download entirely" hint that goes under
/// the consent prompt (ADR-F22).
///
/// A server already on `PATH` is used as-is (`check_if_user_installed` runs
/// before any download), so installing it with the system package manager is a
/// complete answer to the denial. Only the servers whose command is
/// unambiguous get a concrete one; the rest get the general shape, because a
/// wrong command is worse than none.
fn manual_install_hint(server_name: &LanguageServerName) -> String {
    let command = match server_name.as_ref() {
        "clangd" => Some("sudo apt install clangd"),
        "gopls" => Some("go install golang.org/x/tools/gopls@latest"),
        "rust-analyzer" => Some("rustup component add rust-analyzer"),
        _ => None,
    };
    match command {
        Some(command) => format!(
            "{server_name} をこのマシン (リモート接続中はリモートホスト) にインストールして \
             PATH に通しても解決します (例: {command})。"
        ),
        None => format!(
            "{server_name} をこのマシン (リモート接続中はリモートホスト) にインストールして \
             PATH に通しても解決します。"
        ),
    }
}

impl ActivityIndicator {
    pub fn new(
        workspace: &mut Workspace,
        languages: Arc<LanguageRegistry>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<ActivityIndicator> {
        let project = workspace.project().clone();
        let workspace_handle = cx.entity().downgrade();
        let this = cx.new(|cx| {
            let mut status_events = languages.language_server_binary_statuses();
            cx.spawn(async move |this, cx| {
                while let Some((name, binary_status)) = status_events.next().await {
                    this.update(cx, |this: &mut ActivityIndicator, cx| {
                        this.record_status(
                            name,
                            LanguageServerStatusUpdate::Binary(binary_status),
                            cx,
                        );
                    })?;
                }
                anyhow::Ok(())
            })
            .detach();

            let fs = project.read(cx).fs().clone();
            let mut job_events = fs.subscribe_to_jobs();
            cx.spawn(async move |this, cx| {
                while let Some(job_event) = job_events.next().await {
                    this.update(cx, |this: &mut ActivityIndicator, cx| {
                        match job_event {
                            fs::JobEvent::Started { info } => {
                                this.fs_jobs.retain(|j| j.id != info.id);
                                this.fs_jobs.push(info);
                            }
                            fs::JobEvent::Completed { id } => {
                                this.fs_jobs.retain(|j| j.id != id);
                            }
                        }
                        cx.notify();
                    })?;
                }
                anyhow::Ok(())
            })
            .detach();

            cx.subscribe(
                &project.read(cx).lsp_store(),
                |activity_indicator, _, event, cx| {
                    if let LspStoreEvent::LanguageServerUpdate { name, message, .. } = event {
                        if let proto::update_language_server::Variant::StatusUpdate(status_update) =
                            message
                        {
                            let Some(name) = name.clone() else {
                                return;
                            };
                            let status = match &status_update.status {
                                Some(proto::status_update::Status::Binary(binary_status)) => {
                                    if let Some(binary_status) =
                                        proto::ServerBinaryStatus::from_i32(*binary_status)
                                    {
                                        let binary_status = match binary_status {
                                            proto::ServerBinaryStatus::None => BinaryStatus::None,
                                            proto::ServerBinaryStatus::CheckingForUpdate => {
                                                BinaryStatus::CheckingForUpdate
                                            }
                                            proto::ServerBinaryStatus::Downloading => {
                                                BinaryStatus::Downloading
                                            }
                                            proto::ServerBinaryStatus::Starting => {
                                                BinaryStatus::Starting
                                            }
                                            proto::ServerBinaryStatus::Stopping => {
                                                BinaryStatus::Stopping
                                            }
                                            proto::ServerBinaryStatus::Stopped => {
                                                BinaryStatus::Stopped
                                            }
                                            proto::ServerBinaryStatus::Failed => {
                                                let Some(error) = status_update.message.clone()
                                                else {
                                                    return;
                                                };
                                                // ADR-F22: a remote host's
                                                // download refused by the
                                                // allowlist arrives as FAILED
                                                // with the hosts beside it.
                                                if status_update.egress_blocked_hosts.is_empty() {
                                                    BinaryStatus::Failed { error }
                                                } else {
                                                    BinaryStatus::EgressDenied {
                                                        error,
                                                        hosts: status_update
                                                            .egress_blocked_hosts
                                                            .clone(),
                                                    }
                                                }
                                            }
                                        };
                                        LanguageServerStatusUpdate::Binary(binary_status)
                                    } else {
                                        return;
                                    }
                                }
                                Some(proto::status_update::Status::Health(health_status)) => {
                                    if let Some(health) =
                                        proto::ServerHealth::from_i32(*health_status)
                                    {
                                        let health = match health {
                                            proto::ServerHealth::Ok => ServerHealth::Ok,
                                            proto::ServerHealth::Warning => ServerHealth::Warning,
                                            proto::ServerHealth::Error => ServerHealth::Error,
                                        };
                                        LanguageServerStatusUpdate::Health(
                                            health,
                                            status_update.message.clone().map(SharedString::from),
                                        )
                                    } else {
                                        return;
                                    }
                                }
                                None => return,
                            };

                            activity_indicator.record_status(name, status, cx);
                        }
                        cx.notify()
                    }
                },
            )
            .detach();

            cx.subscribe(
                &project.read(cx).environment().clone(),
                |_, _, event, cx| match event {
                    ProjectEnvironmentEvent::ErrorsUpdated => cx.notify(),
                },
            )
            .detach();

            cx.subscribe(
                &project.read(cx).git_store().clone(),
                |_, _, event: &GitStoreEvent, cx| {
                    if let project::git_store::GitStoreEvent::JobsUpdated = event {
                        cx.notify()
                    }
                },
            )
            .detach();

            cx.subscribe(
                &project,
                |this, _, event: &project::Event, cx| match event {
                    project::Event::WorktreeAdded(_)
                    | project::Event::WorktreeRemoved(_)
                    | project::Event::WorktreeUpdatedEntries(..) => {
                        this.update_deferred_scan_status(cx);
                    }
                    _ => {}
                },
            )
            .detach();

            let mut this = Self {
                statuses: Vec::new(),
                project: project.clone(),
                workspace: workspace_handle,
                context_menu_handle: PopoverMenuHandle::default(),
                fs_jobs: Vec::new(),
                deferred_scan_message: DeferredScanMessage::default(),
                egress_consent_offered: HashMap::default(),
                egress_consent_task: None,
                fetch_failures: HashSet::default(),
            };
            this.update_deferred_scan_status(cx);
            this
        });

        cx.subscribe_in(&this, window, move |_, _, event, window, cx| match event {
            Event::ShowStatus {
                server_name,
                status,
            } => {
                let create_buffer =
                    project.update(cx, |project, cx| project.create_buffer(None, false, cx));
                let status = status.clone();
                let server_name = server_name.clone();
                cx.spawn_in(window, async move |workspace, cx| {
                    let buffer = create_buffer.await?;
                    buffer.update(cx, |buffer, cx| {
                        buffer.edit(
                            [(0..0, format!("言語サーバー {server_name}:\n\n{status}"))],
                            None,
                            cx,
                        );
                        buffer.set_capability(language::Capability::ReadOnly, cx);
                    });
                    workspace.update_in(cx, |workspace, window, cx| {
                        workspace.add_item_to_active_pane(
                            Box::new(cx.new(|cx| {
                                let mut editor = Editor::for_buffer(buffer, None, window, cx);
                                editor.set_read_only(true);
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
                .detach();
            }
        })
        .detach();
        this
    }

    /// Records the latest status for a server, whichever ingest path it came
    /// in on (the local registry stream, or a remote host's over proto), and
    /// raises the ADR-F22 consent prompt when the reason is an egress denial.
    fn record_status(
        &mut self,
        name: LanguageServerName,
        status: LanguageServerStatusUpdate,
        cx: &mut Context<Self>,
    ) {
        if let LanguageServerStatusUpdate::Binary(BinaryStatus::EgressDenied { hosts, .. }) =
            &status
        {
            let hosts = hosts.clone();
            self.prompt_egress_consent(name.clone(), hosts, cx);
        }
        let was_fetching = self.statuses.iter().any(|previous| {
            previous.name == name
                && matches!(
                    previous.status,
                    LanguageServerStatusUpdate::Binary(
                        BinaryStatus::CheckingForUpdate | BinaryStatus::Downloading
                    )
                )
        });
        match &status {
            LanguageServerStatusUpdate::Binary(BinaryStatus::Failed { .. }) if was_fetching => {
                self.fetch_failures.insert(name.clone());
            }
            _ => {
                self.fetch_failures.remove(&name);
            }
        }
        self.statuses.retain(|s| s.name != name);
        self.statuses.push(ServerStatus { name, status });
        cx.notify();
    }

    /// Offers to add the hosts a language server download needs to the network
    /// allowlist, once per server per session (ADR-F22).
    ///
    /// Silent when the hosts are already allowed — the denial is then stale,
    /// and the retry the user would be offered has already happened — and
    /// silent for a repeat of a denial already offered, so a server that
    /// retries on every buffer cannot produce a stream of notifications.
    fn prompt_egress_consent(
        &mut self,
        server_name: LanguageServerName,
        hosts: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        // A denial reported by a remote host was judged against *its* allowlist
        // (ADR-F11 pushes the client's list over, but a stale or partial push
        // is exactly the case that produces the denial). Filtering it through
        // the client's own policy would then decide "already allowed" and
        // swallow the prompt, leaving the user with a download that keeps
        // failing and no way to act on it.
        let hosts = if self.project.read(cx).is_via_remote_server() {
            hosts
        } else {
            hosts_still_denied(&hosts, cx)
        };
        if !take_hosts_to_offer(&mut self.egress_consent_offered, &server_name, &hosts) {
            return;
        }

        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let indicator = cx.entity().downgrade();
        let display_name = server_name.0.clone();
        let headline = SharedString::from(format!(
            "{display_name} のダウンロードには次のホストへの通信が必要ですが、\
             ネットワーク許可リストで拒否されました。"
        ));
        let host_labels = hosts
            .iter()
            .map(|host| SharedString::from(host.clone()))
            .collect::<Vec<_>>();
        let manual_hint = format!("許可しない場合は、{}", manual_install_hint(&server_name));
        let hosts_for_click = hosts.clone();

        workspace.update(cx, |workspace, cx| {
            workspace.show_notification(
                NotificationId::composite::<LspEgressConsent>(display_name.clone()),
                cx,
                move |cx| {
                    let headline = headline.clone();
                    let host_labels = host_labels.clone();
                    let indicator = indicator.clone();
                    let server_name = server_name.clone();
                    let hosts = hosts_for_click.clone();
                    cx.new(move |cx| {
                        MessageNotification::new_from_builder(cx, move |_, _| {
                            v_flex()
                                .gap_1()
                                .child(Label::new(headline.clone()))
                                .children(host_labels.iter().map(|host| {
                                    Label::new(format!("・{host}"))
                                        .size(LabelSize::Small)
                                        .color(Color::Muted)
                                }))
                                .into_any_element()
                        })
                        .with_title(format!(
                            "言語サーバー {display_name} を取得できませんでした"
                        ))
                        .content_icon(IconName::Warning, Color::Warning)
                        .secondary_content(manual_hint)
                        .primary_message("許可リストに追加して再試行")
                        .primary_icon(IconName::Check)
                        .primary_on_click(move |_, cx| {
                            indicator
                                .update(cx, |indicator, cx| {
                                    indicator.allow_hosts_and_retry(
                                        server_name.clone(),
                                        hosts.clone(),
                                        cx,
                                    );
                                })
                                .log_err();
                        })
                        .secondary_message("今回は許可しない")
                        .secondary_on_click(|_, _| {})
                    })
                },
            );
        });
    }

    /// Writes `hosts` into `network_allowlist.json`, waits for the watch to
    /// pick them up, and restarts `server_name` so the download runs again
    /// (ADR-F22).
    ///
    /// Owned by the indicator rather than the notification because the
    /// notification dismisses itself the moment the button is pressed, which
    /// would cancel a task it owned.
    fn allow_hosts_and_retry(
        &mut self,
        server_name: LanguageServerName,
        hosts: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        let fs = self.project.read(cx).fs().clone();
        let path = paths::network_allowlist_file().clone();
        let policy = cx
            .try_global::<egress_policy::EgressAllowlistGlobal>()
            .map(|global| global.0.clone());
        let project = self.project.clone();
        let workspace = self.workspace.clone();
        self.egress_consent_task = Some(cx.spawn(async move |_, cx| {
            if let Err(error) = egress_policy::add_allowed_hosts(&fs, &path, &hosts).await {
                log::error!(
                    "failed to add language server download hosts to the allowlist: {error:#}"
                );
                workspace
                    .update(cx, |workspace, cx| {
                        struct LspEgressConsentFailed;
                        workspace.show_notification(
                            NotificationId::unique::<LspEgressConsentFailed>(),
                            cx,
                            |cx| {
                                cx.new(|cx| {
                                    MessageNotification::new(
                                        format!("許可リストへの追加に失敗しました: {error:#}"),
                                        cx,
                                    )
                                    .content_icon(IconName::Warning, Color::Error)
                                })
                            },
                        );
                    })
                    .log_err();
                return;
            }

            // The retry has to be judged against the new list, and the only
            // thing that installs it is the file watch. Waiting on the live
            // handle is what ADR-F16's update consent does for the same
            // reason.
            if let Some(policy) = policy {
                for _ in 0..ALLOWLIST_RELOAD_ATTEMPTS {
                    let snapshot = policy.snapshot();
                    if hosts.iter().all(|host| {
                        RequestHost::parse(host).is_some_and(|host| snapshot.allows(&host))
                    }) {
                        break;
                    }
                    cx.background_executor()
                        .timer(ALLOWLIST_RELOAD_POLL_INTERVAL)
                        .await;
                }
            }

            project.update(cx, |project, cx| {
                // A remote host was handed the allowlist once, at connect
                // time (ADR-F11). Push the new one before asking it to
                // restart the server: messages on that channel are
                // ordered, so this is what makes the retry see it. The
                // watch-driven push in `RemoteClient` covers every other
                // window on the same host.
                if let Some(remote_client) = project.remote_client() {
                    remote_client.read(cx).push_egress_allowlist(cx);
                }
                let buffers = project.opened_buffers(cx);
                project.restart_language_servers_for_buffers(
                    buffers,
                    HashSet::from_iter([LanguageServerSelector::Name(server_name)]),
                    true,
                    cx,
                );
            });
        }));
    }

    fn show_error_message(&mut self, _: &ShowErrorMessage, _: &mut Window, cx: &mut Context<Self>) {
        let mut status_message_shown = false;
        self.statuses.retain(|status| match &status.status {
            LanguageServerStatusUpdate::Binary(BinaryStatus::Failed { error })
                if !status_message_shown =>
            {
                // The raw error is English and, on a machine the fork keeps off
                // the network, usually says nothing the user can act on. The
                // way out is the same one the consent prompt offers, so say so
                // here too — this dialog may be all they ever see.
                cx.emit(Event::ShowStatus {
                    server_name: status.name.clone(),
                    status: SharedString::from(format!(
                        "{error}\n\n{}",
                        manual_install_hint(&status.name)
                    )),
                });
                status_message_shown = true;
                false
            }
            LanguageServerStatusUpdate::Binary(BinaryStatus::EgressDenied { error, hosts })
                if !status_message_shown =>
            {
                // The raw error is English and says "not on the network
                // allowlist" without saying what to do about it, so it gets a
                // Japanese preamble naming the hosts (ADR-F22).
                cx.emit(Event::ShowStatus {
                    server_name: status.name.clone(),
                    status: SharedString::from(format!(
                        "ダウンロードに必要な次のホストが、ネットワーク許可リストで拒否されました:\n{}\n\n{}\n\n{error}",
                        hosts
                            .iter()
                            .map(|host| format!("・{host}"))
                            .collect::<Vec<_>>()
                            .join("\n"),
                        manual_install_hint(&status.name),
                    )),
                });
                status_message_shown = true;
                false
            }
            LanguageServerStatusUpdate::Health(
                ServerHealth::Error | ServerHealth::Warning,
                status_string,
            ) if !status_message_shown => match status_string {
                Some(error) => {
                    cx.emit(Event::ShowStatus {
                        server_name: status.name.clone(),
                        status: error.clone(),
                    });
                    status_message_shown = true;
                    false
                }
                None => false,
            },
            _ => true,
        });
    }

    fn dismiss_message(&mut self, _: &DismissMessage, _: &mut Window, cx: &mut Context<Self>) {
        self.project.update(cx, |project, cx| {
            if project.last_formatting_failure(cx).is_some() {
                project.reset_last_formatting_failure(cx);
                true
            } else {
                false
            }
        });
    }

    fn pending_language_server_work<'a>(
        &self,
        cx: &'a App,
    ) -> impl Iterator<Item = PendingWork<'a>> {
        self.project
            .read(cx)
            .language_server_statuses(cx)
            .rev()
            .flat_map(|(server_id, status)| {
                let mut pending_work = status
                    .pending_work
                    .iter()
                    .map(|(progress_token, progress)| PendingWork {
                        language_server_id: server_id,
                        progress_token,
                        progress,
                    })
                    .collect::<SmallVec<[_; 4]>>();
                pending_work.sort_by_key(|work| Reverse(work.progress.last_update_at));
                pending_work
            })
    }

    fn pending_environment_error<'a>(&'a self, cx: &'a App) -> Option<&'a String> {
        self.project.read(cx).peek_environment_error(cx)
    }

    fn update_deferred_scan_status(&mut self, cx: &mut Context<Self>) {
        let has_deferred_scan_dirs = self.project.read(cx).visible_worktrees(cx).any(|worktree| {
            let worktree = worktree.read(cx);
            worktree.deferred_scan_dir_count() > 0
                && worktree.as_local().is_none_or(|local_worktree| {
                    local_worktree.settings().file_scan_depth.is_some()
                })
        });
        match &self.deferred_scan_message {
            DeferredScanMessage::Undetected if has_deferred_scan_dirs => {
                self.deferred_scan_message = DeferredScanMessage::Pending;
                cx.notify();
            }
            DeferredScanMessage::Pending | DeferredScanMessage::Shown { .. }
                if !has_deferred_scan_dirs =>
            {
                self.deferred_scan_message = DeferredScanMessage::Undetected;
                cx.notify();
            }
            DeferredScanMessage::Dismissed if !has_deferred_scan_dirs => {
                self.deferred_scan_message = DeferredScanMessage::Undetected;
            }
            _ => {}
        }
    }

    #[cfg(feature = "test-support")]
    pub fn message_to_render(&mut self, cx: &mut Context<Self>) -> Option<String> {
        self.content_to_render(cx).map(|content| content.message)
    }

    fn content_to_render(&mut self, cx: &mut Context<Self>) -> Option<Content> {
        if let Some(content) = self.primary_content(cx) {
            return Some(content);
        }
        self.deferred_scan_content(cx)
    }

    fn primary_content(&mut self, cx: &mut Context<Self>) -> Option<Content> {
        // Show if any direnv calls failed
        if let Some(message) = self.pending_environment_error(cx) {
            return Some(Content {
                icon: ActivityIcon::Icon(IconName::Warning),
                message: message.clone(),
                on_click: Some(Arc::new(move |this, window, cx| {
                    this.project.update(cx, |project, cx| {
                        project.pop_environment_error(cx);
                    });
                    window.dispatch_action(Box::new(workspace::OpenLog), cx);
                })),
                tooltip_message: None,
            });
        }
        // Show any language server has pending activity.
        {
            let mut pending_work = self.pending_language_server_work(cx);
            if let Some(PendingWork {
                progress_token,
                progress,
                ..
            }) = pending_work.next()
            {
                let mut message = progress.title.clone().unwrap_or(progress_token.to_string());

                if let Some(percentage) = progress.percentage {
                    write!(&mut message, " ({}%)", percentage).unwrap();
                }

                if let Some(progress_message) = progress.message.as_ref() {
                    message.push_str(": ");
                    message.push_str(progress_message);
                }

                let additional_work_count = pending_work.count();
                if additional_work_count > 0 {
                    write!(&mut message, " + 他 {} 件", additional_work_count).unwrap();
                }

                return Some(Content {
                    icon: ActivityIcon::LoadingSpinner,
                    message,
                    on_click: None,
                    tooltip_message: None,
                });
            }
        }

        if let Some(session) = self
            .project
            .read(cx)
            .dap_store()
            .read(cx)
            .sessions()
            .find(|s| !s.read(cx).is_started())
        {
            return Some(Content {
                icon: ActivityIcon::LoadingSpinner,
                message: format!("デバッグ: {}", session.read(cx).adapter()),
                tooltip_message: session.read(cx).label().map(|label| label.to_string()),
                on_click: None,
            });
        }

        let current_job = self
            .project
            .read(cx)
            .active_repository(cx)
            .map(|r| r.read(cx))
            .and_then(Repository::current_job);
        // Show any long-running git command
        if let Some(job_info) = current_job
            && Instant::now() - job_info.start >= GIT_OPERATION_DELAY
        {
            return Some(Content {
                icon: ActivityIcon::LoadingSpinner,
                message: job_info.message.into(),
                on_click: None,
                tooltip_message: None,
            });
        }

        // Show any long-running fs command
        for fs_job in &self.fs_jobs {
            if Instant::now().duration_since(fs_job.start) >= GIT_OPERATION_DELAY {
                return Some(Content {
                    icon: ActivityIcon::LoadingSpinner,
                    message: fs_job.message.clone().into(),
                    on_click: None,
                    tooltip_message: None,
                });
            }
        }

        // Show any language server installation info.
        let mut downloading = SmallVec::<[_; 3]>::new();
        let mut checking_for_update = SmallVec::<[_; 3]>::new();
        let mut failed = SmallVec::<[_; 3]>::new();
        let mut failed_to_fetch = SmallVec::<[_; 3]>::new();
        let mut egress_denied = SmallVec::<[_; 3]>::new();
        let mut health_messages = SmallVec::<[_; 3]>::new();
        let mut servers_to_clear_statuses = HashSet::<LanguageServerName>::default();
        for status in &self.statuses {
            match &status.status {
                LanguageServerStatusUpdate::Binary(
                    BinaryStatus::Starting | BinaryStatus::Stopping,
                ) => {}
                LanguageServerStatusUpdate::Binary(BinaryStatus::Stopped) => {
                    servers_to_clear_statuses.insert(status.name.clone());
                }
                LanguageServerStatusUpdate::Binary(BinaryStatus::CheckingForUpdate) => {
                    checking_for_update.push(status.name.clone());
                }
                LanguageServerStatusUpdate::Binary(BinaryStatus::Downloading) => {
                    downloading.push(status.name.clone());
                }
                LanguageServerStatusUpdate::Binary(BinaryStatus::Failed { .. }) => {
                    if self.fetch_failures.contains(&status.name) {
                        failed_to_fetch.push(status.name.clone());
                    } else {
                        failed.push(status.name.clone());
                    }
                }
                LanguageServerStatusUpdate::Binary(BinaryStatus::EgressDenied { .. }) => {
                    egress_denied.push(status.name.clone());
                }
                LanguageServerStatusUpdate::Binary(BinaryStatus::None) => {}
                LanguageServerStatusUpdate::Health(health, server_status) => match server_status {
                    Some(server_status) => {
                        health_messages.push((status.name.clone(), *health, server_status.clone()));
                    }
                    None => {
                        servers_to_clear_statuses.insert(status.name.clone());
                    }
                },
            }
        }
        self.statuses
            .retain(|status| !servers_to_clear_statuses.contains(&status.name));

        health_messages.sort_by_key(|(_, health, _)| match health {
            ServerHealth::Error => 2,
            ServerHealth::Warning => 1,
            ServerHealth::Ok => 0,
        });

        if !downloading.is_empty() {
            return Some(Content {
                icon: ActivityIcon::Icon(IconName::Download),
                message: format!(
                    "{} をダウンロード中...",
                    downloading.iter().map(|name| name.as_ref()).fold(
                        String::new(),
                        |mut acc, s| {
                            if !acc.is_empty() {
                                acc.push_str(", ");
                            }
                            acc.push_str(s);
                            acc
                        }
                    )
                ),
                on_click: Some(Arc::new(move |this, window, cx| {
                    this.statuses
                        .retain(|status| !downloading.contains(&status.name));
                    this.dismiss_message(&DismissMessage, window, cx)
                })),
                tooltip_message: None,
            });
        }

        if !checking_for_update.is_empty() {
            return Some(Content {
                icon: ActivityIcon::Icon(IconName::Download),
                message: format!(
                    "{} の更新を確認中...",
                    checking_for_update.iter().map(|name| name.as_ref()).fold(
                        String::new(),
                        |mut acc, s| {
                            if !acc.is_empty() {
                                acc.push_str(", ");
                            }
                            acc.push_str(s);
                            acc
                        }
                    ),
                ),
                on_click: Some(Arc::new(move |this, window, cx| {
                    this.statuses
                        .retain(|status| !checking_for_update.contains(&status.name));
                    this.dismiss_message(&DismissMessage, window, cx)
                })),
                tooltip_message: None,
            });
        }

        // ADR-F22: named apart from a plain failure, because "why is nothing
        // happening" has a concrete answer here and the notification offering
        // to fix it may already have been dismissed.
        if !egress_denied.is_empty() {
            return Some(Content {
                icon: ActivityIcon::Icon(IconName::Warning),
                message: format!(
                    "{} のダウンロードが外部通信の統制で拒否されました。クリックすると必要なホストを表示します。",
                    egress_denied.iter().map(|name| name.as_ref()).fold(
                        String::new(),
                        |mut acc, s| {
                            if !acc.is_empty() {
                                acc.push_str(", ");
                            }
                            acc.push_str(s);
                            acc
                        }
                    ),
                ),
                on_click: Some(Arc::new(|this, window, cx| {
                    this.show_error_message(&ShowErrorMessage, window, cx)
                })),
                tooltip_message: None,
            });
        }

        // A binary that was never fetched cannot have failed to *run*, and
        // saying it did sends the user looking at the wrong thing (the server's
        // own configuration rather than the download).
        if !failed_to_fetch.is_empty() {
            return Some(Content {
                icon: ActivityIcon::Icon(IconName::Warning),
                message: format!(
                    "{} を取得できませんでした。クリックするとエラーを表示します。",
                    failed_to_fetch.iter().map(|name| name.as_ref()).fold(
                        String::new(),
                        |mut acc, s| {
                            if !acc.is_empty() {
                                acc.push_str(", ");
                            }
                            acc.push_str(s);
                            acc
                        }
                    ),
                ),
                on_click: Some(Arc::new(|this, window, cx| {
                    this.show_error_message(&ShowErrorMessage, window, cx)
                })),
                tooltip_message: None,
            });
        }

        if !failed.is_empty() {
            return Some(Content {
                icon: ActivityIcon::Icon(IconName::Warning),
                message: format!(
                    "{} の実行に失敗しました。クリックするとエラーを表示します。",
                    failed
                        .iter()
                        .map(|name| name.as_ref())
                        .fold(String::new(), |mut acc, s| {
                            if !acc.is_empty() {
                                acc.push_str(", ");
                            }
                            acc.push_str(s);
                            acc
                        }),
                ),
                on_click: Some(Arc::new(|this, window, cx| {
                    this.show_error_message(&ShowErrorMessage, window, cx)
                })),
                tooltip_message: None,
            });
        }

        // Show any formatting failure
        if let Some(failure) = self.project.read(cx).last_formatting_failure(cx) {
            return Some(Content {
                icon: ActivityIcon::Icon(IconName::Warning),
                message: format!(
                    "フォーマットに失敗しました: {failure}。クリックするとログを表示します。"
                ),
                on_click: Some(Arc::new(|indicator, window, cx| {
                    indicator.project.update(cx, |project, cx| {
                        project.reset_last_formatting_failure(cx);
                    });
                    window.dispatch_action(Box::new(workspace::OpenLog), cx);
                })),
                tooltip_message: None,
            });
        }

        // Show any health messages for the language servers
        if let Some((server_name, health, message)) = health_messages.pop() {
            let health_str = match health {
                ServerHealth::Ok => format!("({server_name}) "),
                ServerHealth::Warning => format!("({server_name}) 警告: "),
                ServerHealth::Error => format!("({server_name}) エラー: "),
            };
            let single_line_message = message
                .lines()
                .filter_map(|line| {
                    let line = line.trim();
                    if line.is_empty() { None } else { Some(line) }
                })
                .collect::<Vec<_>>()
                .join(" ");
            let mut altered_message = single_line_message != message;
            let truncated_message = truncate_and_trailoff(
                &single_line_message,
                MAX_MESSAGE_LEN.saturating_sub(health_str.len()),
            );
            altered_message |= truncated_message != single_line_message;
            let final_message = format!("{health_str}{truncated_message}");

            let tooltip_message = if altered_message {
                Some(format!("{health_str}{message}"))
            } else {
                None
            };

            return Some(Content {
                icon: ActivityIcon::Icon(IconName::Warning),
                message: final_message,
                tooltip_message,
                on_click: Some(Arc::new(move |activity_indicator, window, cx| {
                    if altered_message {
                        activity_indicator.show_error_message(&ShowErrorMessage, window, cx)
                    } else {
                        activity_indicator
                            .statuses
                            .retain(|status| status.name != server_name);
                        cx.notify();
                    }
                })),
            });
        }

        // Show any extension installation info.
        if let Some(extension_store) =
            ExtensionStore::try_global(cx).map(|extension_store| extension_store.read(cx))
            && let Some((extension_id, operation)) =
                extension_store.outstanding_operations().iter().next()
        {
            let (message, icon) = match operation {
                ExtensionOperation::Install => (
                    format!("{extension_id} 拡張機能をインストール中…"),
                    ActivityIcon::LoadingSpinner,
                ),
                ExtensionOperation::Upgrade => (
                    format!("{extension_id} 拡張機能を更新中…"),
                    ActivityIcon::Icon(IconName::Download),
                ),
                ExtensionOperation::Remove => (
                    format!("{extension_id} 拡張機能を削除中…"),
                    ActivityIcon::LoadingSpinner,
                ),
            };

            return Some(Content {
                icon,
                message,
                on_click: Some(Arc::new(|this, window, cx| {
                    this.dismiss_message(&Default::default(), window, cx)
                })),
                tooltip_message: None,
            });
        }

        None
    }

    fn deferred_scan_content(&mut self, cx: &mut Context<Self>) -> Option<Content> {
        if !matches!(
            self.deferred_scan_message,
            DeferredScanMessage::Pending | DeferredScanMessage::Shown { .. }
        ) {
            return None;
        }
        if matches!(self.deferred_scan_message, DeferredScanMessage::Pending) {
            self.deferred_scan_message = DeferredScanMessage::Shown {
                _dismiss_timer: cx.spawn(async move |this, cx| {
                    cx.background_executor()
                        .timer(DEFERRED_SCAN_MESSAGE_TIMEOUT)
                        .await;
                    this.update(cx, |this, cx| {
                        this.deferred_scan_message = DeferredScanMessage::Dismissed;
                        cx.notify();
                    })
                    .ok();
                }),
            };
        }
        Some(Content {
            icon: ActivityIcon::Icon(IconName::Info),
            // `crates/zed` のテスト (`test_partial_file_index_status_bar_message`) が
            // `message_to_render()` の戻り値と完全一致で照合している。片方だけ変えると落ちる
            message: "部分的なファイルインデックス".to_string(),
            tooltip_message: Some("git リポジトリの外にあり、`file_scan_depth` 設定より深い階層のディレクトリは、必要になった時点でインデックスされます。".to_string()),
            on_click: Some(Arc::new(|this, _, cx| {
                this.deferred_scan_message = DeferredScanMessage::Dismissed;
                cx.notify();
            })),
        })
    }
}

impl EventEmitter<Event> for ActivityIndicator {}

const MAX_MESSAGE_LEN: usize = 50;

impl Render for ActivityIndicator {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let result = h_flex()
            .id("activity-indicator")
            .on_action(cx.listener(Self::show_error_message))
            .on_action(cx.listener(Self::dismiss_message));

        let Some(content) = self.content_to_render(cx) else {
            return result;
        };

        let activity_indicator = cx.entity().downgrade();
        let truncate_content = content.message.len() > MAX_MESSAGE_LEN;
        let has_click_handler = content.on_click.is_some();

        result.child(
            PopoverMenu::new("activity-indicator-popover")
                .trigger(
                    Button::new("activity-indicator-trigger", {
                        if truncate_content {
                            truncate_and_trailoff(&content.message, MAX_MESSAGE_LEN)
                        } else {
                            content.message.clone()
                        }
                    })
                    .label_size(LabelSize::Small)
                    .tab_index(0isize)
                    .map(|this| match content.icon {
                        ActivityIcon::LoadingSpinner => this.loading(true),
                        ActivityIcon::Icon(icon_name) => this.start_icon(
                            Icon::new(icon_name)
                                .size(IconSize::Small)
                                .color(Color::Muted),
                        ),
                    })
                    .map(|button| {
                        if truncate_content {
                            button.tooltip(Tooltip::text(content.message))
                        } else {
                            button.when_some(content.tooltip_message, |this, tooltip_message| {
                                this.tooltip(Tooltip::text(tooltip_message))
                            })
                        }
                    })
                    .when_some(content.on_click, |this, handler| {
                        this.on_click(cx.listener(move |this, _, window, cx| {
                            handler(this, window, cx);
                        }))
                    }),
                )
                .anchor(gpui::Anchor::BottomLeft)
                .when(!has_click_handler, |this| {
                    this.menu(move |window, cx| {
                        let strong_this = activity_indicator.upgrade()?;
                        let mut has_cancellable_work = false;
                        let menu = ContextMenu::build(window, cx, |mut menu, _, cx| {
                            for work in strong_this.read(cx).pending_language_server_work(cx) {
                                let activity_indicator = activity_indicator.clone();
                                let mut title = work
                                    .progress
                                    .title
                                    .clone()
                                    .unwrap_or(work.progress_token.to_string());

                                if work.progress.is_cancellable {
                                    has_cancellable_work = true;
                                    let language_server_id = work.language_server_id;
                                    let token = work.progress_token.clone();
                                    let title = SharedString::from(format!("{title} をキャンセル"));
                                    menu = menu.custom_entry(
                                        move |_, _| {
                                            h_flex()
                                                .w_full()
                                                .gap_1()
                                                .child(
                                                    Icon::new(IconName::Close)
                                                        .color(Color::Muted)
                                                        .size(IconSize::Small),
                                                )
                                                .child(Label::new(title.clone()))
                                                .into_any_element()
                                        },
                                        move |_, cx| {
                                            let token = token.clone();
                                            activity_indicator
                                                .update(cx, |activity_indicator, cx| {
                                                    activity_indicator.project.update(
                                                        cx,
                                                        |project, cx| {
                                                            project.cancel_language_server_work(
                                                                language_server_id,
                                                                Some(token),
                                                                cx,
                                                            );
                                                        },
                                                    );
                                                    activity_indicator.context_menu_handle.hide(cx);
                                                    cx.notify();
                                                })
                                                .ok();
                                        },
                                    );
                                } else {
                                    if let Some(progress_message) = work.progress.message.as_ref() {
                                        title.push_str(": ");
                                        title.push_str(progress_message);
                                    }

                                    menu = menu.label(title);
                                }
                            }
                            menu
                        });
                        has_cancellable_work.then_some(menu)
                    })
                }),
        )
    }
}

impl StatusItemView for ActivityIndicator {
    fn set_active_pane_item(
        &mut self,
        _: Option<&dyn ItemHandle>,
        _window: &mut Window,
        _: &mut Context<Self>,
    ) {
    }

    fn hide_setting(&self, _: &App) -> Option<workspace::HideStatusItem> {
        // Activity indicator auto-hides when there's no work to display.
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    fn server(name: &str) -> LanguageServerName {
        LanguageServerName(SharedString::from(name.to_string()))
    }

    #[test]
    fn the_consent_prompt_is_offered_once_per_server_and_host_set() {
        let mut offered = HashMap::default();
        let clangd = server("clangd");
        let github = vec!["github.com".to_string(), "api.github.com".to_string()];

        assert!(take_hosts_to_offer(&mut offered, &clangd, &github));
        // The same denial repeating — one per open buffer, say — stays silent.
        assert!(!take_hosts_to_offer(&mut offered, &clangd, &github));
        assert!(!take_hosts_to_offer(&mut offered, &clangd, &github[..1]));
        // Another server's denial is its own prompt.
        assert!(take_hosts_to_offer(&mut offered, &server("gopls"), &github));
        // A hop past the one already consented to is a new question.
        assert!(take_hosts_to_offer(
            &mut offered,
            &clangd,
            &["objects.githubusercontent.com".to_string()]
        ));
        // Nothing left to ask for is not a prompt.
        assert!(!take_hosts_to_offer(&mut offered, &server("ruff"), &[]));
    }

    #[gpui::test]
    fn hosts_already_on_the_allowlist_are_not_asked_for_again(cx: &mut TestAppContext) {
        let denied = [
            "api.github.com".to_string(),
            "github.com".to_string(),
            // Repeats in the denial (its own host plus a predicted hop that is
            // the same name) must not become repeats in the prompt.
            "GITHUB.com".to_string(),
        ];

        // With no policy installed at all, everything counts as denied.
        cx.update(|cx| {
            assert_eq!(
                hosts_still_denied(&denied, cx),
                vec!["api.github.com".to_string(), "github.com".to_string()],
            );
        });

        let policy = http_client::EgressPolicyHandle::default();
        policy.set(http_client::EgressPolicy::from_host_strings(&["github.com".to_string()]).0);
        cx.update(|cx| {
            cx.set_global(egress_policy::EgressAllowlistGlobal(policy));
            assert_eq!(
                hosts_still_denied(&denied, cx),
                vec!["api.github.com".to_string()],
                "a host the user has already allowed is not worth a prompt"
            );
            assert!(hosts_still_denied(&["github.com".to_string()], cx).is_empty());
        });
    }

    #[test]
    fn the_manual_install_hint_names_a_command_only_when_it_is_unambiguous() {
        let clangd = manual_install_hint(&server("clangd"));
        assert!(clangd.contains("sudo apt install clangd"), "{clangd}");
        let unknown = manual_install_hint(&server("some-unknown-ls"));
        assert!(unknown.contains("some-unknown-ls"), "{unknown}");
        assert!(!unknown.contains("例:"), "{unknown}");
    }
}
