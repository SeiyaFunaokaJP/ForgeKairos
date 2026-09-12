use anyhow::{Context as _, Result, anyhow};
use askpass::{AskPassDelegate, AskPassSession, EncryptedPassword};
use client::ProjectId;
use collections::HashMap;
use collections::HashSet;
use gpui::TasksIncluded;
use language::File;
use lsp::LanguageServerId;

use client::ProxySettings;
use egress_policy::child_process_proxy::Supervisor as ChildProcessProxySupervisor;
use extension::ExtensionHostProxy;
use extension_host::headless_host::HeadlessExtensionStore;
use fs::{Fs, RemoveOptions, RenameOptions};
use futures::channel::oneshot;
use gpui::{
    App, AppContext as _, AsyncApp, Context, Entity, EventEmitter, PromptLevel, Task, TaskExt,
    WeakEntity,
};
use http_client::{EgressPolicy, EgressPolicyHandle, HttpClient};
use language::{Buffer, BufferEvent, LanguageRegistry, proto::serialize_operation};
use node_runtime::NodeRuntime;
use project::{
    AgentRegistryStore, LspStore, LspStoreEvent, ManifestTree, PrettierStore, ProjectEnvironment,
    ProjectPath, ToolchainStore, WorktreeId,
    agent_server_store::AgentServerStore,
    buffer_store::{BufferStore, BufferStoreEvent},
    context_server_store::ContextServerStore,
    debugger::{breakpoint_store::BreakpointStore, dap_store::DapStore},
    git_store::GitStore,
    image_store::ImageId,
    lsp_store::log_store::{self, GlobalLogStore, LanguageServerKind, LogKind},
    project_settings::SettingsObserver,
    search::SearchQuery,
    task_store::TaskStore,
    trusted_worktrees::{PathTrust, RemoteHostLocation, TrustedWorktrees},
    worktree_store::{WorktreeIdCounter, WorktreeStore},
};
use rpc::{
    AnyProtoClient, TypedEnvelope,
    proto::{self, REMOTE_SERVER_PEER_ID, REMOTE_SERVER_PROJECT_ID},
};
use settings::Settings as _;
use smol::process::Child;

use settings::initial_server_settings_content;
use std::{
    cell::RefCell,
    num::NonZeroU64,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Instant,
};
use sysinfo::{ProcessRefreshKind, RefreshKind, System, UpdateKind};
use util::{ResultExt, paths::PathStyle, rel_path::RelPath};
use worktree::Worktree;
use zeroize::Zeroize as _;

pub struct HeadlessProject {
    pub fs: Arc<dyn Fs>,
    pub session: AnyProtoClient,
    pub worktree_store: Entity<WorktreeStore>,
    pub buffer_store: Entity<BufferStore>,
    pub lsp_store: Entity<LspStore>,
    pub task_store: Entity<TaskStore>,
    pub dap_store: Entity<DapStore>,
    pub breakpoint_store: Entity<BreakpointStore>,
    pub agent_server_store: Entity<AgentServerStore>,
    pub context_server_store: Entity<ContextServerStore>,
    pub settings_observer: Entity<SettingsObserver>,
    /// The allowlist gating this process's own `HttpClient`, pushed by the
    /// connecting IDE right after the connection is established (ADR-F11).
    /// `None` when the process's `HttpClient` isn't an `AllowlistedHttpClient`
    /// (e.g. `BlockedHttpClient` in tests), in which case a pushed allowlist
    /// has nothing to update and is accepted but ignored.
    pub egress_policy: Option<EgressPolicyHandle>,
    /// The CONNECT proxy this host's own agent child processes — ACP agents,
    /// MCP servers, npm — are pointed at, so that `egress_policy` governs
    /// their traffic too (ADR-F14, the remote half of ADR-F13). It listens on
    /// *this* host's loopback: the IDE's endpoint would mean a different
    /// machine's port here. Started when the client pushes its allowlist,
    /// since until then this process's policy denies everything anyway.
    ///
    /// Shared with the [`HeadlessHost`] rather than owned: the endpoint is
    /// process-wide, so it has to outlive any one project (ADR-F45).
    pub(crate) child_process_proxy: Option<Rc<RefCell<ChildProcessProxySupervisor>>>,
    pub next_entry_id: Arc<AtomicUsize>,
    pub languages: Arc<LanguageRegistry>,
    pub extensions: Entity<HeadlessExtensionStore>,
    pub git_store: Entity<GitStore>,
    pub environment: Entity<ProjectEnvironment>,
    pub profiling_collector: gpui::ProfilingCollector,
    // Used mostly to keep alive the toolchain store for RPC handlers.
    // Local variant is used within LSP store, but that's a separate entity.
    pub _toolchain_store: Entity<ToolchainStore>,
    pub kernels: HashMap<String, Child>,
    /// ADR-F28: the transfer commands (`sftp` / `scp` / `curl`) this host is
    /// running for the IDE's file transfer screen. Sending on the channel
    /// makes the handler drop its child, which `kill_on_drop` then kills — the
    /// child itself cannot live here, because awaiting its output consumes it.
    transfer_commands: HashMap<u64, oneshot::Sender<()>>,
    pub host: Entity<HeadlessHost>,
    /// Whether `ShutdownRemoteServer` ends the whole process, which is only
    /// right when this project is the only thing the process serves.
    quit_app_on_shutdown: bool,
}

pub struct HeadlessAppState {
    pub session: AnyProtoClient,
    pub fs: Arc<dyn Fs>,
    pub http_client: Arc<dyn HttpClient>,
    pub node_runtime: NodeRuntime,
    pub languages: Arc<LanguageRegistry>,
    pub extension_host_proxy: Arc<ExtensionHostProxy>,
    pub startup_time: Instant,
}

pub struct HeadlessHostState {
    pub fs: Arc<dyn Fs>,
    pub http_client: Arc<dyn HttpClient>,
    pub node_runtime: NodeRuntime,
    pub languages: Arc<LanguageRegistry>,
    pub extension_host_proxy: Arc<ExtensionHostProxy>,
    pub startup_time: Instant,
}

/// What one `remote_server` process has once, however many projects it serves
/// (ADR-F45).
///
/// Everything here is either process-wide already (the extension proxies, the
/// agent registry global, the child-process proxy endpoint) or has to be
/// shared for the process-wide pieces to stay consistent: language server ids
/// come from the language registry and key the global LSP log store, so two
/// registries in one process would hand out the same id twice.
pub struct HeadlessHost {
    pub fs: Arc<dyn Fs>,
    pub http_client: Arc<dyn HttpClient>,
    pub node_runtime: NodeRuntime,
    pub languages: Arc<LanguageRegistry>,
    pub extensions: Entity<HeadlessExtensionStore>,
    /// The allowlist gating this process's own `HttpClient` (ADR-F11); see
    /// [`HeadlessProject::egress_policy`].
    pub egress_policy: Option<EgressPolicyHandle>,
    child_process_proxy: Option<Rc<RefCell<ChildProcessProxySupervisor>>>,
    startup_time: Instant,
    projects: Vec<WeakEntity<HeadlessProject>>,
}

impl HeadlessHost {
    pub fn new(
        HeadlessHostState {
            fs,
            http_client,
            node_runtime,
            languages,
            extension_host_proxy: proxy,
            startup_time,
        }: HeadlessHostState,
        cx: &mut Context<Self>,
    ) -> Self {
        let egress_policy = http_client.egress_policy().cloned();
        // Only worth a supervisor when there is a policy for it to enforce:
        // without one (a test harness whose client isn't allowlisted) a
        // listener would bind a port and judge nothing.
        let child_process_proxy = egress_policy.as_ref().map(|_| {
            // `try_get` rather than `get_global`: the settings type is
            // registered on the real startup path, but a test that builds a
            // `HeadlessProject` directly need not have registered it, and
            // chaining through the user's proxy is a convenience that must not
            // cost the process its ability to start.
            let upstream_proxy = ProxySettings::try_get(cx).and_then(ProxySettings::proxy_url);
            Rc::new(RefCell::new(ChildProcessProxySupervisor::new(
                upstream_proxy.as_ref(),
            )))
        });

        debug_adapter_extension::init(proxy.clone(), cx);
        languages::init(languages.clone(), fs.clone(), node_runtime.clone(), cx);

        // Deliberately *not* on the IDE lane (ADR-F38): the ACP registry and
        // the agent binaries it points at are AI infrastructure, and the local
        // side keeps them gated too. The allowlist the IDE pushed (ADR-F11) is
        // what they are judged against.
        AgentRegistryStore::init_global(cx, fs.clone(), http_client.clone());

        // The extension proxies are process-wide slots, so they have to reach
        // every project's language servers, not the last project's.
        let host = cx.weak_entity();
        language_extension::init(
            language_extension::LspAccess::ViaWorkspaces(Arc::new(move |cx: &mut App| {
                let host = host
                    .upgrade()
                    .context("the remote server's shared state has been released")?;
                Ok(host.read(cx).live_lsp_stores(cx))
            })),
            proxy.clone(),
            languages.clone(),
        );

        let extensions = HeadlessExtensionStore::new(
            fs.clone(),
            // ADR-F38: extensions are how most language servers arrive.
            http_client::ide_lane(http_client.clone()),
            paths::remote_extensions_dir().to_path_buf(),
            proxy,
            node_runtime.clone(),
            cx,
        );

        Self {
            fs,
            http_client,
            node_runtime,
            languages,
            extensions,
            egress_policy,
            child_process_proxy,
            startup_time,
            projects: Vec::new(),
        }
    }

    fn register_project(&mut self, project: WeakEntity<HeadlessProject>) {
        self.projects.retain(|project| project.upgrade().is_some());
        self.projects.push(project);
    }

    /// The language servers of every project this process still serves.
    pub fn live_lsp_stores(&self, cx: &App) -> Vec<Entity<LspStore>> {
        self.projects
            .iter()
            .filter_map(WeakEntity::upgrade)
            .map(|project| project.read(cx).lsp_store.clone())
            .collect()
    }

    /// Whether this process runs the child-process egress proxy's supervisor.
    pub fn has_child_process_proxy_supervisor(&self) -> bool {
        self.child_process_proxy.is_some()
    }
}

/// What a project tells whoever owns it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadlessProjectEvent {
    /// The client closed this project (`ShutdownRemoteServer`). The process
    /// serves other projects too (ADR-F45), so this ends the project, not the
    /// process.
    ShutdownRequested,
}

impl EventEmitter<HeadlessProjectEvent> for HeadlessProject {}

impl HeadlessProject {
    pub fn init(cx: &mut App) {
        settings::init(cx);
        log_store::init(true, cx);
    }

    /// A project that is the only thing its process serves: it brings its own
    /// [`HeadlessHost`], and closing it ends the process.
    pub fn new(
        HeadlessAppState {
            session,
            fs,
            http_client,
            node_runtime,
            languages,
            extension_host_proxy,
            startup_time,
        }: HeadlessAppState,
        init_worktree_trust: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let host = cx.new(|cx| {
            HeadlessHost::new(
                HeadlessHostState {
                    fs,
                    http_client,
                    node_runtime,
                    languages,
                    extension_host_proxy,
                    startup_time,
                },
                cx,
            )
        });
        let mut project = Self::new_in_host(host, session, init_worktree_trust, cx);
        project.quit_app_on_shutdown = true;
        project
    }

    /// A project that shares its process with others (ADR-F45). Closing it
    /// emits [`HeadlessProjectEvent::ShutdownRequested`] for the owner to act
    /// on, instead of ending the process.
    pub fn new_in_host(
        host: Entity<HeadlessHost>,
        session: AnyProtoClient,
        init_worktree_trust: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let (
            fs,
            http_client,
            node_runtime,
            languages,
            extensions,
            egress_policy,
            child_process_proxy,
            startup_time,
        ) = {
            let host = host.read(cx);
            (
                host.fs.clone(),
                host.http_client.clone(),
                host.node_runtime.clone(),
                host.languages.clone(),
                host.extensions.clone(),
                host.egress_policy.clone(),
                host.child_process_proxy.clone(),
                host.startup_time,
            )
        };
        let this = cx.weak_entity();
        host.update(cx, |host, _| host.register_project(this));

        let worktree_store = cx.new(|cx| {
            let mut store = WorktreeStore::local(true, fs.clone(), WorktreeIdCounter::get(cx));
            store.shared(REMOTE_SERVER_PROJECT_ID, session.clone(), cx);
            store
        });

        if init_worktree_trust {
            project::trusted_worktrees::track_worktree_trust(
                worktree_store.clone(),
                None::<RemoteHostLocation>,
                Some((session.clone(), ProjectId(REMOTE_SERVER_PROJECT_ID))),
                None,
                cx,
            );
        }

        let environment =
            cx.new(|cx| ProjectEnvironment::new(None, worktree_store.downgrade(), None, true, cx));
        let manifest_tree = ManifestTree::new(worktree_store.clone(), cx);
        let toolchain_store = cx.new(|cx| {
            ToolchainStore::local(
                languages.clone(),
                worktree_store.clone(),
                environment.clone(),
                manifest_tree.clone(),
                cx,
            )
        });

        let buffer_store = cx.new(|cx| {
            let mut buffer_store = BufferStore::local(worktree_store.clone(), cx);
            buffer_store.shared(REMOTE_SERVER_PROJECT_ID, session.clone(), cx);
            buffer_store
        });

        let breakpoint_store = cx.new(|_| {
            let mut breakpoint_store =
                BreakpointStore::local(worktree_store.clone(), buffer_store.clone());
            breakpoint_store.shared(REMOTE_SERVER_PROJECT_ID, session.clone());

            breakpoint_store
        });

        let dap_store = cx.new(|cx| {
            let mut dap_store = DapStore::new_local(
                // ADR-F38: debug adapters are IDE tooling here too.
                http_client::ide_lane(http_client.clone()),
                node_runtime.clone(),
                fs.clone(),
                environment.clone(),
                toolchain_store.read(cx).as_language_toolchain_store(),
                worktree_store.clone(),
                breakpoint_store.clone(),
                true,
                cx,
            );
            dap_store.shared(REMOTE_SERVER_PROJECT_ID, session.clone(), cx);
            dap_store
        });

        let git_store = cx.new(|cx| {
            let mut store = GitStore::local(
                &worktree_store,
                buffer_store.clone(),
                environment.clone(),
                fs.clone(),
                cx,
            );
            store.shared(REMOTE_SERVER_PROJECT_ID, session.clone(), cx);
            store
        });

        let prettier_store = cx.new(|cx| {
            PrettierStore::new(
                node_runtime.clone(),
                fs.clone(),
                languages.clone(),
                worktree_store.clone(),
                cx,
            )
        });

        let task_store = cx.new(|cx| {
            let mut task_store = TaskStore::local(
                buffer_store.downgrade(),
                worktree_store.clone(),
                toolchain_store.read(cx).as_language_toolchain_store(),
                environment.clone(),
                git_store.clone(),
                cx,
            );
            task_store.shared(REMOTE_SERVER_PROJECT_ID, session.clone(), cx);
            task_store
        });
        let settings_observer = cx.new(|cx| {
            let mut observer = SettingsObserver::new_local(
                fs.clone(),
                worktree_store.clone(),
                task_store.clone(),
                true,
                cx,
            );
            observer.shared(REMOTE_SERVER_PROJECT_ID, session.clone(), cx);
            observer
        });

        let lsp_store = cx.new(|cx| {
            let mut lsp_store = LspStore::new_local(
                buffer_store.clone(),
                worktree_store.clone(),
                prettier_store.clone(),
                toolchain_store
                    .read(cx)
                    .as_local_store()
                    .expect("Toolchain store to be local")
                    .clone(),
                environment.clone(),
                manifest_tree,
                languages.clone(),
                // ADR-F38: what language servers on this host fetch
                // themselves with. Relayed to the IDE by ADR-F27, and the IDE
                // does not gate the relay either.
                http_client::ide_lane(http_client.clone()),
                fs.clone(),
                cx,
            );
            lsp_store.shared(REMOTE_SERVER_PROJECT_ID, session.clone(), cx);
            lsp_store
        });

        let agent_server_store = cx.new(|cx| {
            let mut agent_server_store = AgentServerStore::local(
                node_runtime.clone(),
                fs.clone(),
                environment.clone(),
                http_client.clone(),
                cx,
            );
            agent_server_store.shared(REMOTE_SERVER_PROJECT_ID, session.clone(), cx);
            agent_server_store
        });

        let context_server_store = cx.new(|cx| {
            let mut context_server_store =
                ContextServerStore::local(worktree_store.clone(), None, true, cx);
            context_server_store.shared(REMOTE_SERVER_PROJECT_ID, session.clone());
            context_server_store
        });

        cx.subscribe(&lsp_store, Self::on_lsp_store_event).detach();

        cx.subscribe(&buffer_store, |_this, _buffer_store, event, cx| {
            if let BufferStoreEvent::BufferAdded(buffer) = event {
                cx.subscribe(buffer, Self::on_buffer_event).detach();
            }
        })
        .detach();

        // local_machine -> ssh handlers
        session.subscribe_to_entity(REMOTE_SERVER_PROJECT_ID, &worktree_store);
        session.subscribe_to_entity(REMOTE_SERVER_PROJECT_ID, &buffer_store);
        session.subscribe_to_entity(REMOTE_SERVER_PROJECT_ID, &cx.entity());
        session.subscribe_to_entity(REMOTE_SERVER_PROJECT_ID, &lsp_store);
        session.subscribe_to_entity(REMOTE_SERVER_PROJECT_ID, &task_store);
        session.subscribe_to_entity(REMOTE_SERVER_PROJECT_ID, &toolchain_store);
        session.subscribe_to_entity(REMOTE_SERVER_PROJECT_ID, &dap_store);
        session.subscribe_to_entity(REMOTE_SERVER_PROJECT_ID, &breakpoint_store);
        session.subscribe_to_entity(REMOTE_SERVER_PROJECT_ID, &settings_observer);
        session.subscribe_to_entity(REMOTE_SERVER_PROJECT_ID, &git_store);
        session.subscribe_to_entity(REMOTE_SERVER_PROJECT_ID, &agent_server_store);
        session.subscribe_to_entity(REMOTE_SERVER_PROJECT_ID, &context_server_store);

        session.add_request_handler(cx.weak_entity(), Self::handle_list_remote_directory);
        session.add_request_handler(cx.weak_entity(), Self::handle_get_path_metadata);
        session.add_request_handler(cx.weak_entity(), Self::handle_shutdown_remote_server);
        session.add_request_handler(cx.weak_entity(), Self::handle_ping);
        session.add_request_handler(cx.weak_entity(), Self::handle_get_processes);
        session.add_request_handler(cx.weak_entity(), Self::handle_get_remote_profiling_data);
        session.add_request_handler(cx.weak_entity(), Self::handle_set_egress_allowlist);

        session.add_entity_request_handler(Self::handle_add_worktree);
        session.add_request_handler(cx.weak_entity(), Self::handle_remove_worktree);

        session.add_entity_request_handler(Self::handle_open_buffer_by_path);
        session.add_entity_request_handler(Self::handle_open_new_buffer);
        session.add_entity_request_handler(Self::handle_find_search_candidates);
        session.add_entity_request_handler(Self::handle_open_server_settings);
        session.add_entity_request_handler(Self::handle_get_directory_environment);
        session.add_entity_message_handler(Self::handle_toggle_lsp_logs);
        session.add_entity_request_handler(Self::handle_open_image_by_path);
        session.add_entity_request_handler(Self::handle_trust_worktrees);
        session.add_entity_request_handler(Self::handle_restrict_worktrees);
        session.add_entity_request_handler(Self::handle_download_file_by_path);

        session.add_entity_message_handler(Self::handle_find_search_candidates_cancel);
        session.add_entity_request_handler(BufferStore::handle_update_buffer);
        session.add_entity_message_handler(BufferStore::handle_close_buffer);

        session.add_request_handler(
            extensions.downgrade(),
            HeadlessExtensionStore::handle_sync_extensions,
        );
        session.add_request_handler(
            extensions.downgrade(),
            HeadlessExtensionStore::handle_install_extension,
        );

        session.add_request_handler(cx.weak_entity(), Self::handle_spawn_kernel);
        session.add_request_handler(cx.weak_entity(), Self::handle_kill_kernel);

        // ADR-F28: the IDE's file transfer screen, using this host as its
        // "local" side.
        session.add_request_handler(cx.weak_entity(), Self::handle_run_transfer_command);
        session.add_request_handler(cx.weak_entity(), Self::handle_cancel_transfer_command);
        session.add_request_handler(cx.weak_entity(), Self::handle_list_transfer_directory);
        session.add_request_handler(cx.weak_entity(), Self::handle_transfer_file_operation);

        BufferStore::init(&session);
        WorktreeStore::init(&session);
        SettingsObserver::init(&session);
        LspStore::init(&session);
        TaskStore::init(Some(&session));
        ToolchainStore::init(&session);
        DapStore::init(&session, cx);
        // todo(debugger): Re init breakpoint store when we set it up for collab
        BreakpointStore::init(&session);
        GitStore::init(&session);
        AgentServerStore::init_headless(&session);
        ContextServerStore::init_headless(&session);

        HeadlessProject {
            next_entry_id: Default::default(),
            session,
            settings_observer,
            egress_policy,
            child_process_proxy,
            fs,
            worktree_store,
            buffer_store,
            lsp_store,
            task_store,
            dap_store,
            breakpoint_store,
            agent_server_store,
            context_server_store,
            languages,
            extensions,
            git_store,
            environment,
            profiling_collector: gpui::ProfilingCollector::new(startup_time),
            _toolchain_store: toolchain_store,
            kernels: Default::default(),
            transfer_commands: Default::default(),
            host,
            quit_app_on_shutdown: false,
        }
    }

    /// Stops what this project runs on the host, for when its session is
    /// closed while the process lives on (ADR-F45). Before that, the process
    /// exiting took care of it.
    pub fn shutdown(&mut self, cx: &mut Context<Self>) -> Task<()> {
        // Dropping a transfer command's cancellation sender is what makes its
        // handler drop — and so kill — the child.
        self.transfer_commands.clear();
        for (kernel_id, mut kernel) in self.kernels.drain() {
            if let Err(error) = kernel.kill() {
                log::warn!("failed to stop kernel {kernel_id}: {error}");
            }
        }

        let running_context_servers = self
            .context_server_store
            .read(cx)
            .running_servers()
            .into_iter()
            .map(|server| server.id())
            .collect::<Vec<_>>();
        self.context_server_store
            .update(cx, |context_server_store, cx| {
                for id in running_context_servers {
                    context_server_store.stop_server(&id, cx).log_err();
                }
            });
        let debug_sessions = self
            .dap_store
            .update(cx, |dap_store, cx| dap_store.shutdown_sessions(cx));
        let language_servers = self.lsp_store.update(cx, |lsp_store, cx| {
            lsp_store.shutdown_all_language_servers(cx)
        });

        // Removed rather than left for the last reference to drop, so that
        // their file watches go now even if something still holds on to one.
        let worktree_ids = self
            .worktree_store
            .read(cx)
            .worktrees()
            .map(|worktree| worktree.read(cx).id())
            .collect::<Vec<_>>();
        self.worktree_store.update(cx, |worktree_store, cx| {
            for worktree_id in worktree_ids {
                worktree_store.remove_worktree(worktree_id, cx);
            }
        });

        cx.spawn(async move |_, _| {
            futures::future::join(debug_sessions, language_servers).await;
        })
    }

    fn on_buffer_event(
        &mut self,
        buffer: Entity<Buffer>,
        event: &BufferEvent,
        cx: &mut Context<Self>,
    ) {
        if let BufferEvent::Operation {
            operation,
            is_local: true,
        } = event
        {
            cx.background_spawn(self.session.request(proto::UpdateBuffer {
                project_id: REMOTE_SERVER_PROJECT_ID,
                buffer_id: buffer.read(cx).remote_id().to_proto(),
                operations: vec![serialize_operation(operation)],
            }))
            .detach()
        }
    }

    fn on_lsp_store_event(
        &mut self,
        lsp_store: Entity<LspStore>,
        event: &LspStoreEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            LspStoreEvent::LanguageServerAdded(id, name, worktree_id) => {
                let log_store = cx
                    .try_global::<GlobalLogStore>()
                    .map(|lsp_logs| lsp_logs.0.clone());
                if let Some(log_store) = log_store {
                    log_store.update(cx, |log_store, cx| {
                        log_store.add_language_server(
                            LanguageServerKind::LocalSsh {
                                lsp_store: self.lsp_store.downgrade(),
                            },
                            *id,
                            Some(name.clone()),
                            *worktree_id,
                            lsp_store.read(cx).language_server_for_id(*id),
                            cx,
                        );
                    });
                }
            }
            LspStoreEvent::LanguageServerRemoved(id) => {
                let log_store = cx
                    .try_global::<GlobalLogStore>()
                    .map(|lsp_logs| lsp_logs.0.clone());
                if let Some(log_store) = log_store {
                    log_store.update(cx, |log_store, cx| {
                        log_store.remove_language_server(*id, cx);
                    });
                }
                self.session
                    .send(proto::UpdateLanguageServer {
                        project_id: REMOTE_SERVER_PROJECT_ID,
                        server_name: None,
                        language_server_id: id.to_proto(),
                        variant: Some(proto::update_language_server::Variant::Removed(
                            proto::ServerRemoved {},
                        )),
                    })
                    .log_err();
            }
            LspStoreEvent::LanguageServerUpdate {
                language_server_id,
                name,
                message,
            } => {
                self.session
                    .send(proto::UpdateLanguageServer {
                        project_id: REMOTE_SERVER_PROJECT_ID,
                        server_name: name.as_ref().map(|name| name.to_string()),
                        language_server_id: language_server_id.to_proto(),
                        variant: Some(message.clone()),
                    })
                    .log_err();
            }
            LspStoreEvent::Notification(message) => {
                self.session
                    .send(proto::Toast {
                        project_id: REMOTE_SERVER_PROJECT_ID,
                        notification_id: "lsp".to_string(),
                        message: message.clone(),
                    })
                    .log_err();
            }
            LspStoreEvent::LanguageServerPrompt(prompt) => {
                let request = self.session.request(proto::LanguageServerPromptRequest {
                    project_id: REMOTE_SERVER_PROJECT_ID,
                    actions: prompt
                        .actions
                        .iter()
                        .map(|action| action.title.to_string())
                        .collect(),
                    level: Some(prompt_to_proto(prompt)),
                    lsp_name: prompt.lsp_name.clone(),
                    message: prompt.message.clone(),
                });
                let prompt = prompt.clone();
                cx.background_spawn(async move {
                    let response = request.await?;
                    if let Some(action_response) = response.action_response {
                        prompt.respond(action_response as usize).await;
                    }
                    anyhow::Ok(())
                })
                .detach();
            }
            _ => {}
        }
    }

    pub async fn handle_add_worktree(
        this: Entity<Self>,
        message: TypedEnvelope<proto::AddWorktree>,
        mut cx: AsyncApp,
    ) -> Result<proto::AddWorktreeResponse> {
        use client::ErrorCodeExt;
        let fs = this.read_with(&cx, |this, _| this.fs.clone());
        let path = PathBuf::from(shellexpand::tilde(&message.payload.path).to_string());

        let canonicalized = match fs.canonicalize(&path).await {
            Ok(path) => path,
            Err(e) => {
                let mut parent = path
                    .parent()
                    .ok_or(e)
                    .with_context(|| format!("{path:?} does not exist"))?;
                if parent == Path::new("") {
                    parent = util::paths::home_dir();
                }
                let parent = fs.canonicalize(parent).await.map_err(|_| {
                    anyhow!(
                        proto::ErrorCode::DevServerProjectPathDoesNotExist
                            .with_tag("path", path.to_string_lossy().as_ref())
                    )
                })?;
                if let Some(file_name) = path.file_name() {
                    parent.join(file_name)
                } else {
                    parent
                }
            }
        };
        let next_worktree_id = this
            .update(&mut cx, |this, cx| {
                this.worktree_store
                    .update(cx, |worktree_store, _| worktree_store.next_worktree_id())
            })
            .await?;
        let worktree = this
            .read_with(&cx.clone(), |this, _| {
                Worktree::local(
                    Arc::from(canonicalized.as_path()),
                    message.payload.visible,
                    this.fs.clone(),
                    this.next_entry_id.clone(),
                    true,
                    next_worktree_id,
                    &mut cx,
                )
            })
            .await?;

        let response = this.read_with(&cx, |_, cx| {
            let worktree = worktree.read(cx);
            proto::AddWorktreeResponse {
                worktree_id: worktree.id().to_proto(),
                canonicalized_path: canonicalized.to_string_lossy().into_owned(),
                root_repo_common_dir: worktree
                    .root_repo_common_dir()
                    .map(|p| p.to_string_lossy().into_owned()),
                root_repo_is_linked_worktree: worktree.root_repo_is_linked_worktree(),
            }
        });

        // We spawn this asynchronously, so that we can send the response back
        // *before* `worktree_store.add()` can send out UpdateProject requests
        // to the client about the new worktree.
        //
        // That lets the client manage the reference/handles of the newly-added
        // worktree, before getting interrupted by an UpdateProject request.
        //
        // This fixes the problem of the client sending the AddWorktree request,
        // headless project sending out a project update, client receiving it
        // and immediately dropping the reference of the new client, causing it
        // to be dropped on the headless project, and the client only then
        // receiving a response to AddWorktree.
        cx.spawn(async move |cx| {
            this.update(cx, |this, cx| {
                this.worktree_store.update(cx, |worktree_store, cx| {
                    worktree_store.add(&worktree, cx);
                });
            });
        })
        .detach();

        Ok(response)
    }

    pub async fn handle_remove_worktree(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::RemoveWorktree>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let worktree_id = WorktreeId::from_proto(envelope.payload.worktree_id);
        this.update(&mut cx, |this, cx| {
            this.worktree_store.update(cx, |worktree_store, cx| {
                worktree_store.remove_worktree(worktree_id, cx);
            });
        });
        Ok(proto::Ack {})
    }

    pub async fn handle_open_buffer_by_path(
        this: Entity<Self>,
        message: TypedEnvelope<proto::OpenBufferByPath>,
        mut cx: AsyncApp,
    ) -> Result<proto::OpenBufferResponse> {
        let worktree_id = WorktreeId::from_proto(message.payload.worktree_id);
        let path = RelPath::from_unix_str(&message.payload.path)?.into();
        let (buffer_store, buffer) = this.update(&mut cx, |this, cx| {
            let buffer_store = this.buffer_store.clone();
            let buffer = this.buffer_store.update(cx, |buffer_store, cx| {
                buffer_store.open_buffer(ProjectPath { worktree_id, path }, cx)
            });
            (buffer_store, buffer)
        });

        let buffer = buffer.await?;
        let buffer_id = buffer.read_with(&cx, |b, _| b.remote_id());
        buffer_store.update(&mut cx, |buffer_store, cx| {
            buffer_store
                .create_buffer_for_peer(&buffer, REMOTE_SERVER_PEER_ID, cx)
                .detach_and_log_err(cx);
        });

        Ok(proto::OpenBufferResponse {
            buffer_id: buffer_id.to_proto(),
        })
    }

    pub async fn handle_open_image_by_path(
        this: Entity<Self>,
        message: TypedEnvelope<proto::OpenImageByPath>,
        mut cx: AsyncApp,
    ) -> Result<proto::OpenImageResponse> {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        let worktree_id = WorktreeId::from_proto(message.payload.worktree_id);
        let path = RelPath::from_unix_str(&message.payload.path)?;
        let project_id = message.payload.project_id;
        use proto::create_image_for_peer::Variant;

        let (worktree_store, session) = this.read_with(&cx, |this, _| {
            (this.worktree_store.clone(), this.session.clone())
        });

        let worktree = worktree_store
            .read_with(&cx, |store, cx| store.worktree_for_id(worktree_id, cx))
            .context("worktree not found")?;

        let load_task = worktree.update(&mut cx, |worktree, cx| {
            worktree.load_binary_file(path.as_ref(), cx)
        });

        let loaded_file = load_task.await?;
        let content = loaded_file.content;
        let file = loaded_file.file;

        let proto_file = worktree.read_with(&cx, |_worktree, cx| file.to_proto(cx));
        let image_id =
            ImageId::from(NonZeroU64::new(NEXT_ID.fetch_add(1, Ordering::Relaxed)).unwrap());

        let format = image::guess_format(&content)
            .map(|f| format!("{:?}", f).to_lowercase())
            .unwrap_or_else(|_| "unknown".to_string());

        let state = proto::ImageState {
            id: image_id.to_proto(),
            file: Some(proto_file),
            content_size: content.len() as u64,
            format,
        };

        session.send(proto::CreateImageForPeer {
            project_id,
            peer_id: Some(REMOTE_SERVER_PEER_ID),
            variant: Some(Variant::State(state)),
        })?;

        const CHUNK_SIZE: usize = 1024 * 1024; // 1MB chunks
        for chunk in content.chunks(CHUNK_SIZE) {
            session.send(proto::CreateImageForPeer {
                project_id,
                peer_id: Some(REMOTE_SERVER_PEER_ID),
                variant: Some(Variant::Chunk(proto::ImageChunk {
                    image_id: image_id.to_proto(),
                    data: chunk.to_vec(),
                })),
            })?;
        }

        Ok(proto::OpenImageResponse {
            image_id: image_id.to_proto(),
        })
    }

    pub async fn handle_trust_worktrees(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::TrustWorktrees>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let trusted_worktrees = cx
            .update(|cx| TrustedWorktrees::try_get_global(cx))
            .context("missing trusted worktrees")?;
        let worktree_store = this.read_with(&cx, |project, _| project.worktree_store.clone());
        trusted_worktrees.update(&mut cx, |trusted_worktrees, cx| {
            trusted_worktrees.trust(
                &worktree_store,
                envelope
                    .payload
                    .trusted_paths
                    .into_iter()
                    .filter_map(PathTrust::from_proto)
                    .collect(),
                cx,
            );
        });
        Ok(proto::Ack {})
    }

    pub async fn handle_restrict_worktrees(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::RestrictWorktrees>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let trusted_worktrees = cx
            .update(|cx| TrustedWorktrees::try_get_global(cx))
            .context("missing trusted worktrees")?;
        let worktree_store = this.read_with(&cx, |project, _| project.worktree_store.downgrade());
        trusted_worktrees.update(&mut cx, |trusted_worktrees, cx| {
            let restricted_paths = envelope
                .payload
                .worktree_ids
                .into_iter()
                .map(WorktreeId::from_proto)
                .map(PathTrust::Worktree)
                .collect::<HashSet<_>>();
            trusted_worktrees.restrict(worktree_store, restricted_paths, cx);
        });
        Ok(proto::Ack {})
    }

    pub async fn handle_download_file_by_path(
        this: Entity<Self>,
        message: TypedEnvelope<proto::DownloadFileByPath>,
        mut cx: AsyncApp,
    ) -> Result<proto::DownloadFileResponse> {
        log::debug!(
            "handle_download_file_by_path: received request: {:?}",
            message.payload
        );

        let worktree_id = WorktreeId::from_proto(message.payload.worktree_id);
        let path = RelPath::from_unix_str(&message.payload.path)?;
        let project_id = message.payload.project_id;
        let file_id = message.payload.file_id;
        log::debug!(
            "handle_download_file_by_path: worktree_id={:?}, path={:?}, file_id={}",
            worktree_id,
            path,
            file_id
        );
        use proto::create_file_for_peer::Variant;

        let (worktree_store, session): (Entity<WorktreeStore>, AnyProtoClient) = this
            .read_with(&cx, |this, _| {
                (this.worktree_store.clone(), this.session.clone())
            });

        let worktree = worktree_store
            .read_with(&cx, |store, cx| store.worktree_for_id(worktree_id, cx))
            .context("worktree not found")?;

        let download_task = worktree.update(&mut cx, |worktree: &mut Worktree, cx| {
            worktree.load_binary_file(path.as_ref(), cx)
        });

        let downloaded_file = download_task.await?;
        let content = downloaded_file.content;
        let file = downloaded_file.file;
        log::debug!(
            "handle_download_file_by_path: file loaded, content_size={}",
            content.len()
        );

        let proto_file = worktree.read_with(&cx, |_worktree: &Worktree, cx| file.to_proto(cx));
        log::debug!(
            "handle_download_file_by_path: using client-provided file_id={}",
            file_id
        );

        let state = proto::FileState {
            id: file_id,
            file: Some(proto_file),
            content_size: content.len() as u64,
        };

        log::debug!("handle_download_file_by_path: sending State message");
        session.send(proto::CreateFileForPeer {
            project_id,
            peer_id: Some(REMOTE_SERVER_PEER_ID),
            variant: Some(Variant::State(state)),
        })?;

        const CHUNK_SIZE: usize = 1024 * 1024; // 1MB chunks
        let num_chunks = content.len().div_ceil(CHUNK_SIZE);
        log::debug!(
            "handle_download_file_by_path: sending {} chunks",
            num_chunks
        );
        for (i, chunk) in content.chunks(CHUNK_SIZE).enumerate() {
            log::trace!(
                "handle_download_file_by_path: sending chunk {}/{}, size={}",
                i + 1,
                num_chunks,
                chunk.len()
            );
            session.send(proto::CreateFileForPeer {
                project_id,
                peer_id: Some(REMOTE_SERVER_PEER_ID),
                variant: Some(Variant::Chunk(proto::FileChunk {
                    file_id,
                    data: chunk.to_vec(),
                })),
            })?;
        }

        log::debug!(
            "handle_download_file_by_path: returning file_id={}",
            file_id
        );
        Ok(proto::DownloadFileResponse { file_id })
    }

    pub async fn handle_open_new_buffer(
        this: Entity<Self>,
        _message: TypedEnvelope<proto::OpenNewBuffer>,
        mut cx: AsyncApp,
    ) -> Result<proto::OpenBufferResponse> {
        let (buffer_store, buffer) = this.update(&mut cx, |this, cx| {
            let buffer_store = this.buffer_store.clone();
            let buffer = this.buffer_store.update(cx, |buffer_store, cx| {
                buffer_store.create_buffer(None, true, cx)
            });
            (buffer_store, buffer)
        });

        let buffer = buffer.await?;
        let buffer_id = buffer.read_with(&cx, |b, _| b.remote_id());
        buffer_store.update(&mut cx, |buffer_store, cx| {
            buffer_store
                .create_buffer_for_peer(&buffer, REMOTE_SERVER_PEER_ID, cx)
                .detach_and_log_err(cx);
        });

        Ok(proto::OpenBufferResponse {
            buffer_id: buffer_id.to_proto(),
        })
    }

    async fn handle_toggle_lsp_logs(
        _: Entity<Self>,
        envelope: TypedEnvelope<proto::ToggleLspLogs>,
        cx: AsyncApp,
    ) -> Result<()> {
        let server_id = LanguageServerId::from_proto(envelope.payload.server_id);
        cx.update(|cx| {
            let log_store = cx
                .try_global::<GlobalLogStore>()
                .map(|global_log_store| global_log_store.0.clone())
                .context("lsp logs store is missing")?;
            let toggled_log_kind =
                match proto::toggle_lsp_logs::LogType::from_i32(envelope.payload.log_type)
                    .context("invalid log type")?
                {
                    proto::toggle_lsp_logs::LogType::Log => LogKind::Logs,
                    proto::toggle_lsp_logs::LogType::Trace => LogKind::Trace,
                    proto::toggle_lsp_logs::LogType::Rpc => LogKind::Rpc,
                };
            log_store.update(cx, |log_store, _| {
                log_store.toggle_lsp_logs(server_id, envelope.payload.enabled, toggled_log_kind);
            });
            anyhow::Ok(())
        })?;

        Ok(())
    }

    async fn handle_open_server_settings(
        this: Entity<Self>,
        _: TypedEnvelope<proto::OpenServerSettings>,
        mut cx: AsyncApp,
    ) -> Result<proto::OpenBufferResponse> {
        let settings_path = paths::settings_file();
        let (worktree, path) = this
            .update(&mut cx, |this, cx| {
                this.worktree_store.update(cx, |worktree_store, cx| {
                    worktree_store.find_or_create_worktree(settings_path, false, cx)
                })
            })
            .await?;

        let (buffer, buffer_store) = this.update(&mut cx, |this, cx| {
            let buffer = this.buffer_store.update(cx, |buffer_store, cx| {
                buffer_store.open_buffer(
                    ProjectPath {
                        worktree_id: worktree.read(cx).id(),
                        path,
                    },
                    cx,
                )
            });

            (buffer, this.buffer_store.clone())
        });

        let buffer = buffer.await?;

        let buffer_id = cx.update(|cx| {
            if buffer.read(cx).is_empty() {
                buffer.update(cx, |buffer, cx| {
                    buffer.edit([(0..0, initial_server_settings_content())], None, cx)
                });
            }

            let buffer_id = buffer.read(cx).remote_id();

            buffer_store.update(cx, |buffer_store, cx| {
                buffer_store
                    .create_buffer_for_peer(&buffer, REMOTE_SERVER_PEER_ID, cx)
                    .detach_and_log_err(cx);
            });

            buffer_id
        });

        Ok(proto::OpenBufferResponse {
            buffer_id: buffer_id.to_proto(),
        })
    }

    async fn handle_spawn_kernel(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::SpawnKernel>,
        cx: AsyncApp,
    ) -> Result<proto::SpawnKernelResponse> {
        let fs = this.update(&mut cx.clone(), |this, _| this.fs.clone());

        let mut ports = Vec::new();
        for _ in 0..5 {
            let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
            let port = listener.local_addr()?.port();
            ports.push(port);
        }

        let connection_info = serde_json::json!({
            "shell_port": ports[0],
            "iopub_port": ports[1],
            "stdin_port": ports[2],
            "control_port": ports[3],
            "hb_port": ports[4],
            "ip": "127.0.0.1",
            "key": uuid::Uuid::new_v4().to_string(),
            "transport": "tcp",
            "signature_scheme": "hmac-sha256",
            "kernel_name": envelope.payload.kernel_name,
        });

        let connection_file_content = serde_json::to_string_pretty(&connection_info)?;
        let kernel_id = uuid::Uuid::new_v4().to_string();

        let connection_file_path = std::env::temp_dir().join(format!("kernel-{}.json", kernel_id));
        fs.save(
            &connection_file_path,
            &connection_file_content.as_str().into(),
            language::LineEnding::Unix,
        )
        .await?;

        let working_directory = if envelope.payload.working_directory.is_empty() {
            std::env::current_dir()
                .ok()
                .map(|p| p.to_string_lossy().into_owned())
        } else {
            Some(envelope.payload.working_directory)
        };

        // Spawn kernel (Assuming python for now, or we'd need to parse kernelspec logic here or pass the command)

        // Spawn kernel
        let spawn_kernel = |binary: &str, args: &[String]| {
            let mut command = smol::process::Command::new(binary);

            if !args.is_empty() {
                for arg in args {
                    if arg == "{connection_file}" {
                        command.arg(&connection_file_path);
                    } else {
                        command.arg(arg);
                    }
                }
            } else {
                command
                    .arg("-m")
                    .arg("ipykernel_launcher")
                    .arg("-f")
                    .arg(&connection_file_path);
            }

            // This ensures subprocesses spawned from the kernel use the correct Python environment
            let python_bin_dir = std::path::Path::new(binary).parent();
            if let Some(bin_dir) = python_bin_dir {
                if let Some(path_var) = std::env::var_os("PATH") {
                    let mut paths = std::env::split_paths(&path_var).collect::<Vec<_>>();
                    paths.insert(0, bin_dir.to_path_buf());
                    if let Ok(new_path) = std::env::join_paths(paths) {
                        command.env("PATH", new_path);
                    }
                }

                if let Some(venv_root) = bin_dir.parent() {
                    command.env("VIRTUAL_ENV", venv_root.to_string_lossy().to_string());
                }
            }

            if let Some(wd) = &working_directory {
                command.current_dir(wd);
            }
            command.spawn()
        };

        // We need to manage the child process lifecycle
        let child = if !envelope.payload.command.is_empty() {
            spawn_kernel(&envelope.payload.command, &envelope.payload.args).context(format!(
                "failed to spawn kernel process (command: {})",
                envelope.payload.command
            ))?
        } else if let Some(venv_python) = working_directory
            .as_ref()
            .and_then(|wd| find_venv_python(wd))
        {
            let path_str = venv_python.to_string_lossy().to_string();
            spawn_kernel(&path_str, &[]).context(format!(
                "failed to spawn kernel process (venv: {})",
                path_str
            ))?
        } else {
            spawn_kernel("python3", &[])
                .or_else(|_| spawn_kernel("python", &[]))
                .context("failed to spawn kernel process (tried python3 and python)")?
        };

        this.update(&mut cx.clone(), |this, _cx| {
            this.kernels.insert(kernel_id.clone(), child);
        });

        Ok(proto::SpawnKernelResponse {
            kernel_id,
            connection_file: connection_file_content,
        })
    }

    async fn handle_kill_kernel(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::KillKernel>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let kernel_id = envelope.payload.kernel_id;
        let child = this.update(&mut cx, |this, _| this.kernels.remove(&kernel_id));
        if let Some(mut child) = child {
            child.kill().log_err();
        }
        Ok(proto::Ack {})
    }

    /// ADR-F28: runs one transfer program on this host on behalf of the IDE's
    /// file transfer screen, so the screen's "local" pane can be this machine
    /// instead of the Windows IDE's.
    ///
    /// The user pressed a button to get here, which ADR-F18 / F19 class with
    /// typing into a terminal, so the process is **not** put behind the egress
    /// proxy. What keeps that from being a general remote-exec hole is
    /// [`ALLOWED_TRANSFER_PROGRAMS`] plus the absence of a shell: the program
    /// and its arguments are passed to `execvp` as given.
    async fn handle_run_transfer_command(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::RunTransferCommand>,
        mut cx: AsyncApp,
    ) -> Result<proto::RunTransferCommandResponse> {
        use futures::{AsyncWriteExt as _, FutureExt as _, select_biased};

        let payload = envelope.payload;
        anyhow::ensure!(
            ALLOWED_TRANSFER_PROGRAMS.contains(&payload.program.as_str()),
            "リモートでは {} を実行できません。",
            payload.program
        );
        let command_id = payload.command_id;
        let session = this.read_with(&cx, |this, _| this.session.clone());

        let mut command = util::command::new_command(&payload.program);
        command
            .args(&payload.args)
            .stdin(if payload.stdin.is_some() {
                std::process::Stdio::piped()
            } else {
                // `scp` has nothing to read; leaving a pipe open would only
                // give it something to block on.
                std::process::Stdio::null()
            })
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        if let Some(working_directory) = payload.cwd.as_deref() {
            command.current_dir(working_directory);
        }

        // Held for as long as the process runs: the helper script it writes
        // lives in a temporary directory that this value owns.
        let mut askpass = if payload.wants_askpass {
            let delegate = transfer_askpass_delegate(session, command_id, &mut cx);
            let askpass = AskPassSession::new(cx.background_executor().clone(), delegate).await?;
            command
                .env("SSH_ASKPASS", askpass.script_path())
                // OpenSSH 8.4+: ask even though this process has no DISPLAY.
                .env("SSH_ASKPASS_REQUIRE", "force");
            Some(askpass)
        } else {
            None
        };

        let mut child = command.spawn().with_context(|| {
            format!(
                "リモートで {} を実行できませんでした。\
                 インストールされているか確認してください。",
                payload.program
            )
        })?;
        if let Some(stdin) = payload.stdin {
            let mut handle = child.stdin.take().context("標準入力を開けませんでした")?;
            handle.write_all(stdin.as_bytes()).await?;
            handle.flush().await?;
            drop(handle);
        }

        let (cancellation_sender, cancellation) = oneshot::channel::<()>();
        this.update(&mut cx, |this, _| {
            this.transfer_commands
                .insert(command_id, cancellation_sender);
        });

        // No timeout on `run`: a transfer may legitimately go a long time
        // without prompting, and the connect timeout in the arguments already
        // bounds the unreachable-host case.
        let output = match askpass.as_mut() {
            Some(askpass) => select_biased! {
                _ = askpass.run(None).fuse() => None,
                _ = cancellation.fuse() => None,
                output = child.output().fuse() => Some(output?),
            },
            None => select_biased! {
                _ = cancellation.fuse() => None,
                output = child.output().fuse() => Some(output?),
            },
        };
        this.update(&mut cx, |this, _| {
            this.transfer_commands.remove(&command_id);
        });

        let Some(output) = output else {
            return Ok(proto::RunTransferCommandResponse {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: None,
                cancelled: true,
            });
        };
        Ok(proto::RunTransferCommandResponse {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            exit_code: output.status.code(),
            cancelled: false,
        })
    }

    async fn handle_cancel_transfer_command(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::CancelTransferCommand>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        this.update(&mut cx, |this, _| {
            // An id that is not here belongs to a command that already ended.
            if let Some(cancellation) = this.transfer_commands.remove(&envelope.payload.command_id)
            {
                cancellation.send(()).ok();
            }
        });
        Ok(proto::Ack {})
    }

    /// ADR-F28: like `ListRemoteDirectory`, but with the sizes and timestamps
    /// a file manager pane has to show.
    async fn handle_list_transfer_directory(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::ListTransferDirectory>,
        cx: AsyncApp,
    ) -> Result<proto::ListTransferDirectoryResponse> {
        use smol::stream::StreamExt as _;

        let fs = cx.read_entity(&this, |this, _| this.fs.clone());
        let directory = PathBuf::from(shellexpand::tilde(&envelope.payload.path).to_string());
        let mut children = fs.read_dir(&directory).await?;
        let mut entries = Vec::new();
        while let Some(child) = children.next().await {
            let child = child?;
            let Some(name) = child
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
            else {
                continue;
            };
            let metadata = fs.metadata(&child).await.ok().flatten();
            let kind = match &metadata {
                Some(metadata) if metadata.is_dir => {
                    proto::TransferEntryKind::TransferEntryDirectory
                }
                Some(metadata) if metadata.is_symlink => {
                    proto::TransferEntryKind::TransferEntrySymlink
                }
                _ => proto::TransferEntryKind::TransferEntryFile,
            };
            entries.push(proto::TransferEntry {
                name,
                kind: kind as i32,
                size: metadata
                    .as_ref()
                    .filter(|metadata| !metadata.is_dir)
                    .map(|metadata| metadata.len),
                mtime_epoch_seconds: metadata.as_ref().and_then(|metadata| {
                    metadata
                        .mtime
                        .timestamp_for_user()
                        .duration_since(std::time::UNIX_EPOCH)
                        .ok()
                        .map(|since_epoch| since_epoch.as_secs() as i64)
                }),
            });
        }
        Ok(proto::ListTransferDirectoryResponse { entries })
    }

    async fn handle_transfer_file_operation(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::TransferFileOperation>,
        cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let fs = cx.read_entity(&this, |this, _| this.fs.clone());
        let operation = envelope
            .payload
            .operation
            .context("操作が指定されていません")?;
        match operation {
            proto::transfer_file_operation::Operation::MakeDirectory(make_directory) => {
                fs.create_dir(Path::new(&make_directory.path)).await?;
            }
            proto::transfer_file_operation::Operation::Rename(rename) => {
                fs.rename(
                    Path::new(&rename.from),
                    Path::new(&rename.to),
                    RenameOptions::default(),
                )
                .await?;
            }
            proto::transfer_file_operation::Operation::Remove(remove) => {
                let path = PathBuf::from(&remove.path);
                let options = RemoveOptions {
                    recursive: remove.recursive,
                    ignore_if_not_exists: false,
                };
                // Decided here rather than taken from the client, whose
                // listing may be stale by now.
                if fs.is_dir(&path).await {
                    fs.remove_dir(&path, options).await?;
                } else {
                    fs.remove_file(&path, options).await?;
                }
            }
        }
        Ok(proto::Ack {})
    }

    async fn handle_find_search_candidates(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::FindSearchCandidates>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        use futures::stream::StreamExt as _;

        let peer_id = envelope.original_sender_id.unwrap_or(envelope.sender_id);
        let message = envelope.payload;
        let query = SearchQuery::from_proto(
            message.query.context("missing query field")?,
            PathStyle::local(),
        )?;

        let project_id = message.project_id;
        let buffer_store = this.read_with(&cx, |this, _| this.buffer_store.clone());
        let handle = message.handle;
        let _buffer_store = buffer_store.clone();
        let client = this.read_with(&cx, |this, _| this.session.clone());
        let task = cx.spawn(async move |cx| {
            let results = this.update(cx, |this, cx| {
                project::Search::local(
                    this.fs.clone(),
                    this.buffer_store.clone(),
                    this.worktree_store.clone(),
                    message.limit as _,
                    cx,
                )
                .into_handle(query, cx)
                .matching_buffers(cx)
            });
            let (batcher, batches) =
                project::project_search::AdaptiveBatcher::new(cx.background_executor());
            let mut new_matches = Box::pin(results.rx);

            let sender_task = cx.background_executor().spawn({
                let client = client.clone();
                async move {
                    let mut batches = std::pin::pin!(batches);
                    while let Some(buffer_ids) = batches.next().await {
                        client
                            .request(proto::FindSearchCandidatesChunk {
                                handle,
                                peer_id: Some(peer_id),
                                project_id,
                                variant: Some(
                                    proto::find_search_candidates_chunk::Variant::Matches(
                                        proto::FindSearchCandidatesMatches { buffer_ids },
                                    ),
                                ),
                            })
                            .await?;
                    }
                    anyhow::Ok(())
                }
            });

            while let Some((buffer, _)) = new_matches.next().await {
                let _ = buffer_store
                    .update(cx, |this, cx| {
                        this.create_buffer_for_peer(&buffer, REMOTE_SERVER_PEER_ID, cx)
                    })
                    .await;
                let buffer_id = buffer.read_with(cx, |this, _| this.remote_id().to_proto());
                batcher.push(buffer_id).await;
            }
            batcher.flush().await;

            sender_task.await?;

            client
                .request(proto::FindSearchCandidatesChunk {
                    handle,
                    peer_id: Some(peer_id),
                    project_id,
                    variant: Some(proto::find_search_candidates_chunk::Variant::Done(
                        proto::FindSearchCandidatesDone {},
                    )),
                })
                .await?;
            anyhow::Ok(())
        });
        _buffer_store.update(&mut cx, |this, _| {
            this.register_ongoing_project_search((peer_id, handle), task);
        });

        Ok(proto::Ack {})
    }

    // Goes from client to host.
    async fn handle_find_search_candidates_cancel(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::FindSearchCandidatesCancelled>,
        mut cx: AsyncApp,
    ) -> Result<()> {
        let buffer_store = this.read_with(&mut cx, |this, _| this.buffer_store.clone());
        BufferStore::handle_find_search_candidates_cancel(buffer_store, envelope, cx).await
    }

    async fn handle_list_remote_directory(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::ListRemoteDirectory>,
        cx: AsyncApp,
    ) -> Result<proto::ListRemoteDirectoryResponse> {
        use smol::stream::StreamExt;
        let fs = cx.read_entity(&this, |this, _| this.fs.clone());
        let expanded = PathBuf::from(shellexpand::tilde(&envelope.payload.path).to_string());
        let check_info = envelope
            .payload
            .config
            .as_ref()
            .is_some_and(|config| config.is_dir);

        let mut entries = Vec::new();
        let mut entry_info = Vec::new();
        let mut response = fs.read_dir(&expanded).await?;
        while let Some(path) = response.next().await {
            let path = path?;
            if let Some(file_name) = path.file_name() {
                entries.push(file_name.to_string_lossy().into_owned());
                if check_info {
                    let is_dir = fs.is_dir(&path).await;
                    entry_info.push(proto::EntryInfo { is_dir });
                }
            }
        }
        Ok(proto::ListRemoteDirectoryResponse {
            entries,
            entry_info,
        })
    }

    async fn handle_get_path_metadata(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GetPathMetadata>,
        cx: AsyncApp,
    ) -> Result<proto::GetPathMetadataResponse> {
        let fs = cx.read_entity(&this, |this, _| this.fs.clone());
        let expanded = PathBuf::from(shellexpand::tilde(&envelope.payload.path).to_string());

        let metadata = fs.metadata(&expanded).await?;
        let is_dir = metadata.map(|metadata| metadata.is_dir).unwrap_or(false);

        Ok(proto::GetPathMetadataResponse {
            exists: metadata.is_some(),
            is_dir,
            path: expanded.to_string_lossy().into_owned(),
        })
    }

    async fn handle_shutdown_remote_server(
        this: Entity<Self>,
        _envelope: TypedEnvelope<proto::ShutdownRemoteServer>,
        cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let quit_app = this.read_with(&cx, |this, _| this.quit_app_on_shutdown);
        // Deferred so that the acknowledgement goes out before anything is
        // torn down.
        cx.spawn(async move |cx| {
            if quit_app {
                cx.update(|cx| {
                    // TODO: This is a hack, because in a headless project, shutdown isn't executed
                    // when calling quit, but it should be.
                    cx.shutdown();
                    cx.quit();
                })
            } else {
                this.update(cx, |_, cx| cx.emit(HeadlessProjectEvent::ShutdownRequested))
            }
        })
        .detach();

        Ok(proto::Ack {})
    }

    pub async fn handle_ping(
        _this: Entity<Self>,
        _envelope: TypedEnvelope<proto::Ping>,
        _cx: AsyncApp,
    ) -> Result<proto::Ack> {
        log::debug!("Received ping from client");
        Ok(proto::Ack {})
    }

    /// Applies the egress allowlist the connecting IDE pushes right after
    /// connecting (ADR-F11, DESIGN.md §4). The client's list is the source of
    /// truth; this process's own `HttpClient` had been denying everything but
    /// loopback (and the built-in extension-marketplace entries) until now.
    ///
    /// Sent with `send`, not `request`, on the client side, so a client that
    /// predates this message (or a test harness with no handler for it) never
    /// blocks on a response that doesn't come; the `Ack` this returns is for
    /// symmetry with the rest of this file's handlers and isn't waited on.
    ///
    /// The same message carries the `proxy_child_processes` choice, which is
    /// what starts this host's child-process egress proxy (ADR-F14). Starting
    /// it here rather than at construction is deliberate: until the allowlist
    /// lands, this process's policy denies everything, and a proxy that denied
    /// every destination would be worse than none.
    pub async fn handle_set_egress_allowlist(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::SetEgressAllowlist>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let egress_policy = this.read_with(&cx, |this, _| this.egress_policy.clone());
        let Some(egress_policy) = egress_policy else {
            log::debug!(
                "egress allowlist: received {} host pattern(s) from the client, \
                 but this process's HTTP client has no allowlist to update",
                envelope.payload.allowed_hosts.len()
            );
            return Ok(proto::Ack {});
        };

        let (policy, errors) = EgressPolicy::from_host_strings(&envelope.payload.allowed_hosts);
        for error in errors {
            log::warn!("egress allowlist: skipping entry pushed by the client — {error}");
        }
        log::info!(
            "egress allowlist: {} host pattern(s) received from the client",
            policy.patterns().len()
        );
        egress_policy.set(policy);

        // A client that predates the field means the mechanism is on, matching
        // what an absent value in the policy file means on the IDE's own side.
        let enabled = envelope.payload.proxy_child_processes.unwrap_or(true);
        this.update(&mut cx, |this, _| {
            if let Some(supervisor) = this.child_process_proxy.as_ref() {
                supervisor.borrow_mut().apply(enabled, &egress_policy);
            }
        });
        match http_client::child_process_proxy() {
            Some(endpoint) => log::info!(
                "child process proxy: agent child processes on this host are pointed at {endpoint}"
            ),
            None => log::info!(
                "child process proxy: off; agent child processes on this host reach the \
                 network directly"
            ),
        }

        Ok(proto::Ack {})
    }

    async fn handle_get_processes(
        _this: Entity<Self>,
        _envelope: TypedEnvelope<proto::GetProcesses>,
        _cx: AsyncApp,
    ) -> Result<proto::GetProcessesResponse> {
        let mut processes = Vec::new();
        let refresh_kind = RefreshKind::nothing().with_processes(
            ProcessRefreshKind::nothing()
                .without_tasks()
                .with_cmd(UpdateKind::Always),
        );

        for process in System::new_with_specifics(refresh_kind)
            .processes()
            .values()
        {
            let name = process.name().to_string_lossy().into_owned();
            let command = process
                .cmd()
                .iter()
                .map(|s| s.to_string_lossy().into_owned())
                .collect::<Vec<_>>();

            processes.push(proto::ProcessInfo {
                pid: process.pid().as_u32(),
                name,
                command,
            });
        }

        processes.sort_by_key(|p| p.name.clone());

        Ok(proto::GetProcessesResponse { processes })
    }

    async fn handle_get_remote_profiling_data(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GetRemoteProfilingData>,
        cx: AsyncApp,
    ) -> Result<proto::GetRemoteProfilingDataResponse> {
        let foreground_only = envelope.payload.foreground_only;

        let (deltas, now_nanos) = cx.update(|cx| {
            let timings = if foreground_only {
                vec![gpui::profiler::get_current_thread_timings(
                    TasksIncluded::OnlyCompleted,
                )]
            } else {
                gpui::profiler::get_all_timings(TasksIncluded::OnlyCompleted)
            };
            this.update(cx, |this, _cx| {
                let deltas = this.profiling_collector.collect_unseen(timings);
                let now_nanos = Instant::now()
                    .duration_since(this.profiling_collector.startup_time())
                    .as_nanos() as u64;
                (deltas, now_nanos)
            })
        });

        let threads = deltas
            .into_iter()
            .map(|delta| proto::RemoteProfilingThread {
                thread_name: delta.thread_name,
                thread_id: delta.thread_id,
                timings: delta
                    .new_timings
                    .into_iter()
                    .map(|t| proto::RemoteProfilingTiming {
                        location: Some(proto::RemoteProfilingLocation {
                            file: t.location.file.to_string(),
                            line: t.location.line,
                            column: t.location.column,
                        }),
                        start_nanos: t.start as u64,
                        duration_nanos: t.duration as u64,
                    })
                    .collect(),
            })
            .collect();

        Ok(proto::GetRemoteProfilingDataResponse { threads, now_nanos })
    }

    async fn handle_get_directory_environment(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GetDirectoryEnvironment>,
        mut cx: AsyncApp,
    ) -> Result<proto::DirectoryEnvironment> {
        let shell = task::shell_from_proto(envelope.payload.shell.context("missing shell")?)?;
        let directory = PathBuf::from(envelope.payload.directory);
        let environment = this
            .update(&mut cx, |this, cx| {
                this.environment.update(cx, |environment, cx| {
                    environment.local_directory_environment(&shell, directory.into(), cx)
                })
            })
            .await
            .context("failed to get directory environment")?
            .into_iter()
            .collect();
        Ok(proto::DirectoryEnvironment { environment })
    }
}

/// ADR-F28: the only programs the IDE's file transfer screen may start on this
/// host. `RunTransferCommand` is a remote-exec message in every other respect,
/// so this list — and the fact that no shell is involved — is what keeps it
/// from being one.
const ALLOWED_TRANSFER_PROGRAMS: [&str; 3] = ["sftp", "scp", "curl"];

/// Relays OpenSSH's passphrase / password / host-key questions to the IDE,
/// which renders them inside the transfer screen. The same shape as the git
/// askpass relay, but keyed by transfer command rather than by repository.
fn transfer_askpass_delegate(
    session: AnyProtoClient,
    command_id: u64,
    cx: &mut AsyncApp,
) -> AskPassDelegate {
    AskPassDelegate::new(cx, move |prompt, response_sender, cx| {
        let session = session.clone();
        cx.spawn(async move |_| {
            let response = session
                .request(proto::TransferAskPassRequest {
                    project_id: REMOTE_SERVER_PROJECT_ID,
                    command_id,
                    prompt,
                })
                .await;
            // Dropping `response_sender` without sending is how the askpass
            // session learns the user declined, which stops the process.
            let Some(mut secret) = response.log_err().and_then(|response| response.response) else {
                return;
            };
            if let Some(password) = EncryptedPassword::try_from(secret.as_str()).log_err() {
                response_sender.send(password).ok();
            }
            secret.zeroize();
        })
        .detach();
    })
}

fn prompt_to_proto(
    prompt: &project::LanguageServerPromptRequest,
) -> proto::language_server_prompt_request::Level {
    match prompt.level {
        PromptLevel::Info => proto::language_server_prompt_request::Level::Info(
            proto::language_server_prompt_request::Info {},
        ),
        PromptLevel::Warning => proto::language_server_prompt_request::Level::Warning(
            proto::language_server_prompt_request::Warning {},
        ),
        PromptLevel::Critical => proto::language_server_prompt_request::Level::Critical(
            proto::language_server_prompt_request::Critical {},
        ),
    }
}

fn find_venv_python(working_directory: &str) -> Option<std::path::PathBuf> {
    let wd = std::path::Path::new(working_directory);
    for dir_name in &[".venv", "venv", ".env", "env"] {
        let venv_dir = wd.join(dir_name);
        let has_pyvenv_cfg = venv_dir.join("pyvenv.cfg").is_file();
        let has_activate = venv_dir.join("bin").join("activate").is_file();
        if has_pyvenv_cfg || has_activate {
            let python = venv_dir.join("bin").join("python");
            if python.is_file() {
                return Some(python);
            }
            let python3 = venv_dir.join("bin").join("python3");
            if python3.is_file() {
                return Some(python3);
            }
        }
    }
    None
}
