use crate::{AgentServer, AgentServerDelegate, load_proxy_env};
use acp_thread::AgentConnection;
use agent_client_protocol::schema::v1 as acp;
use anyhow::{Context as _, Result};
use collections::HashSet;
use fs::Fs;
use gpui::{App, AppContext as _, Entity, Task};
use language_model::{ApiKey, EnvVar};
use project::{
    Project,
    agent_server_store::{AgentId, AgentLaunchTarget, AllAgentServersSettings},
};
use settings::{AgentConfigOptionValue, SettingsStore, update_settings_file};
use std::{rc::Rc, sync::Arc};
use ui::IconName;
use util::ResultExt as _;

pub const GEMINI_ID: &str = "gemini";
pub const CLAUDE_AGENT_ID: &str = "claude-acp";
pub const CODEX_ID: &str = "codex-acp";
pub const CURSOR_ID: &str = "cursor";

/// ADR-F53: the ACP Claude agent's own sign-in, which opens `/login` in a
/// terminal and stores a Claude subscription's credentials.
pub const CLAUDE_SUBSCRIPTION_AUTH_METHOD: &str = "claude-login";

/// ADR-F53: whether this fork restricts `agent_id` to an API key, so that the
/// card that would ask for a sign-in says where a subscription is used
/// instead.
pub fn is_api_key_only_agent(agent_id: &str) -> bool {
    agent_id == CLAUDE_AGENT_ID
}

/// ADR-F53: whether an authentication method of `agent_id` signs in with a
/// Claude subscription, which this route may not use.
///
/// The ACP Claude agent drives the Agent SDK from this IDE's harness, and a
/// Free / Pro / Max sign-in is scoped to Claude Code and Anthropic's own apps,
/// so the method is neither offered nor started here; an API key reaches the
/// agent through its `env` instead, needing no method at all. Anything that
/// reads as a sign-in rather than a key — the vendor may rename or add one —
/// is refused as well, so that a new version cannot quietly put a
/// subscription back on this route. Other agents are left alone: what their
/// vendors permit is their own terms' business.
pub fn is_subscription_auth_method(agent_id: &str, method_id: &str) -> bool {
    if !is_api_key_only_agent(agent_id) {
        return false;
    }
    method_id == CLAUDE_SUBSCRIPTION_AUTH_METHOD
        || method_id == "login"
        || method_id.ends_with("-login")
        || method_id.contains("oauth")
        || method_id.contains("subscription")
}

/// ADR-F53: whether the agent about to be started is pointed at **this**
/// machine's copy of the fork's own `CLAUDE_CONFIG_DIR`.
///
/// [`project::agent_sandbox::claude_acp_host_config_dir`] is a path on the
/// machine this IDE runs on, so it is named only for an agent that runs here.
/// One that runs on a host this project is connected to is pointed at that
/// host's own folder where its command is resolved, since a path of this
/// machine's would name nothing there.
fn points_at_this_machines_claude_folder(
    agent_id: &str,
    is_registry_agent: bool,
    runs_on_this_machine: bool,
) -> bool {
    agent_id == CLAUDE_AGENT_ID && is_registry_agent && runs_on_this_machine
}

/// A generic agent server implementation for custom user-defined agents
pub struct CustomAgentServer {
    agent_id: AgentId,
}

impl CustomAgentServer {
    pub fn new(agent_id: AgentId) -> Self {
        Self { agent_id }
    }
}

impl AgentServer for CustomAgentServer {
    fn agent_id(&self) -> AgentId {
        self.agent_id.clone()
    }

    fn logo(&self) -> IconName {
        IconName::Terminal
    }

    fn default_mode(&self, cx: &App) -> Option<acp::SessionModeId> {
        let settings = cx.read_global(|settings: &SettingsStore, _| {
            settings
                .get::<AllAgentServersSettings>(None)
                .get(self.agent_id().0.as_ref())
                .cloned()
        });

        settings
            .as_ref()
            .and_then(|s| s.default_mode().map(acp::SessionModeId::new))
    }

    fn favorite_config_option_value_ids(
        &self,
        config_id: &acp::SessionConfigId,
        cx: &mut App,
    ) -> HashSet<acp::SessionConfigValueId> {
        let settings = cx.read_global(|settings: &SettingsStore, _| {
            settings
                .get::<AllAgentServersSettings>(None)
                .get(self.agent_id().0.as_ref())
                .cloned()
        });

        settings
            .as_ref()
            .and_then(|s| s.favorite_config_option_values(config_id.0.as_ref()))
            .map(|values| {
                values
                    .iter()
                    .cloned()
                    .map(acp::SessionConfigValueId::new)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn toggle_favorite_config_option_value(
        &self,
        config_id: acp::SessionConfigId,
        value_id: acp::SessionConfigValueId,
        should_be_favorite: bool,
        fs: Arc<dyn Fs>,
        cx: &App,
    ) {
        let agent_id = self.agent_id();
        let config_id = config_id.to_string();
        let value_id = value_id.to_string();

        update_settings_file(fs, cx, move |settings, _cx| {
            let settings = settings
                .agent_servers
                .get_or_insert_default()
                .entry(agent_id.0.to_string())
                .or_insert_with(default_settings_for_agent);

            match settings {
                settings::CustomAgentServerSettings::Custom {
                    favorite_config_option_values,
                    ..
                }
                | settings::CustomAgentServerSettings::Registry {
                    favorite_config_option_values,
                    ..
                } => {
                    let entry = favorite_config_option_values
                        .entry(config_id.clone())
                        .or_insert_with(Vec::new);

                    if should_be_favorite {
                        if !entry.iter().any(|v| v == &value_id) {
                            entry.push(value_id.clone());
                        }
                    } else {
                        entry.retain(|v| v != &value_id);
                        if entry.is_empty() {
                            favorite_config_option_values.remove(&config_id);
                        }
                    }
                }
            }
        });
    }

    fn set_default_mode(&self, mode_id: Option<acp::SessionModeId>, fs: Arc<dyn Fs>, cx: &mut App) {
        let agent_id = self.agent_id();
        update_settings_file(fs, cx, move |settings, _cx| {
            let settings = settings
                .agent_servers
                .get_or_insert_default()
                .entry(agent_id.0.to_string())
                .or_insert_with(default_settings_for_agent);

            match settings {
                settings::CustomAgentServerSettings::Custom { default_mode, .. }
                | settings::CustomAgentServerSettings::Registry { default_mode, .. } => {
                    *default_mode = mode_id.map(|m| m.to_string());
                }
            }
        });
    }

    fn default_config_option(&self, config_id: &str, cx: &App) -> Option<AgentConfigOptionValue> {
        let settings = cx.read_global(|settings: &SettingsStore, _| {
            settings
                .get::<AllAgentServersSettings>(None)
                .get(self.agent_id().as_ref())
                .cloned()
        });

        settings
            .as_ref()
            .and_then(|s| s.default_config_option(config_id).cloned())
    }

    fn set_default_config_option(
        &self,
        config_id: &str,
        value: Option<AgentConfigOptionValue>,
        fs: Arc<dyn Fs>,
        cx: &mut App,
    ) {
        let agent_id = self.agent_id();
        let config_id = config_id.to_string();
        update_settings_file(fs, cx, move |settings, _cx| {
            let settings = settings
                .agent_servers
                .get_or_insert_default()
                .entry(agent_id.0.to_string())
                .or_insert_with(default_settings_for_agent);

            match settings {
                settings::CustomAgentServerSettings::Custom {
                    default_config_options,
                    ..
                }
                | settings::CustomAgentServerSettings::Registry {
                    default_config_options,
                    ..
                } => {
                    if let Some(value) = value {
                        default_config_options.insert(config_id.clone(), value);
                    } else {
                        default_config_options.remove(&config_id);
                    }
                }
            }
        });
    }

    fn connect(
        &self,
        delegate: AgentServerDelegate,
        project: Entity<Project>,
        cx: &mut App,
    ) -> Task<Result<Rc<dyn AgentConnection>>> {
        let agent_id = self.agent_id();
        let default_mode = self.default_mode(cx);
        let is_registry_agent = is_registry_agent(agent_id.clone(), cx);
        let default_config_options = cx.read_global(|settings: &SettingsStore, _| {
            settings
                .get::<AllAgentServersSettings>(None)
                .get(self.agent_id().as_ref())
                .map(|s| match s {
                    project::agent_server_store::CustomAgentServerSettings::Custom {
                        default_config_options,
                        ..
                    }
                    | project::agent_server_store::CustomAgentServerSettings::Registry {
                        default_config_options,
                        ..
                    } => default_config_options.clone(),
                })
                .unwrap_or_default()
        });

        if is_registry_agent {
            if let Some(registry_store) = project::AgentRegistryStore::try_global(cx) {
                registry_store.update(cx, |store, cx| store.refresh_if_stale(cx));
            }
        }

        let mut extra_env = load_proxy_env(cx);
        if delegate.store.read(cx).no_browser() {
            extra_env.insert("NO_BROWSER".to_owned(), "1".to_owned());
        }
        let claude_takes_this_machines_folder = points_at_this_machines_claude_folder(
            agent_id.as_ref(),
            is_registry_agent,
            !project.read(cx).is_via_remote_server(),
        );
        let fs = project.read(cx).fs().clone();
        if is_registry_agent {
            match agent_id.as_ref() {
                // ADR-F53: upstream blanked `ANTHROPIC_API_KEY` here to push
                // the agent onto its OAuth sign-in. This route is API-key
                // only, so the key is left alone — the sandbox builds its
                // environment from nothing anyway, and there the key comes
                // from the agent's settings.
                //
                // The folder is the fork's own even outside the sandbox, so
                // that a subscription the official CLI is signed in with is
                // not picked up here. The sandbox lays its per-project folder
                // over this (`profile.environment`), and the agent's own
                // settings override both. An agent on a host this project is
                // connected to is named its folder there, by the host that
                // resolves its command.
                CLAUDE_AGENT_ID if claude_takes_this_machines_folder => {
                    extra_env.insert(
                        "CLAUDE_CONFIG_DIR".into(),
                        project::agent_sandbox::claude_acp_host_config_dir()
                            .to_string_lossy()
                            .into_owned(),
                    );
                }
                CODEX_ID => {
                    if let Ok(api_key) = std::env::var("CODEX_API_KEY") {
                        extra_env.insert("CODEX_API_KEY".into(), api_key);
                    }
                    if let Ok(api_key) = std::env::var("OPEN_AI_API_KEY") {
                        extra_env.insert("OPEN_AI_API_KEY".into(), api_key);
                    }
                }
                GEMINI_ID => {
                    extra_env.insert("SURFACE".to_owned(), "zed".to_owned());
                }
                _ => {}
            }
        }
        let store = delegate.store.downgrade();
        let run_outside_sandbox = delegate.run_outside_sandbox;
        // ADR-F49 第 2b 段: on Windows the sandbox is the WSL distro of the
        // project's folders, so the command is resolved for it.
        let launch_target = if run_outside_sandbox {
            AgentLaunchTarget::Host
        } else {
            let roots = project::agent_sandbox::project_key_roots(
                project.read(cx).worktree_store().read(cx),
                cx,
            );
            match project::agent_sandbox::wsl_distro_for_roots(&roots) {
                Ok(wsl_distro) => AgentLaunchTarget::Sandbox { wsl_distro },
                Err(error) => return Task::ready(Err(error)),
            }
        };
        cx.spawn(async move |cx| {
            if is_registry_agent && agent_id.as_ref() == GEMINI_ID {
                if let Some(api_key) = cx.update(api_key_for_gemini_cli).await.ok() {
                    extra_env.insert("GEMINI_API_KEY".into(), api_key);
                }
            }
            // The agent looks its configuration folder up rather than creating
            // it in every version, so the folder it is pointed at is made
            // here. Failing to make it is not fatal: the agent still starts,
            // and its own message says what it could not write.
            if claude_takes_this_machines_folder {
                fs.create_dir(&project::agent_sandbox::claude_acp_host_config_dir())
                    .await
                    .log_err();
            }
            let command = store
                .update(cx, |store, cx| {
                    let agent = store.get_external_agent(&agent_id).with_context(|| {
                        format!("Custom agent server `{}` is not registered", agent_id)
                    })?;
                    if let Some(new_version_available_tx) = delegate.new_version_available {
                        agent.set_new_version_available_tx(new_version_available_tx);
                    }
                    if let Some(loading_status_tx) = delegate.loading_status {
                        agent.set_loading_status_tx(loading_status_tx);
                    }
                    anyhow::Ok(agent.get_command(
                        vec![],
                        extra_env,
                        launch_target,
                        &mut cx.to_async(),
                    ))
                })??
                .await?;
            let connection = crate::acp::connect(
                agent_id,
                project,
                command,
                store.clone(),
                default_mode,
                default_config_options,
                run_outside_sandbox,
                cx,
            )
            .await?;
            Ok(connection)
        })
    }

    fn into_any(self: Rc<Self>) -> Rc<dyn std::any::Any> {
        self
    }
}

fn api_key_for_gemini_cli(cx: &mut App) -> Task<Result<String>> {
    let env_var = EnvVar::new("GEMINI_API_KEY".into()).or(EnvVar::new("GOOGLE_AI_API_KEY".into()));
    if let Some(key) = env_var.value {
        return Task::ready(Ok(key));
    }
    let credentials_provider = zed_credentials_provider::global(cx);
    let api_url = google_ai::API_URL.to_string();
    cx.spawn(async move |cx| {
        Ok(
            ApiKey::load_from_system_keychain(&api_url, credentials_provider.as_ref(), cx)
                .await?
                .key()
                .to_string(),
        )
    })
}

fn is_registry_agent(agent_id: impl Into<AgentId>, cx: &App) -> bool {
    let agent_id = agent_id.into();
    let is_in_registry = project::AgentRegistryStore::try_global(cx)
        .map(|store| store.read(cx).agent(&agent_id).is_some())
        .unwrap_or(false);
    let is_settings_registry = cx.read_global(|settings: &SettingsStore, _| {
        settings
            .get::<AllAgentServersSettings>(None)
            .get(agent_id.as_ref())
            .is_some_and(|s| {
                matches!(
                    s,
                    project::agent_server_store::CustomAgentServerSettings::Registry { .. }
                )
            })
    });
    is_in_registry || is_settings_registry
}

fn default_settings_for_agent() -> settings::CustomAgentServerSettings {
    settings::CustomAgentServerSettings::Registry {
        default_mode: None,
        env: Default::default(),
        default_config_options: Default::default(),
        favorite_config_option_values: Default::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use collections::HashMap;
    use gpui::TestAppContext;
    use project::agent_registry_store::{
        AgentRegistryStore, RegistryAgent, RegistryAgentMetadata, RegistryNpxAgent,
    };
    use settings::Settings as _;
    use ui::SharedString;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
    }

    fn init_registry_with_agents(cx: &mut TestAppContext, agent_ids: &[&str]) {
        let agents: Vec<RegistryAgent> = agent_ids
            .iter()
            .map(|id| {
                let id = SharedString::from(id.to_string());
                RegistryAgent::Npx(RegistryNpxAgent {
                    metadata: RegistryAgentMetadata {
                        id: AgentId::new(id.clone()),
                        name: id.clone(),
                        description: SharedString::from(""),
                        version: SharedString::from("1.0.0"),
                        repository: None,
                        website: None,
                        icon_path: None,
                    },
                    package: id,
                    args: Vec::new(),
                    env: HashMap::default(),
                })
            })
            .collect();
        cx.update(|cx| {
            AgentRegistryStore::init_test_global(cx, agents);
        });
    }

    fn set_agent_server_settings(
        cx: &mut TestAppContext,
        entries: Vec<(&str, settings::CustomAgentServerSettings)>,
    ) {
        cx.update(|cx| {
            AllAgentServersSettings::override_global(
                project::agent_server_store::AllAgentServersSettings(
                    entries
                        .into_iter()
                        .map(|(name, settings)| (name.to_string(), settings.into()))
                        .collect(),
                ),
                cx,
            );
        });
    }

    /// ADR-F53: the ACP Claude agent's sign-in stores a Claude subscription's
    /// credentials for the Agent SDK, which this route may not use, so it is
    /// neither offered nor started. Everything else — the same agent's other
    /// methods, and every other agent's — is left as the vendor advertises it.
    #[test]
    fn only_the_acp_claudes_subscription_sign_in_is_refused() {
        assert!(is_subscription_auth_method(
            CLAUDE_AGENT_ID,
            CLAUDE_SUBSCRIPTION_AUTH_METHOD
        ));
        for renamed in ["login", "claude-code-login", "claude-oauth", "subscription"] {
            assert!(
                is_subscription_auth_method(CLAUDE_AGENT_ID, renamed),
                "a sign-in under another name is refused too: {renamed}"
            );
        }
        for kept in ["anthropic-api-key", "api-key", "bedrock", "vertex"] {
            assert!(
                !is_subscription_auth_method(CLAUDE_AGENT_ID, kept),
                "an API-key method is what this route is for: {kept}"
            );
        }
        for other_agent in [CODEX_ID, GEMINI_ID, CURSOR_ID, "my-custom-agent"] {
            assert!(
                !is_subscription_auth_method(other_agent, CLAUDE_SUBSCRIPTION_AUTH_METHOD),
                "another vendor's terms are its own business: {other_agent}"
            );
        }
    }

    /// ADR-F53: the folder that keeps the official CLI's sign-in off this
    /// route is a path on the machine the agent runs on, so this side names it
    /// only for an agent it starts itself. A project on a host it connects to
    /// is left to that host, which names its own folder.
    #[test]
    fn only_an_acp_claude_on_this_machine_takes_this_machines_folder() {
        assert!(points_at_this_machines_claude_folder(
            CLAUDE_AGENT_ID,
            true,
            true
        ));
        assert!(
            !points_at_this_machines_claude_folder(CLAUDE_AGENT_ID, true, false),
            "a path of this machine's would name nothing on the host the agent runs on"
        );
        assert!(
            !points_at_this_machines_claude_folder(CLAUDE_AGENT_ID, false, true),
            "a command the user configured under the same name is not the registry's agent"
        );
        for other_agent in [CODEX_ID, GEMINI_ID, CURSOR_ID] {
            assert!(
                !points_at_this_machines_claude_folder(other_agent, true, true),
                "another agent keeps its own folder: {other_agent}"
            );
        }
    }

    #[gpui::test]
    fn test_unknown_agent_is_not_registry(cx: &mut TestAppContext) {
        init_test(cx);
        cx.update(|cx| {
            assert!(!is_registry_agent("my-custom-agent", cx));
        });
    }

    #[gpui::test]
    fn test_agent_in_registry_store_is_registry(cx: &mut TestAppContext) {
        init_test(cx);
        init_registry_with_agents(cx, &["some-new-registry-agent"]);
        cx.update(|cx| {
            assert!(is_registry_agent("some-new-registry-agent", cx));
            assert!(!is_registry_agent("not-in-registry", cx));
        });
    }

    #[gpui::test]
    fn test_agent_with_registry_settings_type_is_registry(cx: &mut TestAppContext) {
        init_test(cx);
        set_agent_server_settings(
            cx,
            vec![(
                "agent-from-settings",
                settings::CustomAgentServerSettings::Registry {
                    env: HashMap::default(),
                    default_mode: None,
                    default_config_options: HashMap::default(),
                    favorite_config_option_values: HashMap::default(),
                },
            )],
        );
        cx.update(|cx| {
            assert!(is_registry_agent("agent-from-settings", cx));
        });
    }
}
