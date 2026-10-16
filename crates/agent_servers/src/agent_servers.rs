mod acp;
pub mod acp_paths;
mod custom;

#[cfg(any(test, feature = "test-support"))]
pub mod e2e_tests;

use client::ProxySettings;
use collections::{HashMap, HashSet};
pub use custom::*;
use fs::Fs;
use http_client::read_no_proxy_from_env;
use project::{AgentId, Project, agent_server_store::AgentServerStore};

use acp_thread::AgentConnection;
use agent_client_protocol::schema::v1 as acp_schema;
use anyhow::Result;
use gpui::{App, AppContext, Entity, Task};
use settings::{AgentConfigOptionValue, SettingsStore};
use std::{any::Any, rc::Rc, sync::Arc};

#[cfg(any(test, feature = "test-support"))]
pub use acp::test_support::{
    FakeAcpAgentServer, FakeAcpConnectionHarness, connect_fake_acp_connection,
};
pub use acp::{
    AcpConnection, AcpDebugMessage, AcpDebugMessageContent, AcpDebugMessageDirection,
    GEMINI_TERMINAL_AUTH_METHOD_ID, McpOffer, SessionMcp, prelaunch_mcp_offer,
};

pub struct AgentServerDelegate {
    store: Entity<AgentServerStore>,
    new_version_available: Option<watch::Sender<Option<String>>>,
    loading_status: Option<watch::Sender<Option<String>>>,
    run_outside_sandbox: bool,
    keep_in_sandbox: bool,
    lift_agent_hosts: bool,
}

impl AgentServerDelegate {
    pub fn new(
        store: Entity<AgentServerStore>,
        new_version_tx: Option<watch::Sender<Option<String>>>,
        loading_status_tx: Option<watch::Sender<Option<String>>>,
    ) -> Self {
        Self {
            store,
            new_version_available: new_version_tx,
            loading_status: loading_status_tx,
            run_outside_sandbox: false,
            keep_in_sandbox: false,
            lift_agent_hosts: false,
        }
    }

    /// ADR-F49 decision 19 (8c): this start stays in the sandbox even where
    /// the user chose to start the agent outside every time — a thread whose
    /// session was started in the sandbox is read back where it was started,
    /// since the folder outside does not have it.
    pub fn in_sandbox_whatever_the_settings(mut self) -> Self {
        self.keep_in_sandbox = true;
        self
    }

    /// ADR-F49 decision 19: whether `agent_id` starts outside its sandbox on
    /// this delegate — the one start the user confirmed, or, unless this start
    /// is to stay in the sandbox, every start the user's own settings let out.
    /// Neither while running outside the sandbox is not allowed (ADR-F92).
    pub fn starts_outside_sandbox(&self, agent_id: &str, cx: &App) -> bool {
        (self.run_outside_sandbox && project::agent_sandbox::outside_sandbox_allowed(cx))
            || (!self.keep_in_sandbox && custom::runs_outside_sandbox_by_user_choice(agent_id, cx))
    }

    /// ADR-F49 decision 19: for the one start the user confirmed, the agent
    /// runs outside its sandbox.
    pub fn outside_sandbox_once(mut self) -> Self {
        self.run_outside_sandbox = true;
        self
    }

    pub fn runs_outside_sandbox(&self) -> bool {
        self.run_outside_sandbox
    }

    /// ADR-F96 stage 3: this start's sandbox lets the agent reach any host the
    /// editor-wide allowlist permits, rather than only its own hosts. The
    /// allowlist still caps it, and without one published nothing is lifted.
    pub fn lift_agent_hosts(mut self) -> Self {
        self.lift_agent_hosts = true;
        self
    }

    /// Whether this start lifts the agent's own hosts: asked for, and still
    /// allowed by the user's settings (`allow_unrestricted_network`), which a
    /// start asked for earlier does not outlive.
    pub fn lifts_agent_hosts(&self, cx: &App) -> bool {
        self.lift_agent_hosts && project::agent_sandbox::unrestricted_network_allowed(cx)
    }

    /// What reports a newer version of the agent, for a test to report one
    /// with.
    #[cfg(any(test, feature = "test-support"))]
    pub fn take_new_version_sender(&mut self) -> Option<watch::Sender<Option<String>>> {
        self.new_version_available.take()
    }
}

pub trait AgentServer: Send {
    fn logo(&self) -> ui::IconName;
    fn agent_id(&self) -> AgentId;
    fn connect(
        &self,
        delegate: AgentServerDelegate,
        project: Entity<Project>,
        cx: &mut App,
    ) -> Task<Result<Rc<dyn AgentConnection>>>;

    fn into_any(self: Rc<Self>) -> Rc<dyn Any>;

    /// ADR-F99: whether a new thread of this agent waits for its first message
    /// before starting the agent, so that the sandbox and network it starts
    /// with can be picked first. Only an agent whose whole process the sandbox
    /// holds has anything to pick before it starts.
    fn starts_on_first_message(&self) -> bool {
        false
    }

    /// ADR-F103: whether what a session of this agent offers (its model and
    /// other options, its modes and slash commands) may be read in a session
    /// of its own ([`AgentConnection::new_session_to_read_options`]) before
    /// the agent's first thread has started, so that a new thread can offer
    /// them from the very start.
    fn reads_session_options_before_start(&self) -> bool {
        false
    }

    fn default_mode(&self, _cx: &App) -> Option<acp_schema::SessionModeId> {
        None
    }

    fn set_default_mode(
        &self,
        _mode_id: Option<acp_schema::SessionModeId>,
        _fs: Arc<dyn Fs>,
        _cx: &mut App,
    ) {
    }

    fn default_config_option(&self, _config_id: &str, _cx: &App) -> Option<AgentConfigOptionValue> {
        None
    }

    fn set_default_config_option(
        &self,
        _config_id: &str,
        _value: Option<AgentConfigOptionValue>,
        _fs: Arc<dyn Fs>,
        _cx: &mut App,
    ) {
    }

    fn favorite_config_option_value_ids(
        &self,
        _config_id: &acp_schema::SessionConfigId,
        _cx: &mut App,
    ) -> HashSet<acp_schema::SessionConfigValueId> {
        HashSet::default()
    }

    fn toggle_favorite_config_option_value(
        &self,
        _config_id: acp_schema::SessionConfigId,
        _value_id: acp_schema::SessionConfigValueId,
        _should_be_favorite: bool,
        _fs: Arc<dyn Fs>,
        _cx: &App,
    ) {
    }
}

impl dyn AgentServer {
    pub fn downcast<T: 'static + AgentServer + Sized>(self: Rc<Self>) -> Option<Rc<T>> {
        self.into_any().downcast().ok()
    }
}

/// ADR-F49: the proxy an agent in a sandbox has to go through beyond the
/// sandbox's own, which chains to it — Zed's `proxy` setting, else this
/// process's proxy variables — as the variables the sandbox reads it from.
pub(crate) fn upstream_proxy_env(cx: &App) -> HashMap<String, String> {
    let mut env = HashMap::default();
    let proxy_url = cx
        .read_global(|settings: &SettingsStore, _| settings.get::<ProxySettings>(None).proxy_url());
    if let Some(proxy_url) = proxy_url {
        let name = if proxy_url.scheme() == "https" {
            "HTTPS_PROXY"
        } else {
            "HTTP_PROXY"
        };
        env.insert(name.to_owned(), proxy_url.to_string());
        if let Some(no_proxy) = read_no_proxy_from_env() {
            env.insert("NO_PROXY".to_owned(), no_proxy);
        }
        return env;
    }
    for name in [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
        "NO_PROXY",
        "no_proxy",
    ] {
        if let Ok(value) = std::env::var(name) {
            env.insert(name.to_owned(), value);
        }
    }
    env
}

/// Load the default proxy environment variables to pass through to the agent
pub fn load_proxy_env(cx: &mut App) -> HashMap<String, String> {
    let mut env = HashMap::default();

    // ADR-F13 (F2-7): when the fork's child-process egress proxy is running,
    // Zed's own `proxy` setting is deliberately *not* propagated here. The
    // agent is pointed at the fork's proxy instead — by `AcpConnection::stdio`,
    // once the agent's own settings have had their say — and that proxy chains
    // through the user's rather than around it, so nothing is lost. Doing it
    // there rather than here is what keeps a proxy the user spelled out in
    // `agent_servers.<name>.env` winning: this map is merged *over* a custom
    // agent's `env`, whereas the injection downstream only fills a gap.
    if http_client::child_process_proxy().is_some() {
        return env;
    }

    let proxy_url = cx
        .read_global(|settings: &SettingsStore, _| settings.get::<ProxySettings>(None).proxy_url());

    if let Some(proxy_url) = &proxy_url {
        let env_var = if proxy_url.scheme() == "https" {
            "HTTPS_PROXY"
        } else {
            "HTTP_PROXY"
        };
        env.insert(env_var.to_owned(), proxy_url.to_string());
    }

    if let Some(no_proxy) = read_no_proxy_from_env() {
        env.insert("NO_PROXY".to_owned(), no_proxy);
    } else if proxy_url.is_some() {
        // We sometimes need local MCP servers that we don't want to proxy
        env.insert("NO_PROXY".to_owned(), "localhost,127.0.0.1".to_owned());
    }

    env
}
