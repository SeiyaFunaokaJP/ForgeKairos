//! ADR-F94: installing a vendor's agent from the ACP registry straight from the
//! `+` menu. The decision asked of the user is the registry page's (F2-5,
//! ADR-F12): the same [`InstallConsentBanner`] when the install needs hosts
//! added to the allowlist or goes through npx, nothing when it does not. Once
//! the agent server store knows the agent, a thread with it opens.

use std::time::Duration;

use gpui::{
    DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Subscription, Task, WeakEntity,
    prelude::*,
};
use project::AgentId;
use project::agent_server_store::{AgentServerStore, AgentServersUpdated};
use ui::{CommonAnimationExt as _, Modal, ModalFooter, ModalHeader, Section, prelude::*};
use util::ResultExt as _;
use workspace::{DismissDecision, ModalView, Workspace};

use crate::NewExternalAgentThread;
use crate::agent_registry_ui::{
    InstallConsentAction, InstallConsentBanner, InstallRequirement, PendingInstallConsent,
    add_consented_hosts_and_await_pickup, add_registry_agent_to_settings, missing_hosts,
    registry_install_requirement,
};

/// How long the window waits for the agent server store to take in the agent
/// the settings file now names.
const AGENT_PICKUP_TIMEOUT: Duration = Duration::from_secs(30);

/// Whether an install can go ahead without asking: the same rule as the
/// registry page's "Install" button.
pub(crate) fn needs_install_consent(
    requirement: &InstallRequirement,
    missing: &[SharedString],
) -> bool {
    requirement.is_npx || !missing.is_empty()
}

pub(crate) fn install_from_menu(
    agent_id: AgentId,
    workspace: &WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(workspace) = workspace.upgrade() else {
        return;
    };
    let Some(requirement) = registry_install_requirement(&agent_id, cx) else {
        // The registry is not in memory any more, so what the install needs
        // cannot be told: the registry page fetches it again.
        window.dispatch_action(Box::new(zed_actions::AcpRegistry), cx);
        return;
    };
    let display_name = project::AgentRegistryStore::try_global(cx)
        .and_then(|registry| {
            registry
                .read(cx)
                .agent(&agent_id)
                .map(|agent| agent.name().clone())
        })
        .unwrap_or_else(|| agent_id.0.clone());
    let store = workspace
        .read(cx)
        .project()
        .read(cx)
        .agent_server_store()
        .clone();
    workspace.update(cx, |workspace, cx| {
        workspace.toggle_modal(window, cx, |window, cx| {
            AgentInstallModal::new(agent_id, display_name, requirement, store, window, cx)
        });
    });
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum InstallState {
    AskingConsent,
    Installing,
    Failed(SharedString),
}

pub(crate) struct AgentInstallModal {
    agent_id: AgentId,
    display_name: SharedString,
    consent: Option<PendingInstallConsent>,
    state: InstallState,
    store: Entity<AgentServerStore>,
    thread_requested: bool,
    focus_handle: FocusHandle,
    _pending: Vec<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl AgentInstallModal {
    pub(crate) fn new(
        agent_id: AgentId,
        display_name: SharedString,
        requirement: InstallRequirement,
        store: Entity<AgentServerStore>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let missing = missing_hosts(&requirement.hosts, cx);
        let asks_first = needs_install_consent(&requirement, &missing);
        let subscriptions = vec![cx.subscribe_in(
            &store,
            window,
            |this, _, _: &AgentServersUpdated, window, cx| {
                this.open_thread_once_added(window, cx);
            },
        )];
        let mut this = Self {
            agent_id,
            display_name,
            consent: asks_first.then(|| PendingInstallConsent::new(requirement)),
            state: if asks_first {
                InstallState::AskingConsent
            } else {
                InstallState::Installing
            },
            store,
            thread_requested: false,
            focus_handle: cx.focus_handle(),
            _pending: Vec::new(),
            _subscriptions: subscriptions,
        };
        if !asks_first {
            this.install(cx);
        }
        this
    }

    fn is_added(&self, cx: &App) -> bool {
        self.store
            .read(cx)
            .external_agents()
            .any(|id| *id == self.agent_id)
    }

    fn install(&mut self, cx: &mut Context<Self>) {
        self.state = InstallState::Installing;
        add_registry_agent_to_settings(self.agent_id.as_ref(), cx);
        let timeout = cx.background_executor().timer(AGENT_PICKUP_TIMEOUT);
        self._pending.push(cx.spawn(async move |this, cx| {
            timeout.await;
            this.update(cx, |this, cx| {
                if this.state == InstallState::Installing && !this.thread_requested {
                    this.state = InstallState::Failed(
                        i18n::t!(
                            "The agent was added to settings.json, but it has not been loaded yet. Open it later from the + menu."
                        )
                        .into(),
                    );
                    cx.notify();
                }
            })
            .log_err();
        }));
        cx.notify();
    }

    pub(crate) fn open_thread_once_added(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.thread_requested || self.state != InstallState::Installing || !self.is_added(cx) {
            return;
        }
        self.thread_requested = true;
        window.dispatch_action(
            Box::new(NewExternalAgentThread {
                agent: self.agent_id.clone(),
            }),
            cx,
        );
        cx.emit(DismissEvent);
    }

    /// The registry page's `confirm_install`: the hosts first, and the install
    /// only once the running policy has them.
    pub(crate) fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(consent) = self.consent.as_mut() else {
            return;
        };
        if consent.waiting {
            return;
        }
        let hosts = consent
            .hosts
            .iter()
            .map(SharedString::to_string)
            .collect::<Vec<_>>();
        if hosts.is_empty() {
            self.consent = None;
            self.install(cx);
            self.open_thread_once_added(window, cx);
            return;
        }
        consent.waiting = true;
        consent.error = None;
        let write_hosts = add_consented_hosts_and_await_pickup(hosts, cx);
        self._pending
            .push(cx.spawn_in(window, async move |this, cx| {
                let result = write_hosts.await;
                this.update_in(cx, |this, window, cx| {
                    match result {
                        Ok(()) => {
                            this.consent = None;
                            this.install(cx);
                            this.open_thread_once_added(window, cx);
                        }
                        Err(failure) => {
                            if let Some(consent) = this.consent.as_mut() {
                                consent.waiting = false;
                                consent.error = Some(failure.message());
                            }
                        }
                    }
                    cx.notify();
                })
                .log_err();
            }));
        cx.notify();
    }

    /// Once the hosts are on their way, closing would leave them added with
    /// nothing installed.
    fn is_writing_hosts(&self) -> bool {
        self.consent.as_ref().is_some_and(|consent| consent.waiting)
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_writing_hosts() {
            return;
        }
        cx.emit(DismissEvent);
    }
}

impl EventEmitter<DismissEvent> for AgentInstallModal {}

impl Focusable for AgentInstallModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ModalView for AgentInstallModal {
    /// A click outside the window or another modal closes it without going
    /// through [`Self::cancel`].
    fn on_before_dismiss(&mut self, _: &mut Window, _: &mut Context<Self>) -> DismissDecision {
        DismissDecision::Dismiss(!self.is_writing_hosts())
    }
}

impl Render for AgentInstallModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let name = self.display_name.clone();
        let body = match (&self.state, &self.consent) {
            (InstallState::AskingConsent, Some(consent)) => InstallConsentBanner::new(
                format!("menu-{}", self.agent_id.as_ref()),
                consent,
                InstallConsentAction::Install,
                cx.listener(|this, _, window, cx| this.confirm(window, cx)),
                cx.listener(|this, _, window, cx| this.cancel(&menu::Cancel, window, cx)),
            )
            .into_any_element(),
            (InstallState::Failed(error), _) => Label::new(error.clone())
                .size(LabelSize::Small)
                .color(Color::Error)
                .into_any_element(),
            _ => h_flex()
                .gap_1p5()
                .child(
                    Icon::new(IconName::LoadCircle)
                        .size(IconSize::XSmall)
                        .color(Color::Muted)
                        .with_rotate_animation(2),
                )
                .child(
                    Label::new(i18n::t!("Installing {name}…"))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element(),
        };

        v_flex()
            .id("agent-install-modal")
            .key_context("AgentInstallModal")
            .w(rems(34.))
            .elevation_3(cx)
            .overflow_hidden()
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::cancel))
            .child(
                Modal::new("agent-install", None)
                    .header(
                        ModalHeader::new()
                            .headline(i18n::t!("Install {name}"))
                            .description(i18n::t!(
                                "Installs {name} from the ACP registry, then starts a thread with it. You sign in on the agent's own screen."
                            ))
                            .show_dismiss_button(true),
                    )
                    .section(Section::new().child(body))
                    .footer(
                        ModalFooter::new().start_slot(
                            Button::new("agent-install-registry", i18n::t!("Open ACP Registry"))
                                .label_size(LabelSize::Small)
                                .tab_index(0isize)
                                .on_click(cx.listener(|_, _, window, cx| {
                                    window.dispatch_action(Box::new(zed_actions::AcpRegistry), cx);
                                    cx.emit(DismissEvent);
                                })),
                        ),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use collections::HashMap;
    use fs::{FakeFs, Fs as _};
    use gpui::{TestAppContext, VisualTestContext};
    use http_client::{EgressPolicy, EgressPolicyHandle};
    use project::agent_registry_store::{RegistryAgentMetadata, RegistryNpxAgent};
    use project::{AgentRegistryStore, RegistryAgent};
    use std::sync::Arc;

    use super::*;

    const AGENT_ID: &str = "claude-acp";

    #[test]
    fn only_an_install_that_needs_nothing_new_goes_ahead_without_asking() {
        let binary = |hosts: &[&str]| InstallRequirement {
            hosts: hosts.iter().map(|host| SharedString::from(*host)).collect(),
            is_npx: false,
        };
        assert!(!needs_install_consent(&binary(&["github.com"]), &[]));
        assert!(needs_install_consent(
            &binary(&["github.com"]),
            &["github.com".into()]
        ));
        assert!(
            needs_install_consent(
                &InstallRequirement {
                    hosts: Vec::new(),
                    is_npx: true,
                },
                &[]
            ),
            "npx always shows its warning"
        );
    }

    fn policy_allowing(hosts: &[&str]) -> EgressPolicy {
        let (policy, errors) = EgressPolicy::from_host_strings(hosts);
        assert!(errors.is_empty(), "{errors:?}");
        policy
    }

    struct Harness {
        fs: Arc<FakeFs>,
        policy: EgressPolicyHandle,
        workspace: Entity<Workspace>,
    }

    async fn harness(cx: &mut TestAppContext) -> (Harness, VisualTestContext) {
        crate::test_support::init_test(cx);
        let fs = FakeFs::new(cx.executor());
        let policy = EgressPolicyHandle::new(EgressPolicy::deny_all());
        cx.update(|cx| {
            <dyn fs::Fs>::set_global(fs.clone(), cx);
            cx.set_global(egress_policy::EgressAllowlistGlobal(policy.clone()));
            AgentRegistryStore::init_test_global(
                cx,
                vec![RegistryAgent::Npx(RegistryNpxAgent {
                    metadata: RegistryAgentMetadata {
                        id: AgentId::new(AGENT_ID),
                        name: "Claude Agent".into(),
                        description: SharedString::from(""),
                        version: SharedString::from("1.0.0"),
                        repository: None,
                        website: None,
                        license_url: None,
                        icon_path: None,
                    },
                    package: SharedString::from("@agentclientprotocol/claude-agent-acp"),
                    args: Vec::new(),
                    env: HashMap::default(),
                })],
            );
        });
        for file in [paths::network_allowlist_file(), paths::settings_file()] {
            if let Some(directory) = file.parent() {
                fs.create_dir(directory)
                    .await
                    .expect("the fake file system takes the directory");
            }
        }
        let project = project::Project::test(fs.clone(), [], cx).await;
        let window =
            cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project, window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("the window is open");
        let visual = VisualTestContext::from_window(window.into(), cx);
        (
            Harness {
                fs,
                policy,
                workspace,
            },
            visual,
        )
    }

    impl Harness {
        fn open(&self, cx: &mut VisualTestContext) -> Entity<AgentInstallModal> {
            let workspace = self.workspace.downgrade();
            cx.update(|window, cx| {
                install_from_menu(AgentId::new(AGENT_ID), &workspace, window, cx)
            });
            cx.run_until_parked();
            self.modal(cx).expect("the install window is open")
        }

        fn modal(&self, cx: &mut VisualTestContext) -> Option<Entity<AgentInstallModal>> {
            self.workspace.read_with(cx, |workspace, cx| {
                workspace.active_modal::<AgentInstallModal>(cx)
            })
        }

        async fn settings_file(&self) -> String {
            self.fs
                .load(paths::settings_file())
                .await
                .unwrap_or_default()
        }
    }

    #[gpui::test]
    async fn an_install_from_the_menu_asks_first_and_then_opens_a_thread(cx: &mut TestAppContext) {
        let (harness, mut cx) = harness(cx).await;
        let cx = &mut cx;
        let modal = harness.open(cx);
        assert_eq!(
            modal.read_with(cx, |modal, _| modal.state.clone()),
            InstallState::AskingConsent
        );

        modal.update_in(cx, |modal, window, cx| modal.confirm(window, cx));
        cx.run_until_parked();
        assert!(
            !harness.settings_file().await.contains(AGENT_ID),
            "nothing is installed while the policy does not have the hosts yet"
        );
        assert!(!modal.read_with(cx, |modal, _| modal.thread_requested));

        harness
            .policy
            .set(policy_allowing(&["nodejs.org", "registry.npmjs.org"]));
        cx.run_until_parked();
        assert_eq!(
            modal.read_with(cx, |modal, _| modal.state.clone()),
            InstallState::Installing
        );
        assert!(
            harness.settings_file().await.contains(AGENT_ID),
            "the install is the registry page's: settings.json names the agent"
        );
        assert!(
            modal.read_with(cx, |modal, _| modal.thread_requested),
            "the thread is asked for once the agent server store has the agent"
        );
        assert!(
            harness.modal(cx).is_none(),
            "the window closes with the thread"
        );
    }

    /// A thread asked for before the agent server store has the agent would
    /// fail to connect.
    #[gpui::test]
    async fn the_thread_waits_for_the_agent_server_store(cx: &mut TestAppContext) {
        let (harness, mut cx) = harness(cx).await;
        let cx = &mut cx;
        let modal = harness.open(cx);
        modal.update_in(cx, |modal, window, cx| {
            modal.consent = None;
            modal.install(cx);
            modal.open_thread_once_added(window, cx);
            assert!(
                !modal.thread_requested,
                "settings.json is still being written"
            );
        });
        cx.run_until_parked();
        assert!(modal.read_with(cx, |modal, _| modal.thread_requested));
    }

    #[gpui::test]
    async fn the_window_stays_while_the_hosts_are_being_added(cx: &mut TestAppContext) {
        let (harness, mut cx) = harness(cx).await;
        let cx = &mut cx;
        let modal = harness.open(cx);
        modal.update_in(cx, |modal, window, cx| modal.confirm(window, cx));
        cx.run_until_parked();
        assert!(modal.read_with(cx, |modal, _| modal.is_writing_hosts()));

        harness.workspace.update_in(cx, |workspace, window, cx| {
            workspace.hide_modal(window, cx);
        });
        cx.run_until_parked();
        assert!(
            harness.modal(cx).is_some(),
            "closing it now would leave the hosts added with nothing installed"
        );

        harness
            .policy
            .set(policy_allowing(&["nodejs.org", "registry.npmjs.org"]));
        cx.run_until_parked();
        assert!(
            harness.settings_file().await.contains(AGENT_ID),
            "the install the hosts were added for goes ahead"
        );
    }

    #[gpui::test]
    async fn cancelling_the_consent_installs_nothing(cx: &mut TestAppContext) {
        let (harness, mut cx) = harness(cx).await;
        let cx = &mut cx;
        let modal = harness.open(cx);
        modal.update_in(cx, |modal, window, cx| {
            modal.cancel(&menu::Cancel, window, cx)
        });
        cx.run_until_parked();
        assert!(harness.modal(cx).is_none());
        assert!(!harness.settings_file().await.contains(AGENT_ID));
        assert!(
            harness
                .fs
                .load(paths::network_allowlist_file())
                .await
                .is_err(),
            "and adds no host"
        );
    }
}
