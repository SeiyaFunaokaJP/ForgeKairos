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
    BinaryStatus, LanguageName, LanguageServerId, LanguageServerName, LanguageServerStatusUpdate,
    ServerHealth, language_settings::all_language_settings,
};
use lsp::LanguageServerSelector;
use project::{
    LanguageServerProgress, LspStoreEvent, ProgressToken, Project, ProjectEnvironmentEvent,
    git_store::{GitStoreEvent, Repository},
    lsp_store::dev_tools::{self, DevToolState},
    project_settings::ProjectSettings,
    symbol_index_store::{SymbolIndexStatus, SymbolIndexStoreEvent},
};
use settings::Settings as _;
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

/// How long the ADR-F39 check waits after a file is opened. Long enough for the
/// language servers that file starts to register, so that one Zed is already
/// fetching is not reported as missing; short enough to still be about the file
/// the user just opened.
const DEV_TOOLS_CHECK_DELAY: Duration = Duration::from_secs(3);

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
    /// Languages whose tools have already been looked for (ADR-F39). Opening a
    /// second file of the same language must not re-ask: the answer cannot have
    /// changed on its own, and the notification is interrupting.
    dev_tools_checked: HashSet<SharedString>,
    dev_tools_check_task: Option<Task<()>>,
    /// The install running right now, shown in the status bar because it is a
    /// download the user asked for and can take a while over a relayed link.
    dev_tools_install: Option<DevToolInstall>,
}

struct DevToolInstall {
    message: SharedString,
    _task: Task<()>,
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

/// Names the ADR-F39 notification, one per language.
struct DevToolsMissing;

/// Names the ADR-F39 failure notification, one per tool.
struct DevToolsInstallFailed;

/// Waits for the allowlist file watch to install hosts that were just written.
/// A retry judged against the old list would be refused again, and the watch is
/// the only thing that installs the new one (ADR-F22 / ADR-F16).
async fn wait_for_allowlist(
    policy: Option<http_client::EgressPolicyHandle>,
    hosts: &[String],
    cx: &mut gpui::AsyncApp,
) {
    let Some(policy) = policy else {
        return;
    };
    for _ in 0..ALLOWLIST_RELOAD_ATTEMPTS {
        let snapshot = policy.snapshot();
        if hosts
            .iter()
            .all(|host| RequestHost::parse(host).is_some_and(|host| snapshot.allows(&host)))
        {
            return;
        }
        cx.background_executor()
            .timer(ALLOWLIST_RELOAD_POLL_INTERVAL)
            .await;
    }
}

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
        Some(command) => i18n::t!(
            "Installing {server_name} on this machine (on the remote host while connected \
             remotely) and adding it to PATH also fixes this (e.g. {command})."
        ),
        None => i18n::t!(
            "Installing {server_name} on this machine (on the remote host while connected \
             remotely) and adding it to PATH also fixes this."
        ),
    }
}

impl ActivityIndicator {
    pub fn new(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<ActivityIndicator> {
        let project = workspace.project().clone();
        let workspace_handle = cx.entity().downgrade();
        let this = cx.new(|cx| {
            let fs = project.read(cx).fs().clone();
            let mut job_events = fs.subscribe_to_jobs();
            cx.spawn(async move |this, cx| {
                while let Some(job_event) = job_events.next().await {
                    this.update(cx, |this: &mut ActivityIndicator, cx| {
                        match job_event {
                            fs::JobEvent::Started { info } => {
                                this.fs_jobs.retain(|job| job.id != info.id);
                                this.fs_jobs.push(info);
                            }
                            fs::JobEvent::Updated { id, message } => {
                                if let Some(job) = this.fs_jobs.iter_mut().find(|job| job.id == id)
                                {
                                    job.message = message;
                                }
                            }
                            fs::JobEvent::Completed { id } => {
                                this.fs_jobs.retain(|job| job.id != id);
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
                                        proto::ServerBinaryStatus::try_from(*binary_status).ok()
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
                                        proto::ServerHealth::try_from(*health_status).ok()
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

            // ADR-F41: the first build of a large tree's symbol index takes a while.
            cx.subscribe(
                &project.read(cx).symbol_index_store().clone(),
                |_, _, _: &SymbolIndexStoreEvent, cx| cx.notify(),
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
                dev_tools_checked: HashSet::default(),
                dev_tools_check_task: None,
                dev_tools_install: None,
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
                            [(0..0, i18n::t!("Language server {server_name}:\n\n{status}"))],
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

    /// Records the latest status for a server, as the project's LSP store
    /// reports it (local servers and a remote host's alike), and raises the
    /// ADR-F22 consent prompt when the reason is an egress denial.
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
    /// Since ADR-F38 language server downloads run on the IDE egress lane and
    /// are not gated at all, so this is a fallback for a denial that reaches
    /// here anyway — an older remote host, or a path nobody has stamped — and
    /// no longer part of the ordinary first-run experience.
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
        let headline = SharedString::from(i18n::t!(
            "Downloading {display_name} needs the following hosts, but the network \
             allowlist denied them."
        ));
        let host_labels = hosts
            .iter()
            .map(|host| SharedString::from(host.clone()))
            .collect::<Vec<_>>();
        let manual_hint = i18n::t!(
            "If you'd rather not allow them: {}",
            manual_install_hint(&server_name)
        );
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
                                    Label::new(i18n::t!("• {host}"))
                                        .size(LabelSize::Small)
                                        .color(Color::Muted)
                                }))
                                .into_any_element()
                        })
                        .with_title(i18n::t!(
                            "Couldn't download the {display_name} language server"
                        ))
                        .content_icon(IconName::Warning, Color::Warning)
                        .secondary_content(manual_hint)
                        .primary_message(i18n::t!("Add to Allowlist and Retry"))
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
                        .secondary_message(i18n::t!("Don't Allow This Time"))
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
                                        i18n::t!(
                                            "Couldn't add the hosts to the allowlist: {error:#}"
                                        ),
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

    /// ADR-F39: looks for the tools the active buffer's language needs on the
    /// machine that owns the files, once per language.
    fn check_dev_tools(&mut self, editor: &Entity<Editor>, cx: &mut Context<Self>) {
        if !ProjectSettings::get_global(cx).dev_tools.suggest_missing {
            return;
        }

        let Some((language, worktree_id)) = editor.read(cx).active_buffer(cx).and_then(|buffer| {
            let buffer = buffer.read(cx);
            let language = buffer.language()?.name();
            let worktree_id = buffer.file()?.worktree_id(cx);
            Some((language, worktree_id))
        }) else {
            return;
        };
        if !self.dev_tools_checked.insert(language.as_ref().into()) {
            return;
        }

        self.dev_tools_check_task = Some(cx.spawn(async move |this, cx| {
            // Opening a file also starts the language servers for it, and a
            // server Zed is in the middle of downloading is not missing. Give
            // that a moment to show up before asking anyone anything.
            cx.background_executor().timer(DEV_TOOLS_CHECK_DELAY).await;
            let Ok(check) = this.update(cx, |this, cx| {
                this.project.update(cx, |project, cx| {
                    project.check_dev_tools(language.clone(), worktree_id, cx)
                })
            }) else {
                return;
            };
            match check.await {
                Ok(states) => {
                    this.update(cx, |this, cx| {
                        this.prompt_missing_dev_tools(&language, states, cx)
                    })
                    .log_err();
                }
                // Not being able to look is not worth a notification: the
                // language still works as well as it did before.
                Err(error) => log::warn!("failed to look for {language} tools: {error:#}"),
            }
        }));
    }

    fn prompt_missing_dev_tools(
        &mut self,
        language: &LanguageName,
        states: Vec<DevToolState>,
        cx: &mut Context<Self>,
    ) {
        let settings = ProjectSettings::get_global(cx).dev_tools.clone();
        // A server the project already knows about is being handled by Zed
        // itself — it is starting, downloading, or running — and a failure of
        // its own has its own notification (ADR-F22).
        let known_servers = self
            .project
            .read(cx)
            .language_server_statuses(cx)
            .map(|(_, status)| status.name.0.to_string())
            .collect::<HashSet<_>>();
        // A formatter is only worth mentioning if this language is actually
        // configured to run it: someone who pointed the setting somewhere else
        // is not missing anything.
        let configured_formatters = all_language_settings(None, cx)
            .language(None, Some(language), cx)
            .formatter
            .as_ref()
            .iter()
            .filter_map(|formatter| match formatter {
                settings::Formatter::External { command, .. } => Some(
                    std::path::Path::new(command)
                        .file_stem()
                        .map(|stem| stem.to_string_lossy().into_owned())
                        .unwrap_or_else(|| command.clone()),
                ),
                _ => None,
            })
            .collect::<HashSet<_>>();
        let missing = states
            .iter()
            .filter(|state| !state.present)
            .filter_map(|state| dev_tools::tool_by_id(&state.id))
            .filter(|tool| settings.should_suggest(tool.id))
            .filter(|tool| {
                tool.language_server_name()
                    .is_none_or(|server| !known_servers.contains(server))
            })
            .filter(|tool| {
                tool.purpose != dev_tools::DevToolPurpose::Formatting
                    || configured_formatters.contains(tool.command)
            })
            .collect::<Vec<_>>();
        if missing.is_empty() {
            return;
        }

        let remote_host = self
            .project
            .read(cx)
            .remote_connection_options(cx)
            .map(|options| options.display_name());
        let installable = missing
            .iter()
            .copied()
            .filter(|tool| tool.can_install())
            .collect::<Vec<_>>();
        let title = match &remote_host {
            Some(host) => i18n::t!("{language} tools are missing on the remote host ({host})"),
            None => i18n::t!("{language} tools are missing"),
        };
        let tool_labels = missing
            .iter()
            .map(|tool| format!("{} ({})", i18n::tr(tool.display_name), tool.purpose.label()))
            .collect::<Vec<_>>();
        let guidance = missing
            .iter()
            .filter_map(|tool| match tool.acquisition {
                dev_tools::DevToolAcquisition::Guidance(guidance) => Some(format!(
                    "{}: {}",
                    i18n::tr(tool.display_name),
                    i18n::tr(guidance)
                )),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let install_label = if remote_host.is_some() {
            i18n::t!("Install on Remote")
        } else {
            i18n::t!("Install")
        };
        // Where the bytes come from is the part that is not obvious, and it is
        // the reassuring part on a host with no route to the internet. Left out
        // when there is nothing to install, so it cannot read as an offer.
        let install_hint = match (&remote_host, installable.is_empty()) {
            (_, true) => String::new(),
            (Some(host), false) => i18n::t!(
                "They're downloaded over this computer's network and placed only in {host}'s \
                 home directory (no administrator rights needed; other users aren't affected)."
            ),
            (None, false) => {
                i18n::t!("They're placed only in this user's data directory.").to_string()
            }
        };
        let secondary_content = match (install_hint.is_empty(), guidance.is_empty()) {
            (false, false) => format!("{install_hint}\n{guidance}"),
            (false, true) => install_hint,
            (true, false) => guidance,
            (true, true) => String::new(),
        };

        let indicator = cx.entity().downgrade();
        let ignore_ids = missing
            .iter()
            .map(|tool| tool.id.to_string())
            .collect::<Vec<_>>();
        let language_for_install = language.clone();
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            workspace.show_notification(
                NotificationId::composite::<DevToolsMissing>(language.as_ref().to_string()),
                cx,
                move |cx| {
                    let title = title.clone();
                    let tool_labels = tool_labels.clone();
                    let secondary_content = secondary_content.clone();
                    let indicator = indicator.clone();
                    let installable = installable.clone();
                    let ignore_ids = ignore_ids.clone();
                    let language = language_for_install.clone();
                    cx.new(move |cx| {
                        let mut notification =
                            MessageNotification::new_from_builder(cx, move |_, _| {
                                v_flex()
                                    .gap_1()
                                    .children(tool_labels.iter().map(|label| {
                                        Label::new(i18n::t!("• {label}"))
                                            .size(LabelSize::Small)
                                            .color(Color::Muted)
                                    }))
                                    .into_any_element()
                            })
                            .with_title(title)
                            .content_icon(IconName::Warning, Color::Warning);
                        if !secondary_content.is_empty() {
                            notification =
                                notification.secondary_content(secondary_content.clone());
                        }
                        let notification = notification
                            .secondary_message(i18n::t!("Don't Show This Again"))
                            .secondary_on_click({
                                let indicator = indicator.clone();
                                let ignore_ids = ignore_ids.clone();
                                move |_, cx| {
                                    indicator
                                        .update(cx, |indicator, cx| {
                                            indicator.ignore_dev_tools(ignore_ids.clone(), cx);
                                        })
                                        .log_err();
                                }
                            });
                        if installable.is_empty() {
                            notification
                        } else {
                            notification
                                .primary_message(install_label)
                                .primary_icon(IconName::Download)
                                .primary_on_click(move |_, cx| {
                                    indicator
                                        .update(cx, |indicator, cx| {
                                            indicator.install_dev_tools(
                                                language.clone(),
                                                installable.clone(),
                                                cx,
                                            );
                                        })
                                        .log_err();
                                })
                        }
                    })
                },
            );
        });
    }

    /// Installs the tools the user accepted, then restarts that language's
    /// servers so the buffers already open pick them up.
    fn install_dev_tools(
        &mut self,
        language: LanguageName,
        tools: Vec<&'static dev_tools::DevTool>,
        cx: &mut Context<Self>,
    ) {
        let downloads = tools
            .iter()
            .filter(|tool| tool.language_server_name().is_none())
            .map(|tool| (tool.id, i18n::tr(tool.display_name)))
            .collect::<Vec<_>>();
        let servers = tools
            .iter()
            .filter_map(|tool| tool.language_server_name())
            .map(LanguageServerName::new_static)
            .collect::<Vec<_>>();
        let message = match downloads.first() {
            Some((_, display_name)) => i18n::t!("Installing {display_name}"),
            None => i18n::t!("Downloading the {language} language server"),
        };

        let project = self.project.clone();
        let workspace = self.workspace.clone();
        let task = cx.spawn(async move |this, cx| {
            let mut installed = Vec::new();
            for (id, display_name) in downloads {
                let install = project.update(cx, |project, cx| {
                    project.install_dev_tool(id.to_string(), cx)
                });
                match install.await {
                    Ok(_) => installed.push(display_name),
                    Err(error) => {
                        this.update(cx, |this, cx| {
                            this.report_dev_tool_failure(id, display_name, error, cx)
                        })
                        .log_err();
                        return;
                    }
                }
            }

            // A language server is installed by letting Zed's own download run
            // again; the restart is also what makes a freshly installed
            // formatter or linter visible to the buffers already open.
            if !servers.is_empty() {
                project.update(cx, |project, cx| {
                    let buffers = project.opened_buffers(cx);
                    project.restart_language_servers_for_buffers(
                        buffers,
                        servers
                            .iter()
                            .cloned()
                            .map(LanguageServerSelector::Name)
                            .collect(),
                        true,
                        cx,
                    );
                });
            }

            this.update(cx, |this, cx| {
                this.dev_tools_install = None;
                cx.notify();
            })
            .log_err();

            if !installed.is_empty() {
                workspace
                    .update(cx, |workspace, cx| {
                        struct DevToolsInstalled;
                        workspace.show_notification(
                            NotificationId::unique::<DevToolsInstalled>(),
                            cx,
                            |cx| {
                                cx.new(|cx| {
                                    MessageNotification::new(
                                        i18n::t!("Installed {}", installed.join(" / ")),
                                        cx,
                                    )
                                    .content_icon(IconName::Check, Color::Success)
                                })
                            },
                        );
                    })
                    .log_err();
            }
        });
        self.dev_tools_install = Some(DevToolInstall {
            message: message.into(),
            _task: task,
        });
        cx.notify();
    }

    /// Explains why an install did not happen. An allowlist refusal gets the
    /// hosts and a way to accept them, the same shape as ADR-F22, because that
    /// is the one failure the user can act on from here.
    fn report_dev_tool_failure(
        &mut self,
        tool_id: &'static str,
        display_name: &'static str,
        error: anyhow::Error,
        cx: &mut Context<Self>,
    ) {
        self.dev_tools_install = None;
        cx.notify();
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let hosts = dev_tools::egress_denied_hosts(&error);
        let indicator = cx.entity().downgrade();
        workspace.update(cx, |workspace, cx| {
            workspace.show_notification(
                NotificationId::composite::<DevToolsInstallFailed>(tool_id),
                cx,
                move |cx| {
                    let hosts = hosts.clone();
                    let indicator = indicator.clone();
                    cx.new(move |cx| {
                        let notification = match &hosts {
                            Some(hosts) => MessageNotification::new(
                                i18n::t!(
                                    "The following hosts needed to download {display_name} were denied by the network allowlist:\n{}",
                                    hosts
                                        .iter()
                                        .map(|host| i18n::t!("• {host}"))
                                        .collect::<Vec<_>>()
                                        .join("\n")
                                ),
                                cx,
                            ),
                            None => MessageNotification::new(
                                i18n::t!("Couldn't install {display_name}: {error:#}"),
                                cx,
                            ),
                        }
                        .content_icon(IconName::Warning, Color::Warning);
                        match hosts {
                            Some(hosts) => notification
                                .primary_message(i18n::t!("Add to Allowlist and Retry"))
                                .primary_icon(IconName::Check)
                                .primary_on_click(move |_, cx| {
                                    indicator
                                        .update(cx, |indicator, cx| {
                                            indicator.allow_hosts_and_install(
                                                tool_id,
                                                display_name,
                                                hosts.clone(),
                                                cx,
                                            );
                                        })
                                        .log_err();
                                }),
                            None => notification,
                        }
                    })
                },
            );
        });
    }

    /// Accepts the hosts a tool download needs and runs it again.
    fn allow_hosts_and_install(
        &mut self,
        tool_id: &'static str,
        display_name: &'static str,
        hosts: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        let fs = self.project.read(cx).fs().clone();
        let path = paths::network_allowlist_file().clone();
        let policy = cx
            .try_global::<egress_policy::EgressAllowlistGlobal>()
            .map(|global| global.0.clone());
        let project = self.project.clone();
        let task = cx.spawn(async move |this, cx| {
            if let Err(error) = egress_policy::add_allowed_hosts(&fs, &path, &hosts).await {
                this.update(cx, |this, cx| {
                    this.report_dev_tool_failure(tool_id, display_name, error, cx)
                })
                .log_err();
                return;
            }
            wait_for_allowlist(policy, &hosts, cx).await;
            // The download runs on the remote host, so it is judged against the
            // list that host holds; push it before retrying, as ADR-F22 does.
            let install = project.update(cx, |project, cx| {
                if let Some(remote_client) = project.remote_client() {
                    remote_client.read(cx).push_egress_allowlist(cx);
                }
                project.install_dev_tool(tool_id.to_string(), cx)
            });
            match install.await {
                Ok(_) => {
                    this.update(cx, |this, cx| {
                        this.dev_tools_install = None;
                        cx.notify();
                    })
                    .log_err();
                }
                Err(error) => {
                    this.update(cx, |this, cx| {
                        this.report_dev_tool_failure(tool_id, display_name, error, cx)
                    })
                    .log_err();
                }
            }
        });
        self.dev_tools_install = Some(DevToolInstall {
            message: i18n::t!("Installing {display_name}").into(),
            _task: task,
        });
        cx.notify();
    }

    fn ignore_dev_tools(&mut self, tool_ids: Vec<String>, cx: &mut Context<Self>) {
        let fs = self.project.read(cx).fs().clone();
        settings::update_settings_file(fs, cx, move |content, _| {
            let dev_tools = content
                .project
                .dev_tools
                .get_or_insert_with(Default::default);
            let ignored = dev_tools.ignored.get_or_insert_with(Vec::new);
            for tool_id in tool_ids {
                if !ignored.contains(&tool_id) {
                    ignored.push(tool_id);
                }
            }
        });
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
                    status: SharedString::from(i18n::t!(
                        "The following hosts needed for the download were denied by the network allowlist:\n{}\n\n{}\n\n{error}",
                        hosts
                            .iter()
                            .map(|host| i18n::t!("• {host}"))
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
                    message.push_str(&i18n::t!(" + {} more", additional_work_count));
                }

                return Some(Content {
                    icon: ActivityIcon::LoadingSpinner,
                    message,
                    on_click: None,
                    tooltip_message: None,
                });
            }
        }

        // ADR-F39: a download the user pressed a button for, which over a
        // relayed link is slow enough that silence looks like nothing happened.
        if let Some(install) = &self.dev_tools_install {
            return Some(Content {
                icon: ActivityIcon::LoadingSpinner,
                message: install.message.to_string(),
                on_click: None,
                tooltip_message: None,
            });
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
                message: i18n::t!("Debug: {}", session.read(cx).adapter()),
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
                message: i18n::t!(
                    "Downloading {}...",
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
                message: i18n::t!(
                    "Checking for updates to {}...",
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
                message: i18n::t!(
                    "Downloading {} was blocked by the egress control. Click to show the hosts it needs.",
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
                message: i18n::t!(
                    "Couldn't download {}. Click to show error.",
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
                message: i18n::t!(
                    "Failed to run {}. Click to show error.",
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
                message: i18n::t!("Formatting failed: {failure}. Click to see logs."),
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
                ServerHealth::Warning => i18n::t!("({server_name}) Warning: "),
                ServerHealth::Error => i18n::t!("({server_name}) Error: "),
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

        if let Some(message) = symbol_index_progress(
            self.project
                .read(cx)
                .symbol_index_store()
                .read(cx)
                .statuses(cx)
                .into_iter()
                .map(|(_, status)| status),
        ) {
            return Some(Content {
                icon: ActivityIcon::LoadingSpinner,
                message,
                on_click: None,
                tooltip_message: None,
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
                    i18n::t!("Installing {extension_id} extension…"),
                    ActivityIcon::LoadingSpinner,
                ),
                ExtensionOperation::Upgrade => (
                    i18n::t!("Updating {extension_id} extension…"),
                    ActivityIcon::Icon(IconName::Download),
                ),
                ExtensionOperation::Remove => (
                    i18n::t!("Removing {extension_id} extension…"),
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
            message: i18n::t!("Partial file index").to_string(),
            tooltip_message: Some(i18n::t!("Directories outside of git repositories and deeper than the `file_scan_depth` setting will be indexed on demand.").to_string()),
            on_click: Some(Arc::new(|this, _, cx| {
                this.deferred_scan_message = DeferredScanMessage::Dismissed;
                cx.notify();
            })),
        })
    }
}

impl EventEmitter<Event> for ActivityIndicator {}

/// ADR-F41: what the status bar says while the symbol index of a worktree is being built.
/// Waiting for the first scan is not mentioned: every project passes through it at startup,
/// including those that turn out to have no C to index.
fn symbol_index_progress(statuses: impl IntoIterator<Item = SymbolIndexStatus>) -> Option<String> {
    let (done, total) = statuses
        .into_iter()
        .filter_map(|status| match status {
            SymbolIndexStatus::Indexing { done, total } => Some((done, total)),
            SymbolIndexStatus::Scanning
            | SymbolIndexStatus::Ready { .. }
            | SymbolIndexStatus::Disabled
            | SymbolIndexStatus::Unavailable { .. } => None,
        })
        .fold((0, 0), |(all_done, all_total), (done, total)| {
            (all_done + done, all_total + total)
        });
    (total > 0).then(|| {
        i18n::t!(
            "Building the symbol index ({done} / {total})",
            done = group_digits(done),
            total = group_digits(total)
        )
    })
}

/// `12345` as `12,345`.
fn group_digits(number: usize) -> String {
    let digits = number.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

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
                                    let title = SharedString::from(i18n::t!("Cancel {title}"));
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
        active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // ADR-F39: opening a file is the moment its language's tools are about
        // to matter, and the only moment the user is thinking about them.
        if let Some(editor) = active_pane_item.and_then(|item| item.downcast::<Editor>()) {
            self.check_dev_tools(&editor, cx);
        }
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
    use std::{cell::Cell, rc::Rc};

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
        assert!(!unknown.contains("e.g."), "{unknown}");
    }

    #[test]
    fn symbol_index_progress_sums_the_worktrees_being_built() {
        let ready = SymbolIndexStatus::Ready {
            stats: Default::default(),
        };
        assert_eq!(
            symbol_index_progress([
                SymbolIndexStatus::Indexing {
                    done: 12_000,
                    total: 50_000
                },
                ready.clone(),
                SymbolIndexStatus::Indexing {
                    done: 345,
                    total: 10_000
                },
            ])
            .as_deref(),
            Some("Building the symbol index (12,345 / 60,000)")
        );
        assert_eq!(
            symbol_index_progress([SymbolIndexStatus::Scanning, ready.clone()]),
            None,
            "every project waits for its first scan, C or not"
        );
        assert_eq!(
            symbol_index_progress([SymbolIndexStatus::Indexing { done: 0, total: 0 }]),
            None,
            "nothing counted yet"
        );
        assert_eq!(
            symbol_index_progress([ready, SymbolIndexStatus::Disabled]),
            None
        );
    }

    #[gpui::test]
    async fn the_status_bar_shows_the_symbol_index_being_built(cx: &mut TestAppContext) {
        cx.update(|cx| {
            workspace::AppState::test(cx);
        });
        let fs = project::FakeFs::new(cx.executor());
        fs.insert_tree(
            util::path!("/project"),
            serde_json::json!({ "main.c": "int main(void) { return 0; }\n" }),
        )
        .await;
        let project = Project::test(fs, [std::path::Path::new(util::path!("/project"))], cx).await;
        let worktree_id = project.read_with(cx, |project, cx| {
            project
                .worktrees(cx)
                .next()
                .expect("the project has a worktree")
                .read(cx)
                .id()
        });
        let directory = tempfile::tempdir().expect("a temporary directory");
        let index = project::symbol_index_store::build_index_for_tests(
            directory.path(),
            &[("main.c", "int main(void) { return 0; }\n")],
        )
        .expect("building the index");
        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace =
            multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
        let indicator = workspace.update_in(cx, |workspace, window, cx| {
            ActivityIndicator::new(workspace, window, cx)
        });
        let set_status = |status: SymbolIndexStatus, cx: &mut gpui::VisualTestContext| {
            project.update(cx, |project, cx| {
                project.symbol_index_store().update(cx, |store, cx| {
                    store.set_status_for_tests(worktree_id, status, cx)
                })
            });
            cx.run_until_parked();
        };
        let message = |cx: &mut gpui::VisualTestContext| {
            indicator.update(cx, |indicator, cx| {
                indicator
                    .content_to_render(cx)
                    .map(|content| content.message)
            })
        };
        let notified = Rc::new(Cell::new(0));
        let _observation = cx.update(|_, cx| {
            let notified = notified.clone();
            cx.observe(&indicator, move |_, _| notified.set(notified.get() + 1))
        });
        project.update(cx, |project, cx| {
            project.symbol_index_store().update(cx, |store, cx| {
                store.insert_index_for_tests(worktree_id, index, cx)
            })
        });
        cx.run_until_parked();
        let before = notified.get();

        set_status(
            SymbolIndexStatus::Indexing {
                done: 12_345,
                total: 60_000,
            },
            cx,
        );
        assert!(
            notified.get() > before,
            "the indicator is drawn again when the index reports progress"
        );
        assert_eq!(
            message(cx).as_deref(),
            Some("Building the symbol index (12,345 / 60,000)")
        );
        set_status(
            SymbolIndexStatus::Ready {
                stats: Default::default(),
            },
            cx,
        );
        assert_eq!(message(cx), None);
    }
}
