//! ADR-F101: looking for newer versions of the external agents installed from
//! the ACP registry — once when the IDE starts, or when the user asks — and
//! asking before any of them is updated. An agent's start launches what is
//! installed (its record) and never updates it unasked.

use fs::Fs;
use gpui::{App, AppContext as _, Context, Global, SharedString, TaskExt as _, Window, actions};
use project::agent_server_store::{AllAgentServersSettings, CustomAgentServerSettings};
use project::agent_update::{
    AgentDistributionKind, AgentInstallPlace, AgentServersUpdateCheckSettings,
    RecordedAgentVersion, local_agent_update_approvals, recorded_agent_versions, unix_now,
};
use project::{AgentId, AgentRegistryStore, DisableAiSettings, RegistryAgent};
use settings::Settings as _;
use util::ResultExt as _;
use workspace::{
    Toast, Workspace,
    notifications::{NotificationId, simple_message_notification::MessageNotification},
};

use crate::agent_registry_ui::{missing_hosts, registry_install_requirement};
use crate::{Agent, AgentPanel};

actions!(
    agent,
    [
        /// Checks the ACP registry for newer versions of the installed
        /// external agents, and asks before updating them.
        CheckForAgentUpdates
    ]
);

/// An installed agent whose registry version is not the one installed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AgentUpdate {
    pub(crate) id: AgentId,
    pub(crate) name: SharedString,
    pub(crate) installed: String,
    pub(crate) available: SharedString,
    /// The places whose install is behind, which agreeing to the update
    /// updates (each on its next start there).
    pub(crate) places: Vec<AgentInstallPlace>,
}

/// An agent the settings install from the registry, as the registry has it.
#[derive(Clone, Debug)]
struct InstalledRegistryAgent {
    id: AgentId,
    name: SharedString,
    available: SharedString,
    kind: AgentDistributionKind,
}

pub(crate) fn distribution_kind(agent: &RegistryAgent) -> AgentDistributionKind {
    match agent {
        RegistryAgent::Npx(_) => AgentDistributionKind::Npx,
        RegistryAgent::Binary(_) => AgentDistributionKind::Archive,
    }
}

/// The version shown as installed and the places behind `available`, among
/// what the places still in use recorded.
pub(crate) fn places_behind(
    recorded: &[RecordedAgentVersion],
    available: &str,
) -> Option<(String, Vec<AgentInstallPlace>)> {
    let behind = recorded
        .iter()
        .filter(|recorded| recorded.registry_version != available)
        .collect::<Vec<_>>();
    let installed = behind.first()?.registry_version.clone();
    Some((
        installed,
        behind
            .into_iter()
            .map(|recorded| recorded.place.clone())
            .collect(),
    ))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CheckReason {
    /// The check when the IDE starts: it says nothing unless there is an
    /// update.
    Startup,
    /// The user asked: every outcome is reported.
    Asked,
}

/// The check at startup is made once per process, in its first workspace.
struct StartupCheckStarted;

impl Global for StartupCheckStarted {}

/// The question whether to update.
struct AgentUpdatesNotification;

/// What a check the user asked for found when there is nothing to update.
struct AgentUpdateCheckMessage;

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _window, cx| {
        workspace.register_action(
            |workspace: &mut Workspace, _: &CheckForAgentUpdates, _window, cx| {
                check_for_agent_updates(workspace, CheckReason::Asked, cx);
            },
        );
        if !cx.has_global::<StartupCheckStarted>() && checks_on_startup(cx) {
            cx.set_global(StartupCheckStarted);
            check_for_agent_updates(workspace, CheckReason::Startup, cx);
        }
    })
    .detach();
}

fn checks_on_startup(cx: &App) -> bool {
    AgentServersUpdateCheckSettings::checks_on_startup(cx)
        && !DisableAiSettings::get_global(cx).disable_ai
}

fn check_for_agent_updates(
    workspace: &mut Workspace,
    reason: CheckReason,
    cx: &mut Context<Workspace>,
) {
    let Some(registry) = AgentRegistryStore::try_global(cx) else {
        if reason == CheckReason::Asked {
            show_message(
                workspace,
                i18n::t!("The ACP registry is not available, so agent updates cannot be checked.")
                    .into(),
                cx,
            );
        }
        return;
    };
    let refresh = registry.update(cx, |registry, cx| {
        registry.refresh_and_wait(reason == CheckReason::Asked, cx)
    });
    let fs = workspace.app_state().fs.clone();
    cx.spawn(async move |workspace, cx| {
        refresh.await;
        let (fetch_error, installed) = cx.update(|cx| {
            let registry = registry.read(cx);
            (
                registry.fetch_error(),
                installed_registry_agents(registry, cx),
            )
        });
        if reason == CheckReason::Asked
            && let Some(error) = fetch_error
        {
            workspace.update(cx, |workspace, cx| {
                show_message(
                    workspace,
                    i18n::t!("Could not check for agent updates: {error}").into(),
                    cx,
                )
            })?;
            return anyhow::Ok(());
        }
        let updates = pending_agent_updates(fs.as_ref(), installed, unix_now()).await;
        workspace.update(cx, |workspace, cx| {
            show_check_result(workspace, reason, updates, cx)
        })?;
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

/// The agents the settings install from the registry, with the registry's
/// name and version for each.
fn installed_registry_agents(
    registry: &AgentRegistryStore,
    cx: &App,
) -> Vec<InstalledRegistryAgent> {
    let settings = AllAgentServersSettings::get_global(cx);
    let mut agents = settings
        .iter()
        .filter(|(_, settings)| matches!(settings, CustomAgentServerSettings::Registry { .. }))
        .filter_map(|(id, _)| {
            let agent = registry.agent(&AgentId::new(id.clone()))?;
            Some(InstalledRegistryAgent {
                id: agent.id().clone(),
                name: agent.name().clone(),
                available: agent.version().clone(),
                kind: distribution_kind(agent),
            })
        })
        .collect::<Vec<_>>();
    agents.sort_by(|left, right| left.name.cmp(&right.name));
    agents
}

/// The installed agents whose record — on this host or in a WSL distro still
/// in use ([`recorded_agent_versions`], at `now`) — is for another version
/// than the registry's. An agent with no record has not been started since
/// it was installed (or since this check exists); its next start records it.
async fn pending_agent_updates(
    fs: &dyn Fs,
    installed: Vec<InstalledRegistryAgent>,
    now: u64,
) -> Vec<AgentUpdate> {
    let mut updates = Vec::new();
    for agent in installed {
        let recorded = recorded_agent_versions(fs, agent.id.as_ref(), agent.kind, now).await;
        if let Some((installed, places)) = places_behind(&recorded, agent.available.as_ref()) {
            updates.push(AgentUpdate {
                id: agent.id,
                name: agent.name,
                installed,
                available: agent.available,
                places,
            });
        }
    }
    updates
}

fn show_check_result(
    workspace: &mut Workspace,
    reason: CheckReason,
    updates: Vec<AgentUpdate>,
    cx: &mut Context<Workspace>,
) {
    if updates.is_empty() {
        if reason == CheckReason::Asked {
            show_message(
                workspace,
                i18n::t!("All external agents are up to date.").into(),
                cx,
            );
        }
        return;
    }

    let list = updates
        .iter()
        .map(|update| {
            i18n::t!(
                "{name} {installed} → {available}",
                name = update.name,
                installed = update.installed,
                available = update.available
            )
        })
        .collect::<Vec<_>>()
        .join(i18n::tc!("list separator", ", "));
    let message = i18n::tn!(
        updates.len(),
        "An update is available for {count} external agent: {list}",
        "Updates are available for {count} external agents: {list}",
        list = list
    );
    let workspace_handle = cx.weak_entity();
    workspace.show_notification(
        NotificationId::unique::<AgentUpdatesNotification>(),
        cx,
        |cx| {
            cx.new(|cx| {
                MessageNotification::new(message, cx)
                    .primary_message(i18n::t!("Update"))
                    .primary_icon(ui::IconName::Download)
                    .primary_on_click(move |window, cx| {
                        let updates = updates.clone();
                        workspace_handle
                            .update(cx, |workspace, cx| {
                                agree_to_agent_updates(workspace, updates, window, cx)
                            })
                            .log_err();
                    })
                    .secondary_message(i18n::t!("Later"))
                    .secondary_on_click(|_, _| {})
                    .show_suppress_button(false)
            })
        },
    );
}

fn show_message(workspace: &mut Workspace, message: SharedString, cx: &mut Context<Workspace>) {
    workspace.show_toast(
        Toast::new(
            NotificationId::unique::<AgentUpdateCheckMessage>(),
            message.to_string(),
        )
        .autohide(),
        cx,
    );
}

/// The notification's "Update". An update whose download needs hosts the
/// allowlist does not have yet is not agreed to here: the registry page asks
/// for them first, with the same consent as an install.
fn agree_to_agent_updates(
    workspace: &mut Workspace,
    updates: Vec<AgentUpdate>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let (needs_consent, agreed): (Vec<_>, Vec<_>) = updates.into_iter().partition(|update| {
        registry_install_requirement(&update.id, cx)
            .is_some_and(|requirement| !missing_hosts(&requirement.hosts, cx).is_empty())
    });
    approve_agent_updates(&agreed, cx);
    if !needs_consent.is_empty() {
        let page = crate::open_agent_registry_page(workspace, window, cx);
        page.update(cx, |page, cx| {
            for update in &needs_consent {
                page.begin_update(update.id.as_ref(), cx);
            }
        });
    }
}

/// ADR-F101: the user agreed to these updates. Each agent's next start on this
/// host (or in a WSL distro) installs the registry's version, and the threads
/// on its running connections show the update banner, which restarts them on
/// it.
pub(crate) fn approve_agent_updates(updates: &[AgentUpdate], cx: &mut App) {
    if updates.is_empty() {
        return;
    }
    let approvals = local_agent_update_approvals(cx);
    for update in updates {
        for place in &update.places {
            approvals.approve(&update.id, place.clone());
        }
    }
    // Deferred: the notification's "Update" runs while its workspace is being
    // updated, and announcing to every workspace from there would update that
    // one again.
    let updates = updates.to_vec();
    cx.defer(move |cx| announce_agent_updates(&updates, cx));
}

fn announce_agent_updates(updates: &[AgentUpdate], cx: &mut App) {
    let workspaces = workspace::AppState::try_global(cx)
        .map(|app_state| {
            app_state
                .workspace_store
                .read(cx)
                .workspaces()
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    for workspace in workspaces {
        workspace
            .update(cx, |workspace, cx| {
                // A remote host's agents are its installs, which these
                // records do not describe: their threads offer the update
                // when a start there finds it.
                if !workspace.project().read(cx).is_local() {
                    return;
                }
                let Some(panel) = workspace.panel::<AgentPanel>(cx) else {
                    return;
                };
                panel
                    .read(cx)
                    .connection_store()
                    .clone()
                    .update(cx, |store, cx| {
                        for update in updates {
                            store.announce_agent_update(
                                &Agent::from(update.id.clone()),
                                update.available.clone(),
                                cx,
                            );
                        }
                    });
            })
            .ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{BorrowAppContext as _, Entity, TestAppContext, VisualTestContext};
    use http_client::{AsyncBody, EgressPolicy, EgressPolicyHandle, FakeHttpClient, Response};
    use project::agent_update::{
        AgentServersUpdateCheck, InstalledAgentLaunch, InstalledAgentRecord,
        host_archive_record_path, host_npx_record_path, write_installed_record, wsl_record_path,
    };
    use settings::SettingsStore;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const AGENT_ID: &str = "test-agent";
    const REGISTRY_URL: &str =
        "https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json";
    const REGISTRY_JSON: &str = r#"{"version":"1","agents":[{"id":"test-agent","name":"Test Agent","version":"0.6.0","description":"","distribution":{"npx":{"package":"test-agent@0.6.0"}}}]}"#;

    /// A window opened the way the IDE opens its first one, with `test-agent`
    /// installed from a registry at 0.6.0 (cached on disk, and what a fetch
    /// returns), recorded at `recorded`, and `check` set.
    async fn open_first_window(
        check: AgentServersUpdateCheck,
        recorded: Option<&str>,
        cx: &mut TestAppContext,
    ) -> (Entity<Workspace>, VisualTestContext, Arc<AtomicUsize>) {
        crate::test_support::init_test(cx);
        cx.update(|cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings
                        .agent
                        .get_or_insert_default()
                        .agent_servers_update_check = Some(check);
                    settings.agent_servers.get_or_insert_default().insert(
                        AGENT_ID.to_string(),
                        settings::CustomAgentServerSettings::Registry {
                            env: Default::default(),
                            default_mode: None,
                            default_config_options: Default::default(),
                            favorite_config_option_values: Default::default(),
                            allow_subscription_sign_in: None,
                            run_outside_sandbox: None,
                        },
                    );
                });
            });
        });
        let fs = fs::FakeFs::new(cx.executor());
        let cache = paths::external_agents_dir()
            .join("registry")
            .join("registry.json");
        fs.create_dir(cache.parent().unwrap()).await.unwrap();
        fs.insert_file(&cache, REGISTRY_JSON.as_bytes().to_vec())
            .await;
        if let Some(version) = recorded {
            write_installed_record(
                fs.as_ref(),
                &host_npx_record_path(AGENT_ID),
                &record(version),
            )
            .await
            .unwrap();
        }
        let fetches = Arc::new(AtomicUsize::new(0));
        let http_client = FakeHttpClient::create({
            let fetches = fetches.clone();
            move |request| {
                if request.uri() == REGISTRY_URL {
                    fetches.fetch_add(1, Ordering::SeqCst);
                }
                async move {
                    Ok(Response::builder()
                        .status(200)
                        .body(AsyncBody::from(REGISTRY_JSON.as_bytes().to_vec()))?)
                }
            }
        });
        cx.update(|cx| {
            <dyn Fs>::set_global(fs.clone(), cx);
            AgentRegistryStore::init_global(cx, fs.clone(), http_client);
            init(cx);
        });

        let project = project::Project::test(fs, [], cx).await;
        let window =
            cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project, window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("the window is open");
        let cx = VisualTestContext::from_window(window.into(), cx);
        cx.run_until_parked();
        (workspace, cx, fetches)
    }

    fn shows(
        workspace: &Entity<Workspace>,
        id: NotificationId,
        cx: &mut VisualTestContext,
    ) -> bool {
        workspace.read_with(cx, |workspace, _| {
            workspace.notification_ids().contains(&id)
        })
    }

    fn asks_to_update(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
        shows(
            workspace,
            NotificationId::unique::<AgentUpdatesNotification>(),
            cx,
        )
    }

    fn says_nothing_to_update(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
        shows(
            workspace,
            NotificationId::unique::<AgentUpdateCheckMessage>(),
            cx,
        )
    }

    /// ADR-F101: at startup the registry is fetched once, and the user is asked
    /// when an installed agent's record is behind it.
    #[gpui::test]
    async fn the_startup_check_asks_when_an_agent_is_behind(cx: &mut TestAppContext) {
        let (workspace, mut cx, fetches) =
            open_first_window(AgentServersUpdateCheck::OnStartup, Some("0.5.1"), cx).await;
        assert_eq!(fetches.load(Ordering::SeqCst), 1, "fetched once");
        assert!(asks_to_update(&workspace, &mut cx));
    }

    /// ADR-F101: a startup check with nothing to update says nothing.
    #[gpui::test]
    async fn the_startup_check_is_silent_when_nothing_is_behind(cx: &mut TestAppContext) {
        let (workspace, mut cx, _) =
            open_first_window(AgentServersUpdateCheck::OnStartup, Some("0.6.0"), cx).await;
        assert!(!asks_to_update(&workspace, &mut cx));
        assert!(!says_nothing_to_update(&workspace, &mut cx));
    }

    /// ADR-F101: `manual` neither fetches nor asks at startup; asking reports
    /// what it found, "up to date" included.
    #[gpui::test]
    async fn manual_checks_only_when_asked(cx: &mut TestAppContext) {
        let (workspace, mut cx, fetches) =
            open_first_window(AgentServersUpdateCheck::Manual, Some("0.6.0"), cx).await;
        assert_eq!(fetches.load(Ordering::SeqCst), 0);
        assert!(!asks_to_update(&workspace, &mut cx));
        assert!(!says_nothing_to_update(&workspace, &mut cx));

        workspace.update(&mut cx, |workspace, cx| {
            check_for_agent_updates(workspace, CheckReason::Asked, cx)
        });
        cx.run_until_parked();
        assert_eq!(fetches.load(Ordering::SeqCst), 1, "asking fetches");
        assert!(says_nothing_to_update(&workspace, &mut cx));
        assert!(!asks_to_update(&workspace, &mut cx));
    }

    /// ADR-F101: "Update" agrees to an update whose hosts are allowed — for
    /// the places that are behind, and only those — and sends one that needs
    /// hosts to the registry page's consent instead.
    #[gpui::test]
    async fn update_agrees_only_where_the_hosts_are_allowed(cx: &mut TestAppContext) {
        let (workspace, mut cx, _) =
            open_first_window(AgentServersUpdateCheck::Manual, Some("0.5.1"), cx).await;
        let update = AgentUpdate {
            id: AgentId::new(AGENT_ID),
            name: "Test Agent".into(),
            installed: "0.5.1".to_string(),
            available: "0.6.0".into(),
            places: vec![AgentInstallPlace::wsl(Some("Ubuntu"))],
        };
        let approved_at = |place: AgentInstallPlace, cx: &mut VisualTestContext| {
            cx.update(|_, cx| {
                local_agent_update_approvals(cx).is_approved(&AgentId::new(AGENT_ID), &place)
            })
        };
        let approved =
            |cx: &mut VisualTestContext| approved_at(AgentInstallPlace::wsl(Some("Ubuntu")), cx);

        cx.update(|_, cx| {
            cx.set_global(egress_policy::EgressAllowlistGlobal(
                EgressPolicyHandle::new(EgressPolicy::deny_all()),
            ))
        });
        workspace.update_in(&mut cx, |workspace, window, cx| {
            agree_to_agent_updates(workspace, vec![update.clone()], window, cx)
        });
        cx.run_until_parked();
        assert!(!approved(&mut cx), "not before the hosts are allowed");
        let page =
            workspace
                .read_with(&cx, |workspace, cx| {
                    workspace.active_pane().read(cx).items().find_map(|item| {
                        item.downcast::<crate::agent_registry_ui::AgentRegistryPage>()
                    })
                })
                .expect("the registry page is open");
        assert!(page.read_with(&cx, |page, _| page.has_pending_update_consent(AGENT_ID)));

        let (allowing, errors) =
            EgressPolicy::from_host_strings(&["nodejs.org", "registry.npmjs.org"]);
        assert!(errors.is_empty());
        cx.update(|_, cx| {
            cx.set_global(egress_policy::EgressAllowlistGlobal(
                EgressPolicyHandle::new(allowing),
            ))
        });
        // As in the IDE, the workspace the notification is in is one of every
        // workspace the agreement is announced to.
        let app_state = workspace.read_with(&cx, |workspace, _| workspace.app_state().clone());
        cx.update(|_, cx| workspace::AppState::set_global(app_state, cx));
        workspace.update_in(&mut cx, |workspace, window, cx| {
            agree_to_agent_updates(workspace, vec![update], window, cx)
        });
        cx.run_until_parked();
        assert!(approved(&mut cx));
        assert!(
            !approved_at(AgentInstallPlace::Host, &mut cx),
            "a place that is not behind is not updated"
        );
    }

    fn record(version: &str) -> InstalledAgentRecord {
        npx_record(version, Some(unix_now()))
    }

    fn npx_record(version: &str, last_started: Option<u64>) -> InstalledAgentRecord {
        InstalledAgentRecord {
            registry_version: version.to_string(),
            launch: InstalledAgentLaunch::Npx {
                package: format!("agent@{version}"),
                executable: "/agent.js".to_string(),
                args: Vec::new(),
                env: Default::default(),
            },
            last_started,
        }
    }

    fn archive_record(version: &str, last_started: Option<u64>) -> InstalledAgentRecord {
        InstalledAgentRecord {
            registry_version: version.to_string(),
            launch: InstalledAgentLaunch::Archive {
                archive_url: "https://example.test/agent".to_string(),
                sha256: None,
                directory: "/v_1".to_string(),
                cmd: "./agent".to_string(),
                args: Vec::new(),
                env: Default::default(),
            },
            last_started,
        }
    }

    fn installed(id: &str, version: &str, kind: AgentDistributionKind) -> InstalledRegistryAgent {
        InstalledRegistryAgent {
            id: AgentId::new(id.to_string()),
            name: SharedString::from(format!("Agent {id}")),
            available: SharedString::from(version.to_string()),
            kind,
        }
    }

    /// ADR-F101: an update is one where what a place still in use recorded is
    /// not the registry's version, and it is for exactly those places. An
    /// agent recorded at that version, with no record at all, recorded only
    /// for another distribution kind than the registry's, or recorded only in
    /// a place that has not started it for 30 days, has none.
    #[gpui::test]
    async fn only_places_in_use_behind_the_registry_are_updates(cx: &mut TestAppContext) {
        let fs = fs::FakeFs::new(cx.executor());
        let now = 1_000 * 24 * 60 * 60;
        let day = 24 * 60 * 60;
        for (path, record) in [
            (
                host_npx_record_path("old"),
                npx_record("0.5.1", Some(now - day)),
            ),
            (
                wsl_record_path(Some("Ubuntu"), "old"),
                npx_record("0.6.0", Some(now)),
            ),
            (
                wsl_record_path(Some("Debian"), "old"),
                npx_record("0.4.0", Some(now - 40 * day)),
            ),
            (host_npx_record_path("both"), npx_record("0.1.0", None)),
            (
                wsl_record_path(Some("Ubuntu"), "both"),
                npx_record("0.1.0", Some(now)),
            ),
            (
                host_archive_record_path("current"),
                archive_record("2.0.0", Some(now)),
            ),
            (
                host_npx_record_path("switched"),
                npx_record("0.1.0", Some(now)),
            ),
            (
                wsl_record_path(None, "unused"),
                npx_record("0.1.0", Some(now - 31 * day)),
            ),
        ] {
            write_installed_record(fs.as_ref(), &path, &record)
                .await
                .unwrap();
        }

        let updates = pending_agent_updates(
            fs.as_ref(),
            vec![
                installed("old", "0.6.0", AgentDistributionKind::Npx),
                installed("both", "0.2.0", AgentDistributionKind::Npx),
                installed("current", "2.0.0", AgentDistributionKind::Archive),
                installed("switched", "1.0.0", AgentDistributionKind::Archive),
                installed("unused", "1.0.0", AgentDistributionKind::Npx),
                installed("never-started", "1.0.0", AgentDistributionKind::Npx),
            ],
            now,
        )
        .await;
        assert_eq!(
            updates,
            vec![
                AgentUpdate {
                    id: AgentId::new("old"),
                    name: "Agent old".into(),
                    installed: "0.5.1".to_string(),
                    available: "0.6.0".into(),
                    places: vec![AgentInstallPlace::Host],
                },
                AgentUpdate {
                    id: AgentId::new("both"),
                    name: "Agent both".into(),
                    installed: "0.1.0".to_string(),
                    available: "0.2.0".into(),
                    places: vec![
                        AgentInstallPlace::Host,
                        AgentInstallPlace::wsl(Some("Ubuntu"))
                    ],
                },
            ]
        );
    }
}
