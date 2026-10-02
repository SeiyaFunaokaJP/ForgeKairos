mod daemon;
pub mod display_language;
mod headless_project;
mod stale_files;
pub mod terminal_session;

#[cfg(test)]
mod clangd_memory_remote_tests;
#[cfg(test)]
mod remote_editing_tests;
#[cfg(test)]
mod symbol_index_remote_tests;

#[cfg(windows)]
pub mod windows;

pub use headless_project::{
    HeadlessAppState, HeadlessHost, HeadlessHostState, HeadlessProject, HeadlessProjectEvent,
};

use anyhow::{Context as _, Result, anyhow};
use clap::Subcommand;
use client::ProxySettings;
use collections::HashMap;
use daemon::{SessionRegistry, SessionRegistryOptions, SessionRouter};
use extension::ExtensionHostProxy;
use fs::{Fs, RealFs};
use futures::{
    AsyncRead, AsyncWrite, AsyncWriteExt, FutureExt,
    channel::{mpsc, oneshot},
};
use git::GitHostingProviderRegistry;
use gpui::{App, AppContext as _, Entity, Global, TaskExt as _, UpdateGlobal as _};
use gpui_tokio::Tokio;
use http_client::{
    AllowlistedHttpClient, AsyncBody, BlockedHttpClient, EgressPolicyHandle, HttpClient,
    RemoteProxyHttpClient, Request, Response, Url, read_proxy_from_env,
};
use language::LanguageRegistry;
use net::async_net::{UnixListener, UnixStream};
use node_runtime::{NodeBinaryOptions, NodeRuntime};
use paths::logs_dir;
use project::{project_settings::ProjectSettings, trusted_worktrees};
use proto::CrashReport;
use release_channel::{AppCommitSha, AppVersion, RELEASE_CHANNEL, ReleaseChannel};
use remote::{
    ProxyHttpRelay,
    json_log::LogRecord,
    protocol::{
        DAEMON_PROTOCOL_VERSION, DaemonFrameKind, DaemonHello, DaemonHelloOutcome,
        DaemonHelloReply, DaemonKeyParts, client_id_of_identifier, daemon_key, read_daemon_frame,
        read_json_message, read_message_raw, write_daemon_frame, write_json_message,
        write_size_prefixed_buffer,
    },
    proxy::ProxyLaunchError,
};
use reqwest_client::ReqwestClient;
use rpc::proto::{self, REMOTE_SERVER_PROJECT_ID};
use rpc::{AnyProtoClient, TypedEnvelope};
use settings::{RegisterSetting, Settings, SettingsStore, watch_config_file};
use smol::{
    Timer,
    channel::{Receiver, Sender},
    stream::StreamExt as _,
};
use std::{
    ffi::{OsStr, OsString},
    fs::File,
    io::Write,
    path::{Path, PathBuf},
    rc::Rc,
    str::FromStr,
    sync::{
        Arc, LazyLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, UNIX_EPOCH},
};
use thiserror::Error;
use util::ResultExt;

#[derive(Subcommand)]
pub enum Commands {
    /// Serves every session one client has on this host (ADR-F45). Started by
    /// `proxy`, which says where the daemon keeps its socket and its log.
    Daemon {
        #[arg(long)]
        log_file: PathBuf,
        #[arg(long)]
        state_dir: PathBuf,
    },
    Proxy {
        #[arg(long)]
        reconnect: bool,
        #[arg(long)]
        identifier: String,
    },
    Version,
    /// Keeps the shell of a remote terminal alive across disconnects.
    TerminalSession {
        #[command(subcommand)]
        command: terminal_session::TerminalSessionCommand,
    },
}

pub fn run(command: Commands) -> anyhow::Result<()> {
    use anyhow::Context;
    use release_channel::{RELEASE_CHANNEL, ReleaseChannel};

    match command {
        Commands::Daemon {
            log_file,
            state_dir,
        } => execute_daemon(log_file, state_dir),
        Commands::Proxy {
            identifier,
            reconnect,
        } => execute_proxy(identifier, reconnect).context("running proxy on the remote server"),
        Commands::Version => {
            let release_channel = *RELEASE_CHANNEL;
            match release_channel {
                ReleaseChannel::Stable | ReleaseChannel::Preview => {
                    println!("{}", env!("ZED_PKG_VERSION"))
                }
                ReleaseChannel::Nightly | ReleaseChannel::Dev => {
                    let commit_sha =
                        option_env!("ZED_COMMIT_SHA").unwrap_or(release_channel.dev_name());
                    let build_id = option_env!("ZED_BUILD_ID");
                    if let Some(build_id) = build_id {
                        println!("{}+{}", build_id, commit_sha)
                    } else {
                        println!("{commit_sha}");
                    }
                }
            };
            Ok(())
        }
        // Its exit codes are what the terminal reads (see
        // `terminal_session`), not the generic 1 of an error.
        Commands::TerminalSession { command } => std::process::exit(terminal_session::run(command)),
    }
}

pub static VERSION: LazyLock<String> = LazyLock::new(|| match *RELEASE_CHANNEL {
    ReleaseChannel::Stable | ReleaseChannel::Preview => env!("ZED_PKG_VERSION").to_owned(),
    ReleaseChannel::Nightly | ReleaseChannel::Dev => {
        let commit_sha = option_env!("ZED_COMMIT_SHA").unwrap_or("missing-zed-commit-sha");
        let build_identifier = option_env!("ZED_BUILD_ID");
        if let Some(build_id) = build_identifier {
            format!("{build_id}+{commit_sha}")
        } else {
            commit_sha.to_owned()
        }
    }
});

fn init_logging_proxy() {
    env_logger::builder()
        .format(|buf, record| {
            let mut log_record = LogRecord::new(record);
            log_record.message =
                std::borrow::Cow::Owned(format!("(remote proxy) {}", log_record.message));
            serde_json::to_writer(&mut *buf, &log_record)?;
            buf.write_all(b"\n")?;
            Ok(())
        })
        .init();
}

const REMOTE_SERVER_LOG_MAX_BYTES: u64 = 1024 * 1024;

struct RotatingLogFile {
    path: PathBuf,
    file: File,
    size_bytes: u64,
}

impl RotatingLogFile {
    fn open(path: &Path) -> Result<Self> {
        if std::fs::metadata(path)
            .map(|metadata| metadata.len() >= REMOTE_SERVER_LOG_MAX_BYTES)
            .unwrap_or(false)
        {
            rotate_log_file(path, &rotated_log_path(path))
                .context("failed to rotate existing remote server log")?;
        }

        let file = open_log_file(path).context("failed to open remote server log")?;
        let size_bytes = file
            .metadata()
            .context("failed to read remote server log metadata")?
            .len();

        Ok(Self {
            path: path.to_path_buf(),
            file,
            size_bytes,
        })
    }

    fn rotate(&mut self) -> std::io::Result<()> {
        self.file.flush()?;
        rotate_log_file(&self.path, &rotated_log_path(&self.path))?;
        self.file = open_log_file(&self.path)?;
        self.size_bytes = 0;
        Ok(())
    }
}

impl Write for RotatingLogFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.size_bytes.saturating_add(buf.len() as u64) > REMOTE_SERVER_LOG_MAX_BYTES {
            self.rotate()?;
        }

        self.file.write_all(buf)?;
        self.size_bytes += buf.len() as u64;

        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

fn open_log_file(path: &Path) -> std::io::Result<File> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
}

fn rotated_log_path(path: &Path) -> PathBuf {
    path.with_extension("1.log")
}

fn rotate_log_file(path: &Path, rotated_path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(rotated_path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    match std::fs::rename(path, rotated_path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    Ok(())
}

fn init_logging_server(log_file_path: &Path) -> Result<Receiver<Vec<u8>>> {
    struct MultiWrite {
        file: RotatingLogFile,
        channel: Sender<Vec<u8>>,
        buffer: Vec<u8>,
    }

    impl Write for MultiWrite {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.file.write_all(buf)?;
            self.buffer.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.channel
                .send_blocking(self.buffer.clone())
                .map_err(std::io::Error::other)?;
            self.buffer.clear();
            self.file.flush()
        }
    }

    let log_file = RotatingLogFile::open(log_file_path)
        .context("Failed to open rotating remote server log file")?;

    let (tx, rx) = async_channel::unbounded();

    let target = Box::new(MultiWrite {
        file: log_file,
        channel: tx,
        buffer: Vec::new(),
    });

    let old_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let backtrace = std::backtrace::Backtrace::force_capture();
        let message = info.payload_as_str().unwrap_or("Box<Any>").to_owned();
        let location = info
            .location()
            .map_or_else(|| "<unknown>".to_owned(), |location| location.to_string());
        let current_thread = std::thread::current();
        let thread_name = current_thread.name().unwrap_or("<unnamed>");

        let msg = format!("thread '{thread_name}' panicked at {location}:\n{message}\n{backtrace}");
        // NOTE: This log never reaches the client, as the communication is handled on a main thread task
        // which will never run once we panic.
        log::error!("{msg}");
        old_hook(info);
    }));
    env_logger::Builder::new()
        .filter_level(log::LevelFilter::Info)
        .parse_default_env()
        .target(env_logger::Target::Pipe(target))
        .format(|buf, record| {
            let mut log_record = LogRecord::new(record);
            log_record.message =
                std::borrow::Cow::Owned(format!("(remote server) {}", log_record.message));
            serde_json::to_writer(&mut *buf, &log_record)?;
            buf.write_all(b"\n")?;
            Ok(())
        })
        .init();

    Ok(rx)
}

/// Initializes the telemetry queue on the remote server, forwarding every
/// emitted event to the connected client over the proto channel.
///
/// The remote server cannot upload telemetry itself (it has no logged-in user,
/// no checksum seed, and no Telemetry instance), so without this its
/// `telemetry::event!` calls are silently dropped. The client attributes these
/// events to the remote host using the platform it already detected during
/// connection setup, so no OS metadata needs to be sent here.
///
/// Every session of the daemon belongs to the same IDE (ADR-F45), so each event
/// goes out once, on whichever session `router` prefers.
fn init_telemetry_forwarding(router: SessionRouter, cx: &mut App) {
    let (tx, mut rx) = mpsc::unbounded::<telemetry::Event>();
    telemetry::init(tx);

    cx.background_spawn(async move {
        while let Some(event) = rx.next().await {
            let Some(session) = router.preferred_client() else {
                continue;
            };
            let Some(event_json) = serde_json::to_string(&event).log_err() else {
                continue;
            };
            session
                .send(proto::TelemetryEvent {
                    project_id: REMOTE_SERVER_PROJECT_ID,
                    event_json,
                })
                .log_err();
        }
    })
    .detach();
}

pub(crate) fn handle_crash_files_requests(
    project: &Entity<HeadlessProject>,
    client: &AnyProtoClient,
) {
    client.add_request_handler(
        project.downgrade(),
        |_, _: TypedEnvelope<proto::GetCrashFiles>, _cx| async move {
            let mut legacy_panics = Vec::new();
            let mut crashes = Vec::new();
            let mut children = smol::fs::read_dir(paths::logs_dir()).await?;
            while let Some(child) = children.next().await {
                let child = child?;
                let child_path = child.path();

                let extension = child_path.extension();
                if extension == Some(OsStr::new("panic")) {
                    let filename = if let Some(filename) = child_path.file_name() {
                        filename.to_string_lossy()
                    } else {
                        continue;
                    };

                    if !filename.starts_with("zed") {
                        continue;
                    }

                    let file_contents = smol::fs::read_to_string(&child_path)
                        .await
                        .context("error reading panic file")?;

                    legacy_panics.push(file_contents);
                    smol::fs::remove_file(&child_path)
                        .await
                        .context("error removing panic")
                        .log_err();
                } else if extension == Some(OsStr::new("dmp")) {
                    let mut json_path = child_path.clone();
                    json_path.set_extension("json");
                    if let Ok(json_content) = smol::fs::read_to_string(&json_path).await {
                        crashes.push(CrashReport {
                            metadata: json_content,
                            minidump_contents: smol::fs::read(&child_path).await?,
                        });
                        smol::fs::remove_file(&child_path).await.log_err();
                        smol::fs::remove_file(&json_path).await.log_err();
                    } else {
                        log::error!("Couldn't find json metadata for crash: {child_path:?}");
                    }
                }
            }

            anyhow::Ok(proto::GetCrashFilesResponse { crashes })
        },
    );
}

fn init_paths() -> anyhow::Result<()> {
    for path in [
        paths::config_dir(),
        paths::extensions_dir(),
        paths::languages_dir(),
        paths::logs_dir(),
        paths::temp_dir(),
        paths::hang_traces_dir(),
        paths::remote_extensions_dir(),
        paths::remote_extensions_uploads_dir(),
    ]
    .iter()
    {
        std::fs::create_dir_all(path).with_context(|| format!("creating directory {path:?}"))?;
    }
    Ok(())
}

/// Keeps the daemon's sessions alive for as long as the process runs.
struct DaemonSessions(#[allow(dead_code)] Entity<SessionRegistry>);

impl Global for DaemonSessions {}

pub fn execute_daemon(log_file: PathBuf, state_dir: PathBuf) -> Result<()> {
    init_paths()?;

    let daemon_paths = DaemonPaths::new(state_dir, log_file);
    let startup_time = Instant::now();
    let app = gpui_platform::headless();
    let pid = std::process::id();
    let id = pid.to_string();
    let should_install_crash_handler =
        client::telemetry::should_install_crash_handler(*RELEASE_CHANNEL);

    let crash_handler = if should_install_crash_handler {
        Some(app.background_executor().spawn(crashes::init(
            crashes::InitCrashHandler {
                session_id: id,
                zed_version: VERSION.to_owned(),
                binary: "zed-remote-server".to_string(),
                release_channel: release_channel::RELEASE_CHANNEL_NAME.clone(),
                commit_sha: option_env!("ZED_COMMIT_SHA").unwrap_or("no_sha").to_owned(),
            },
            {
                let background_executor = app.background_executor();
                move |task| {
                    background_executor.spawn(task).detach();
                }
            },
            |pid| paths::temp_dir().join(format!("zed-remote-server-crash-handler-{pid}")),
            // we are running outside gpui
            #[allow(clippy::disallowed_methods)]
            |duration| FutureExt::map(Timer::after(duration), |_| ()),
        )))
    } else {
        crashes::force_backtrace();
        None
    };
    let log_rx = init_logging_server(&daemon_paths.log_file)?;
    log::info!(
        "starting up the daemon with PID {}:\nstate_dir: {:?}, log_file: {:?}",
        pid,
        daemon_paths.state_dir,
        daemon_paths.log_file
    );

    #[cfg(unix)]
    raise_open_file_limit();

    if let Err(error) = rayon::ThreadPoolBuilder::new()
        .num_threads(std::thread::available_parallelism().map_or(1, |n| n.get().div_ceil(2)))
        .stack_size(10 * 1024 * 1024)
        .thread_name(|ix| format!("RayonWorker{}", ix))
        .build_global()
    {
        log::warn!("could not size the rayon thread pool, using its defaults: {error}");
    }

    #[cfg(unix)]
    let shell_env_loaded_rx = {
        let (shell_env_loaded_tx, shell_env_loaded_rx) = oneshot::channel();
        app.background_executor()
            .spawn(async {
                util::load_login_shell_environment().await.log_err();
                shell_env_loaded_tx.send(()).ok();
            })
            .detach();
        Some(shell_env_loaded_rx)
    };
    #[cfg(windows)]
    let shell_env_loaded_rx: Option<oneshot::Receiver<()>> = None;

    let git_hosting_provider_registry = Arc::new(GitHostingProviderRegistry::new());
    let run = move |cx: &mut App| {
        if let Some(crash_handler) = crash_handler {
            cx.spawn(async move |_cx| {
                let _crash_handler = crash_handler.await;
                // cx.update(|cx| cx.set_global(CrashHandler(crash_handler)))
            })
            .detach();
        }
        settings::init(cx);
        let app_commit_sha = option_env!("ZED_COMMIT_SHA").map(|s| AppCommitSha::new(s.to_owned()));
        let app_version = AppVersion::load(
            env!("ZED_PKG_VERSION"),
            option_env!("ZED_BUILD_ID"),
            app_commit_sha,
        );
        release_channel::init(app_version, cx);
        gpui_tokio::init(cx);

        HeadlessProject::init(cx);
        display_language::init(cx);

        let is_wsl_interop = if cfg!(target_os = "linux") {
            // See: https://learn.microsoft.com/en-us/windows/wsl/filesystems#disable-interoperability
            matches!(std::fs::read_to_string("/proc/sys/fs/binfmt_misc/WSLInterop").or_else(|_| std::fs::read_to_string("/proc/sys/fs/binfmt_misc/WSLInterop-late")), Ok(s) if s.contains("enabled"))
        } else {
            false
        };

        log::info!("gpui app started, initializing the daemon");
        trusted_worktrees::init(HashMap::default(), cx);

        GitHostingProviderRegistry::set_global(git_hosting_provider_registry, cx);
        git_hosting_providers::init(cx);
        dap_adapters::init(cx);

        extension::init(cx);
        let extension_host_proxy = ExtensionHostProxy::global(cx);

        json_schema_store::init(cx);

        let router = SessionRouter::default();
        init_telemetry_forwarding(router.clone(), cx);

        // ADR-F27 (DESIGN.md §4.27). Built before any session exists, because
        // this host's HTTP client is. Each request goes out on a session that
        // is connected when it is made (ADR-F45), and fails at once when none
        // is, rather than waiting for a client that may not come back.
        let (proxy_http_relay, proxy_http_transport) = ProxyHttpRelay::new_with_session_source(
            {
                let router = router.clone();
                Arc::new(move || router.connected_client())
            },
            cx,
        );
        let relay_http_through_client = watch_relay_http_setting(cx);

        let fs = RealFs::new(None, cx.background_executor().clone());
        let node_settings_rx = initialize_settings(router.clone(), fs.clone(), cx);

        let proxy_url = read_proxy_settings(cx);

        // DESIGN.md §4 (ADR-F03) gates the IDE's own HTTP client behind
        // `AllowlistedHttpClient`; ADR-F11 extends that to this binary too,
        // since it makes its own outgoing requests (LSP/node downloads,
        // extensions) on the remote host. The policy starts out empty
        // (loopback only, deny-by-default) and is filled in once the
        // connecting client pushes its allowlist — see
        // `HeadlessProject::handle_set_egress_allowlist`.
        let http_client: Arc<dyn HttpClient> = {
            let _guard = Tokio::handle(cx).enter();
            let user_agent = format!(
                "Zed-Server/{} ({}; {})",
                env!("CARGO_PKG_VERSION"),
                std::env::consts::OS,
                std::env::consts::ARCH
            );
            let direct: Arc<dyn HttpClient> = match ReqwestClient::proxy_and_user_agent(
                proxy_url,
                &user_agent,
            ) {
                Ok(client) => Arc::new(client),
                Err(error) => {
                    // Editing does not need it; only what this host
                    // downloads without going through the IDE does.
                    log::error!(
                        "could not start this host's HTTP client, so only requests relayed through the IDE can succeed: {error:#}"
                    );
                    Arc::new(BlockedHttpClient)
                }
            };
            // ADR-F27: this host may have no route to the internet at all,
            // in which case its downloads only work by going through the
            // IDE. The allowlist check stays on the outside either way, so
            // a request refused here never leaves this machine.
            Arc::new(AllowlistedHttpClient::new(
                Arc::new(RemoteHttpClient {
                    relay: RemoteProxyHttpClient::new(
                        proxy_http_transport,
                        direct.user_agent().cloned(),
                    ),
                    direct,
                    relay_enabled: relay_http_through_client,
                }),
                EgressPolicyHandle::default(),
            ))
        };

        // ADR-F38: node/npm on this host serves language servers and
        // extensions, so it takes the IDE lane, exactly as it does in the
        // app itself.
        let node_runtime = NodeRuntime::new(
            http_client::ide_lane(http_client.clone()),
            shell_env_loaded_rx,
            node_settings_rx,
        );

        let mut languages = LanguageRegistry::new(cx.background_executor().clone());
        languages.set_language_server_download_dir(paths::languages_dir().clone());
        let languages = Arc::new(languages);

        let sweep_fs = fs.clone();
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
        let registry = cx.new(|cx| {
            SessionRegistry::new(
                SessionRegistryOptions {
                    host,
                    router,
                    proxy_http_relay,
                    is_wsl_interop,
                    init_worktree_trust: true,
                    on_idle: Rc::new({
                        let daemon_paths = daemon_paths.clone();
                        move |cx: &mut App| {
                            // Taken down before exiting, so that a proxy
                            // arriving now starts a new daemon instead of
                            // reaching one that is on its way out.
                            daemon_paths.remove_if_owned_by(std::process::id());
                            // TODO: This is a hack, because in a headless project, shutdown isn't executed
                            // when calling quit, but it should be.
                            cx.shutdown();
                            cx.quit();
                        }
                    }),
                },
                log_rx,
                cx,
            )
        });

        // Bound only now that everything a session needs is in place, so that
        // a proxy that reaches the socket is answered straight away: a proxy
        // left waiting long enough takes the daemon for hung and replaces it.
        let listener = match bind_daemon_socket(&daemon_paths) {
            Ok(listener) => listener,
            Err(error) => {
                log::error!("{error:#}; exiting");
                cx.shutdown();
                cx.quit();
                return;
            }
        };
        // Before the first request is served, so that no sandboxed agent
        // session on this host can start ahead of it: the agents keep the
        // sign-in they had under the name builds before the rename used.
        #[cfg(target_os = "linux")]
        match sandbox::migrate_legacy_sandboxed_agent_configuration() {
            Ok(sandbox::LegacyAgentConfigurationMigration::Moved) => {
                log::info!("moved the sandboxed agents' configuration to its new name");
            }
            Ok(sandbox::LegacyAgentConfigurationMigration::NotOwnDirectory) => log::warn!(
                "not moving the sandboxed agents' configuration: the old one is not a \
                 directory of this account's own"
            ),
            Ok(_) => {}
            Err(error) => {
                log::warn!("could not move the sandboxed agents' configuration: {error:#}");
            }
        }
        daemon::serve(listener, registry.downgrade(), cx);
        cx.set_global(DaemonSessions(registry));

        cx.background_spawn(async move {
            cleanup_old_binaries_wsl();
            cleanup_old_binaries()
        })
        .detach_and_log_err(cx);
        cx.background_spawn({
            let daemon_paths = daemon_paths.clone();
            async move { sweep_stale_state_and_logs(sweep_fs.as_ref(), &daemon_paths).await }
        })
        .detach();
        cx.background_spawn({
            let daemon_paths = daemon_paths.clone();
            let executor = cx.background_executor().clone();
            async move {
                loop {
                    executor.timer(stale_files::STATE_DIR_HEARTBEAT).await;
                    daemon_paths
                        .mark_in_use_by(std::process::id())
                        .context("marking the daemon's state directory as in use")
                        .log_err();
                }
            }
        })
        .detach();
    };
    // We do not reuse any of the state after unwinding, so we don't run risk of observing broken invariants.
    let app = std::panic::AssertUnwindSafe(app);
    let run = std::panic::AssertUnwindSafe(run);
    let res = std::panic::catch_unwind(move || { app }.0.run({ run }.0));
    if let Err(_) = res {
        log::error!("app panicked. quitting.");
        Err(anyhow::anyhow!("panicked"))
    } else {
        log::info!("gpui app is shut down. quitting.");
        Ok(())
    }
}

/// Clears out what daemons of other builds and other client machines left
/// behind (F21). Never stops this daemon from serving: whatever cannot be
/// removed is logged and kept.
async fn sweep_stale_state_and_logs(fs: &dyn Fs, daemon_paths: &DaemonPaths) {
    // Only the pids the pid files name are looked up: listing every process
    // on a busy shared host costs far more than the handful of files here.
    let pid_is_alive = |pid: u32| {
        let pid = sysinfo::Pid::from_u32(pid);
        let mut system = sysinfo::System::new();
        system.refresh_processes_specifics(
            sysinfo::ProcessesToUpdate::Some(&[pid]),
            false,
            sysinfo::ProcessRefreshKind::nothing(),
        );
        system.process(pid).is_some()
    };
    let outcome = stale_files::StaleFileSweep {
        state_root: paths::remote_server_state_dir(),
        logs_dir: logs_dir(),
        own_state_dir: &daemon_paths.state_dir,
        own_log_file: &daemon_paths.log_file,
        now: std::time::SystemTime::now(),
        pid_is_alive: &pid_is_alive,
        lock_is_held: &stale_files::lock_file_is_held,
    }
    .run(fs)
    .await;
    if !outcome.removed_state_dirs.is_empty() || !outcome.removed_logs.is_empty() {
        log::info!(
            "removed {} stale state directories and {} stale logs",
            outcome.removed_state_dirs.len(),
            outcome.removed_logs.len()
        );
    }
}

fn bind_daemon_socket(paths: &DaemonPaths) -> Result<UnixListener> {
    // The proxy that started this process holds the daemon lock, so a socket
    // still here belongs to a daemon that is gone.
    remove_file_if_present(&paths.socket).context("failed to remove a stale daemon socket")?;
    let listener = UnixListener::bind(&paths.socket).context("failed to bind the daemon socket")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&paths.socket, std::fs::Permissions::from_mode(0o600))
            .context("failed to restrict the daemon socket to this user")?;
    }
    write_pid_file(&paths.pid_file, std::process::id())
        .with_context(|| format!("failed to write pid file: {:?}", paths.pid_file))?;
    Ok(listener)
}

/// Every session of the daemon draws on one process's file descriptors —
/// its connections, its worktrees' watches, its language servers' pipes —
/// where each project used to have a process, and a limit, of its own.
#[cfg(unix)]
fn raise_open_file_limit() {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `getrlimit` only writes to `limit`, which outlives the call.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        log::warn!(
            "could not read the open file limit: {}",
            std::io::Error::last_os_error()
        );
        return;
    }
    if limit.rlim_cur >= limit.rlim_max {
        return;
    }
    let raised = libc::rlimit {
        rlim_cur: limit.rlim_max,
        rlim_max: limit.rlim_max,
    };
    // SAFETY: `setrlimit` only reads `raised`, which outlives the call.
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &raised) } == 0 {
        log::info!(
            "raised the open file limit from {} to {}",
            limit.rlim_cur,
            limit.rlim_max
        );
    } else {
        log::warn!(
            "could not raise the open file limit from {} to {}: {}",
            limit.rlim_cur,
            limit.rlim_max,
            std::io::Error::last_os_error()
        );
    }
}

#[derive(Debug, Error)]
pub enum ServerPathError {
    #[error("Failed to create server_dir `{path}`")]
    CreateServerDir {
        #[source]
        source: std::io::Error,
        path: PathBuf,
    },
    #[error("Failed to create logs_dir `{path}`")]
    CreateLogsDir {
        #[source]
        source: std::io::Error,
        path: PathBuf,
    },
}

/// Where a daemon keeps its socket, pid, lock and log (ADR-F45).
#[derive(Clone, Debug)]
struct DaemonPaths {
    state_dir: PathBuf,
    socket: PathBuf,
    pid_file: PathBuf,
    lock_file: PathBuf,
    log_file: PathBuf,
}

impl DaemonPaths {
    fn new(state_dir: PathBuf, log_file: PathBuf) -> Self {
        Self {
            socket: state_dir.join("daemon.sock"),
            pid_file: state_dir.join("daemon.pid"),
            lock_file: state_dir.join("daemon.lock"),
            state_dir,
            log_file,
        }
    }

    /// The daemon a proxy for `identifier` hands its session to: the one this
    /// very binary runs for the same client on this machine.
    fn for_identifier(identifier: &str) -> Result<Self, ServerPathError> {
        let (executable_size, executable_modified) = this_executable_identity();
        let host = this_host_identity();
        let key = daemon_key(&DaemonKeyParts {
            release_channel: RELEASE_CHANNEL.dev_name(),
            version: VERSION.as_str(),
            executable_size,
            executable_modified,
            client_id: client_id_of_identifier(identifier),
            host: &host,
        });
        let state_dir = paths::remote_server_state_dir().join(format!("d-{key}"));
        let create_error = |source| ServerPathError::CreateServerDir {
            source,
            path: state_dir.clone(),
        };
        std::fs::create_dir_all(&state_dir).map_err(create_error)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            // Only this user's proxies have any business with the socket in
            // here.
            std::fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o700))
                .map_err(create_error)?;
        }
        let log_dir = logs_dir();
        std::fs::create_dir_all(log_dir).map_err(|source| ServerPathError::CreateLogsDir {
            source,
            path: log_dir.clone(),
        })?;
        Ok(Self::new(
            state_dir,
            log_dir.join(format!("server-d-{key}.log")),
        ))
    }

    /// Refreshes the modification time of the pid file, provided it still
    /// names `pid`. A daemon connected to for longer than the sweep's age limit
    /// changes nothing else in its directory, and a daemon on another host
    /// sharing this home directory has a pid no process check here can see.
    fn mark_in_use_by(&self, pid: u32) -> Result<()> {
        if read_pid(&self.pid_file) != Some(pid) {
            return Ok(());
        }
        std::fs::OpenOptions::new()
            .write(true)
            .open(&self.pid_file)?
            .set_modified(std::time::SystemTime::now())?;
        Ok(())
    }

    /// Removes the socket and the pid file, provided the pid file still names
    /// `pid`: once a newer daemon has taken the directory over, they are its.
    fn remove_if_owned_by(&self, pid: u32) {
        if read_pid(&self.pid_file) != Some(pid) {
            return;
        }
        for path in [&self.socket, &self.pid_file] {
            remove_file_if_present(path).log_err();
        }
    }
}

/// The size and modification time of this binary, which tell two builds of
/// one version apart (see [`DaemonKeyParts`]).
fn this_executable_identity() -> (u64, Option<Duration>) {
    match std::env::current_exe().and_then(std::fs::metadata) {
        Ok(metadata) => (
            metadata.len(),
            metadata
                .modified()
                .ok()
                .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok()),
        ),
        Err(error) => {
            log::warn!(
                "could not read this binary's size and modification time ({error}); \
                 builds that share a version will share a daemon"
            );
            (0, None)
        }
    }
}

/// Tells apart hosts that share a home directory (see [`DaemonKeyParts`]).
/// The machine id catches hosts that were given the same name; the name
/// catches hosts cloned from one image, which share a machine id.
pub(crate) fn this_host_identity() -> String {
    let host_name = sysinfo::System::host_name().unwrap_or_default();
    let machine_id = ["/etc/machine-id", "/var/lib/dbus/machine-id"]
        .iter()
        .find_map(|path| std::fs::read_to_string(path).ok())
        .map(|machine_id| machine_id.trim().to_string())
        .unwrap_or_default();
    format!("{host_name}\n{machine_id}")
}

fn read_pid(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|contents| contents.trim().parse().ok())
}

fn remove_file_if_present(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[derive(Debug, Error)]
pub enum ExecuteProxyError {
    #[error("Failed to init server paths: {0:#}")]
    ServerPath(#[from] ServerPathError),

    #[error(transparent)]
    ServerNotRunning(#[from] ProxyLaunchError),

    #[error("Failed to check PidFile '{path}': {source:#}")]
    CheckPidFile {
        #[source]
        source: CheckPidError,
        path: PathBuf,
    },

    #[error("Failed to kill existing server with pid '{pid}'")]
    KillRunningServer { pid: u32 },

    #[error("failed to take the daemon lock '{path}'")]
    DaemonLock {
        #[source]
        source: std::io::Error,
        path: PathBuf,
    },

    #[error("failed to spawn server")]
    SpawnServer(#[source] SpawnServerError),

    #[error("failed to introduce this proxy to the daemon: {0:#}")]
    Handshake(#[source] anyhow::Error),

    #[error("the daemon refused this proxy: {0}")]
    DaemonRejected(String),

    #[error("stdin_task failed: {0:#}")]
    StdinTask(#[source] anyhow::Error),
    #[error("stdout_task failed: {0:#}")]
    StdoutTask(#[source] anyhow::Error),
}

impl ExecuteProxyError {
    pub fn to_exit_code(&self) -> i32 {
        match self {
            ExecuteProxyError::ServerNotRunning(proxy_launch_error) => {
                proxy_launch_error.to_exit_code()
            }
            _ => 1,
        }
    }
}

fn server_not_running() -> ExecuteProxyError {
    ExecuteProxyError::ServerNotRunning(ProxyLaunchError::ServerNotRunning)
}

pub(crate) fn execute_proxy(
    identifier: String,
    is_reconnecting: bool,
) -> Result<(), ExecuteProxyError> {
    init_logging_proxy();

    let daemon_paths = DaemonPaths::for_identifier(&identifier)?;

    let id = std::process::id().to_string();
    let should_install_crash_handler =
        client::telemetry::should_install_crash_handler(*RELEASE_CHANNEL);

    if should_install_crash_handler {
        smol::spawn(crashes::init(
            crashes::InitCrashHandler {
                session_id: id,
                zed_version: VERSION.to_owned(),
                binary: "zed-remote-proxy".to_string(),
                release_channel: release_channel::RELEASE_CHANNEL_NAME.clone(),
                commit_sha: option_env!("ZED_COMMIT_SHA").unwrap_or("no_sha").to_owned(),
            },
            |task| {
                smol::spawn(task).detach();
            },
            |pid| paths::temp_dir().join(format!("zed-remote-server-proxy-crash-handler-{pid}")),
            // we are running outside gpui
            #[allow(clippy::disallowed_methods)]
            |duration| FutureExt::map(Timer::after(duration), |_| ()),
        ))
        .detach();
    };
    log::info!(
        "starting proxy process. PID: {}, daemon: {:?}",
        std::process::id(),
        daemon_paths.state_dir
    );

    let hello = DaemonHello {
        protocol_version: DAEMON_PROTOCOL_VERSION,
        identifier,
        reconnect: is_reconnecting,
        ssh_auth_sock: std::env::var("SSH_AUTH_SOCK").ok(),
    };
    let connector = DaemonConnector::new(daemon_paths.clone());
    let connection = gpui::block_on(connector.connect(&hello))?;

    if let Err(forwarding_error) = gpui::block_on(forward_stdio(connection)) {
        log::error!("encountered error while forwarding messages: {forwarding_error:#}",);
        if !daemon_is_running(&daemon_paths) {
            log::error!("the daemon exited unexpectedly");
            return Err(server_not_running());
        }
        return Err(forwarding_error);
    }

    Ok(())
}

/// How long a daemon may take to answer an introduction before a proxy takes
/// it for hung and replaces it. Long, because replacing a daemon ends every
/// session it serves: an answer that is merely slow — a loaded host, a daemon
/// busy opening a session — must not be mistaken for a hung process.
const UNRESPONSIVE_DAEMON_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a freshly spawned daemon has to start accepting connections. It
/// binds its socket only once it is ready to serve.
const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a proxy waits for another one to finish starting the daemon. Has
/// to outlast [`DAEMON_START_TIMEOUT`], which the other proxy may spend in
/// full while holding the lock.
const DAEMON_LOCK_TIMEOUT: Duration = Duration::from_secs(90);
const DAEMON_POLL_INTERVAL: Duration = Duration::from_millis(50);
/// A daemon that closes the connection is usually on its way out, which is
/// over within moments; a few attempts see it gone and a new one started.
const CONNECT_ATTEMPTS: usize = 3;

/// Waits without an executor to ask for a timer, which the proxy does not
/// have.
async fn sleep(duration: Duration) {
    #[allow(clippy::disallowed_methods, reason = "the proxy runs outside gpui")]
    let timer = Timer::after(duration);
    timer.await;
}

/// What tells one daemon from its successor: its pid and its socket file.
/// Both are read before connecting, so that a daemon found hung is only
/// replaced if it is still the one that was found hung.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DaemonIdentity {
    pid: Option<u32>,
    socket: Option<(u64, u64)>,
}

impl DaemonIdentity {
    fn of(paths: &DaemonPaths) -> Self {
        Self {
            pid: read_pid(&paths.pid_file),
            socket: socket_identity(&paths.socket),
        }
    }
}

#[cfg(unix)]
fn socket_identity(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt as _;
    let metadata = std::fs::symlink_metadata(path).ok()?;
    Some((metadata.dev(), metadata.ino()))
}

/// Windows has no inode number to read here; the file's creation time tells a
/// socket from its successor just as well.
#[cfg(windows)]
fn socket_identity(path: &Path) -> Option<(u64, u64)> {
    let created = std::fs::symlink_metadata(path)
        .ok()?
        .created()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?;
    Some((created.as_secs(), u64::from(created.subsec_nanos())))
}

/// What became of a proxy's introduction to the daemon.
enum Introduction {
    Accepted(UnixStream),
    SessionNotFound,
    /// The daemon closed the connection: it was most likely on its way out.
    Closed(anyhow::Error),
    /// The daemon kept the connection but did not answer.
    TimedOut,
}

/// Starts this binary as the daemon for `paths`.
type SpawnDaemon = Box<dyn Fn(&DaemonPaths) -> Result<(), SpawnServerError>>;

/// How a proxy reaches — and when it has to, starts or replaces — the daemon
/// for its client (ADR-F45).
struct DaemonConnector {
    paths: DaemonPaths,
    /// A seam for tests, which cannot start this binary as a daemon.
    spawn: SpawnDaemon,
    reply_timeout: Duration,
    start_timeout: Duration,
    lock_timeout: Duration,
}

impl DaemonConnector {
    fn new(paths: DaemonPaths) -> Self {
        Self {
            paths,
            spawn: Box::new(spawn_daemon),
            reply_timeout: UNRESPONSIVE_DAEMON_TIMEOUT,
            start_timeout: DAEMON_START_TIMEOUT,
            lock_timeout: DAEMON_LOCK_TIMEOUT,
        }
    }

    async fn connect(&self, hello: &DaemonHello) -> Result<UnixStream, ExecuteProxyError> {
        if hello.reconnect {
            let Ok(connection) = UnixStream::connect(&self.paths.socket).await else {
                log::error!("attempted to reconnect, but no daemon is running");
                return Err(server_not_running());
            };
            return match self.introduce(connection, hello).await? {
                Introduction::Accepted(connection) => Ok(connection),
                Introduction::SessionNotFound => {
                    log::error!(
                        "attempted to reconnect, but the daemon no longer has this session"
                    );
                    Err(server_not_running())
                }
                Introduction::Closed(error) => {
                    log::error!(
                        "attempted to reconnect, but the daemon closed the connection: {error:#}"
                    );
                    Err(server_not_running())
                }
                // Not "server not running": for a daemon that is merely slow
                // that would cost the client its session. A plain failure
                // lets the client try again.
                Introduction::TimedOut => Err(ExecuteProxyError::Handshake(anyhow!(
                    "the daemon did not answer within {:?}",
                    self.reply_timeout
                ))),
            };
        }

        for _ in 0..CONNECT_ATTEMPTS {
            let (connection, identity) = self.ensure_running().await?;
            match self.introduce(connection, hello).await? {
                Introduction::Accepted(connection) => return Ok(connection),
                Introduction::SessionNotFound => {
                    return Err(ExecuteProxyError::Handshake(anyhow!(
                        "the daemon found no session to start, which a fresh connection never asks it for"
                    )));
                }
                Introduction::Closed(error) => {
                    log::info!("the daemon closed the connection ({error:#}); trying again");
                    sleep(DAEMON_POLL_INTERVAL).await;
                }
                Introduction::TimedOut => {
                    log::warn!(
                        "the daemon did not answer within {:?}; replacing it",
                        self.reply_timeout
                    );
                    self.restart(identity).await?;
                }
            }
        }
        Err(ExecuteProxyError::Handshake(anyhow!(
            "no daemon answered in {CONNECT_ATTEMPTS} attempts"
        )))
    }

    async fn introduce(
        &self,
        mut connection: UnixStream,
        hello: &DaemonHello,
    ) -> Result<Introduction, ExecuteProxyError> {
        if let Err(error) = write_json_message(&mut connection, hello).await {
            return Ok(Introduction::Closed(error));
        }
        let reply = {
            let reply = read_json_message::<_, DaemonHelloReply>(&mut connection);
            let deadline = sleep(self.reply_timeout);
            smol::future::or(async { Some(reply.await) }, async {
                deadline.await;
                None
            })
            .await
        };
        let reply = match reply {
            None => return Ok(Introduction::TimedOut),
            Some(Err(error)) => return Ok(Introduction::Closed(error)),
            Some(Ok(reply)) => reply,
        };
        match reply.outcome {
            DaemonHelloOutcome::Accepted => Ok(Introduction::Accepted(connection)),
            DaemonHelloOutcome::SessionNotFound => Ok(Introduction::SessionNotFound),
            DaemonHelloOutcome::Rejected => Err(ExecuteProxyError::DaemonRejected(
                reply.message.unwrap_or_default(),
            )),
        }
    }

    async fn ensure_running(&self) -> Result<(UnixStream, DaemonIdentity), ExecuteProxyError> {
        let identity = DaemonIdentity::of(&self.paths);
        if let Ok(connection) = UnixStream::connect(&self.paths.socket).await {
            return Ok((connection, identity));
        }
        let _lock = DaemonLock::acquire(&self.paths.lock_file, self.lock_timeout).await?;
        // Another proxy may have started one while this one waited for the
        // lock.
        let identity = DaemonIdentity::of(&self.paths);
        if let Ok(connection) = UnixStream::connect(&self.paths.socket).await {
            return Ok((connection, identity));
        }
        self.start_locked().await
    }

    /// Replaces the daemon that did not answer, `unanswered`. If another
    /// proxy has replaced it in the meantime, its successor is left alone.
    async fn restart(&self, unanswered: DaemonIdentity) -> Result<(), ExecuteProxyError> {
        let _lock = DaemonLock::acquire(&self.paths.lock_file, self.lock_timeout).await?;
        if DaemonIdentity::of(&self.paths) != unanswered {
            log::info!("another proxy has replaced the daemon already");
            return Ok(());
        }
        self.start_locked().await.map(drop)
    }

    /// Starts a daemon in place of whatever `paths` records. The caller holds
    /// the daemon lock, and has found the recorded daemon refusing
    /// connections or hung.
    async fn start_locked(&self) -> Result<(UnixStream, DaemonIdentity), ExecuteProxyError> {
        let recorded = check_pid_file(&self.paths.pid_file).map_err(|source| {
            ExecuteProxyError::CheckPidFile {
                source,
                path: self.paths.pid_file.clone(),
            }
        })?;
        if let Some(pid) = recorded {
            kill_daemon(pid, &self.paths)?;
        }
        remove_file_if_present(&self.paths.socket).map_err(|error| {
            ExecuteProxyError::SpawnServer(SpawnServerError::RemoveSocket(error))
        })?;
        (self.spawn)(&self.paths).map_err(ExecuteProxyError::SpawnServer)?;
        let connection = wait_for_daemon(&self.paths, self.start_timeout)
            .await
            .map_err(ExecuteProxyError::SpawnServer)?;
        Ok((connection, DaemonIdentity::of(&self.paths)))
    }
}

/// Serializes starting the daemon, so that proxies started together — a client
/// restoring several windows at once — end up with one daemon between them.
///
/// The operating system releases the lock when the file is closed, which
/// includes the process dying: a crashed proxy never keeps the next one out.
struct DaemonLock {
    _file: File,
}

impl DaemonLock {
    async fn acquire(path: &Path, timeout: Duration) -> Result<Self, ExecuteProxyError> {
        let lock_error = |source: std::io::Error| ExecuteProxyError::DaemonLock {
            source,
            path: path.to_path_buf(),
        };
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .map_err(lock_error)?;
        let deadline = Instant::now() + timeout;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Self { _file: file }),
                Err(std::fs::TryLockError::WouldBlock) => {}
                Err(std::fs::TryLockError::Error(error)) => return Err(lock_error(error)),
            }
            if Instant::now() >= deadline {
                return Err(lock_error(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "another proxy held the daemon lock for too long",
                )));
            }
            sleep(DAEMON_POLL_INTERVAL).await;
        }
    }
}

/// The command line of the process with `pid`, if it is running.
fn command_line_of(pid: u32) -> Option<Vec<OsString>> {
    let system =
        sysinfo::System::new_with_specifics(sysinfo::RefreshKind::nothing().with_processes(
            sysinfo::ProcessRefreshKind::nothing().with_cmd(sysinfo::UpdateKind::Always),
        ));
    system
        .process(sysinfo::Pid::from_u32(pid))
        .map(|process| process.cmd().to_vec())
}

/// Whether `arguments` start the daemon that serves `state_dir`. Checked
/// before a pid read from a file is trusted with anything: the file can
/// outlive its process, and the pid can by then belong to anything. The
/// command line, unlike the executable's path, also survives the binary being
/// replaced under a running daemon.
fn is_daemon_command_line(arguments: &[OsString], state_dir: &Path) -> bool {
    let has_subcommand = arguments
        .iter()
        .skip(1)
        .any(|argument| argument == "daemon");
    let serves_state_dir = arguments.windows(2).any(|pair| {
        matches!(pair, [flag, value] if flag == "--state-dir" && Path::new(value) == state_dir)
    });
    has_subcommand && serves_state_dir
}

fn daemon_is_running(paths: &DaemonPaths) -> bool {
    read_pid(&paths.pid_file)
        .and_then(command_line_of)
        .is_some_and(|arguments| is_daemon_command_line(&arguments, &paths.state_dir))
}

/// Stops the daemon recorded in `paths`, provided `pid` really is it.
fn kill_daemon(pid: u32, paths: &DaemonPaths) -> Result<(), ExecuteProxyError> {
    let system =
        sysinfo::System::new_with_specifics(sysinfo::RefreshKind::nothing().with_processes(
            sysinfo::ProcessRefreshKind::nothing().with_cmd(sysinfo::UpdateKind::Always),
        ));

    if let Some(process) = system.process(sysinfo::Pid::from_u32(pid)) {
        if is_daemon_command_line(process.cmd(), &paths.state_dir) {
            log::info!("killing the daemon with PID {pid}");
            if !process.kill() {
                return Err(ExecuteProxyError::KillRunningServer { pid });
            }
        } else {
            log::warn!(
                "PID {pid} in {:?} is not the daemon of {:?}; leaving it alone",
                paths.pid_file,
                paths.state_dir
            );
        }
    }

    for path in [&paths.pid_file, &paths.socket] {
        log::debug!("cleaning up {path:?} before starting a new daemon");
        remove_file_if_present(path).log_err();
    }

    Ok(())
}

#[derive(Debug, Error)]
pub enum SpawnServerError {
    #[error("failed to remove the stale daemon socket")]
    RemoveSocket(#[source] std::io::Error),

    #[error("failed to get current_exe")]
    CurrentExe(#[source] std::io::Error),

    #[error("failed to launch server process")]
    ProcessStatus(#[source] std::io::Error),

    #[error("failed to wait for server to be ready to accept connections")]
    Timeout,
}

fn spawn_daemon(paths: &DaemonPaths) -> Result<(), SpawnServerError> {
    log::info!("spawning the daemon");
    let binary_name = std::env::current_exe().map_err(SpawnServerError::CurrentExe)?;

    #[cfg(windows)]
    {
        spawn_daemon_windows(&binary_name, paths)?;
    }

    #[cfg(not(windows))]
    {
        spawn_daemon_normal(&binary_name, paths)?;
    }

    Ok(())
}

async fn wait_for_daemon(
    paths: &DaemonPaths,
    timeout: Duration,
) -> Result<UnixStream, SpawnServerError> {
    let started = Instant::now();
    loop {
        if paths.socket.exists()
            && let Ok(connection) = UnixStream::connect(&paths.socket).await
        {
            log::info!(
                "the daemon is accepting connections. total time waited: {:?}",
                started.elapsed()
            );
            return Ok(connection);
        }
        if started.elapsed() > timeout {
            return Err(SpawnServerError::Timeout);
        }
        log::debug!("waiting for the daemon to accept connections...");
        sleep(DAEMON_POLL_INTERVAL).await;
    }
}

#[cfg(windows)]
fn spawn_daemon_windows(binary_name: &Path, paths: &DaemonPaths) -> Result<(), SpawnServerError> {
    let binary_path = binary_name.to_string_lossy().to_string();
    let parameters = format!(
        "daemon --log-file \"{}\" --state-dir \"{}\"",
        paths.log_file.to_string_lossy(),
        paths.state_dir.to_string_lossy()
    );

    let directory = binary_name
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();

    crate::windows::shell_execute_from_explorer(&binary_path, &parameters, &directory)
        .map_err(|e| SpawnServerError::ProcessStatus(std::io::Error::other(e)))?;

    Ok(())
}

#[cfg(not(windows))]
fn spawn_daemon_normal(binary_name: &Path, paths: &DaemonPaths) -> Result<(), SpawnServerError> {
    let mut server_process = util::command::new_command(binary_name);
    server_process
        .stdin(util::command::Stdio::null())
        .stdout(util::command::Stdio::null())
        .stderr(util::command::Stdio::null())
        .arg("daemon")
        .arg("--log-file")
        .arg(&paths.log_file)
        .arg("--state-dir")
        .arg(&paths.state_dir);

    server_process
        .spawn()
        .map_err(SpawnServerError::ProcessStatus)?;

    Ok(())
}

#[derive(Debug, Error)]
#[error("Failed to remove PID file for missing process (pid `{pid}`")]
pub struct CheckPidError {
    #[source]
    source: std::io::Error,
    pid: u32,
}

fn check_pid_file(path: &Path) -> Result<Option<u32>, CheckPidError> {
    let Some(pid) = std::fs::read_to_string(path)
        .ok()
        .and_then(|contents| contents.parse::<u32>().ok())
    else {
        return Ok(None);
    };

    log::debug!("Checking if process with PID {} exists...", pid);

    let system = sysinfo::System::new_with_specifics(
        sysinfo::RefreshKind::nothing().with_processes(sysinfo::ProcessRefreshKind::nothing()),
    );

    if system.process(sysinfo::Pid::from_u32(pid)).is_some() {
        log::debug!("Process with PID {} exists.", pid);
        Ok(Some(pid))
    } else {
        log::debug!("Found PID file, but process with that PID does not exist. Removing PID file.");
        std::fs::remove_file(path).map_err(|source| CheckPidError { source, pid })?;
        Ok(None)
    }
}

fn write_pid_file(path: &Path, pid: u32) -> Result<()> {
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    log::debug!("writing PID {} to file {:?}", pid, path);
    std::fs::write(path, pid.to_string()).context("Failed to write PID file")
}

async fn forward_stdio(connection: UnixStream) -> Result<(), ExecuteProxyError> {
    forward(
        connection,
        smol::Unblock::new(std::io::stdin()),
        smol::Unblock::new(std::io::stdout()),
        smol::Unblock::new(std::io::stderr()),
    )
    .await
}

/// Relays between this proxy's stdio, which the client's ssh (or wsl, or
/// docker) process is attached to, and the daemon. The client's side is the
/// same length-prefixed envelopes on stdin and stdout, plus the server's log on
/// stderr, that a per-project server used to serve directly.
async fn forward<C, I, O, E>(
    connection: C,
    mut stdin: I,
    mut stdout: O,
    mut stderr: E,
) -> Result<(), ExecuteProxyError>
where
    C: AsyncRead + AsyncWrite + Send + 'static,
    I: AsyncRead + Unpin + Send + 'static,
    O: AsyncWrite + Unpin + Send + 'static,
    E: AsyncWrite + Unpin + Send + 'static,
{
    let (mut from_daemon, mut to_daemon) = futures::io::AsyncReadExt::split(connection);

    let stdin_task: smol::Task<Result<()>> = smol::spawn(async move {
        let mut buffer = Vec::new();
        loop {
            read_message_raw(&mut stdin, &mut buffer)
                .await
                .context("failed to read message from stdin")?;
            write_daemon_frame(&mut to_daemon, DaemonFrameKind::Envelope, &buffer)
                .await
                .context("failed to write message to the daemon")?;
            to_daemon.flush().await?;
        }
    });

    let stdout_task: smol::Task<Result<()>> = smol::spawn(async move {
        let mut buffer = Vec::new();
        loop {
            match read_daemon_frame(&mut from_daemon, &mut buffer)
                .await
                .context("failed to read from the daemon")?
            {
                DaemonFrameKind::Envelope => {
                    write_size_prefixed_buffer(&mut stdout, &mut buffer)
                        .await
                        .context("failed to write message to stdout")?;
                    stdout.flush().await?;
                }
                DaemonFrameKind::Log => {
                    stderr.write_all(&buffer).await?;
                    stderr.flush().await?;
                }
            }
        }
    });

    futures::select! {
        result = stdin_task.fuse() => result.map_err(ExecuteProxyError::StdinTask),
        result = stdout_task.fuse() => result.map_err(ExecuteProxyError::StdoutTask),
    }
}

fn initialize_settings(
    router: SessionRouter,
    fs: Arc<dyn Fs>,
    cx: &mut App,
) -> watch::Receiver<Option<NodeBinaryOptions>> {
    let (user_settings_file_rx, watcher_task) =
        watch_config_file(cx.background_executor(), fs, paths::settings_file().clone());

    handle_settings_file_changes(user_settings_file_rx, watcher_task, cx, {
        move |err, _cx| {
            if let Some(e) = &err {
                log::info!("Server settings failed to change: {}", e);
            }
            // Every session has a window of its own that should say so.
            for session in router.all_clients() {
                if let Some(e) = &err {
                    session
                        .send(proto::Toast {
                            project_id: REMOTE_SERVER_PROJECT_ID,
                            notification_id: "server-settings-failed".to_string(),
                            message: format!(
                                "Error in settings on remote host {:?}: {}",
                                paths::settings_file(),
                                e
                            ),
                        })
                        .log_err();
                } else {
                    session
                        .send(proto::HideToast {
                            project_id: REMOTE_SERVER_PROJECT_ID,
                            notification_id: "server-settings-failed".to_string(),
                        })
                        .log_err();
                }
            }
        }
    });

    let (mut tx, rx) = watch::channel(None);
    let mut node_settings = None;
    cx.observe_global::<SettingsStore>(move |cx| {
        let new_node_settings = &ProjectSettings::get_global(cx).node;
        if Some(new_node_settings) != node_settings.as_ref() {
            log::info!("Got new node settings: {new_node_settings:?}");
            let options = NodeBinaryOptions {
                allow_path_lookup: !new_node_settings.ignore_system_version,
                // TODO: Implement this setting
                allow_binary_download: true,
                use_paths: new_node_settings.path.as_ref().map(|node_path| {
                    let node_path = PathBuf::from(shellexpand::tilde(node_path).as_ref());
                    let npm_path = new_node_settings
                        .npm_path
                        .as_ref()
                        .map(|path| PathBuf::from(shellexpand::tilde(&path).as_ref()));
                    (
                        node_path.clone(),
                        npm_path.unwrap_or_else(|| {
                            let base_path = PathBuf::new();
                            node_path.parent().unwrap_or(&base_path).join("npm")
                        }),
                    )
                }),
            };
            node_settings = Some(new_node_settings.clone());
            tx.send(Some(options)).ok();
        }
    })
    .detach();

    rx
}

pub fn handle_settings_file_changes(
    mut server_settings_file: mpsc::UnboundedReceiver<String>,
    watcher_task: gpui::Task<()>,
    cx: &mut App,
    settings_changed: impl Fn(Option<anyhow::Error>, &mut App) + 'static,
) {
    let Some(server_settings_content) = cx
        .foreground_executor()
        .block_on(server_settings_file.next())
    else {
        log::error!(
            "the settings file watcher stopped before reporting anything; this host's settings file is not applied"
        );
        return;
    };
    SettingsStore::update_global(cx, |store, cx| {
        store
            .set_server_settings(&server_settings_content, cx)
            .log_err();
    });
    cx.spawn(async move |cx| {
        let _watcher_task = watcher_task;
        while let Some(server_settings_content) = server_settings_file.next().await {
            cx.update_global(|store: &mut SettingsStore, cx| {
                let result = store.set_server_settings(&server_settings_content, cx);
                if let Err(err) = &result {
                    log::error!("Failed to load server settings: {err}");
                }
                settings_changed(result.err(), cx);
                cx.refresh_windows();
            });
        }
    })
    .detach();
}

/// Whether this host relays its outgoing HTTP through the connected IDE
/// (ADR-F27, `docs/jp-enhance/DESIGN.md` §4.27).
///
/// The setting is written on the client and reaches this process with the rest
/// of the user settings, so it is not readable when the HTTP client is built —
/// which is why [`RemoteHttpClient`] consults it per request instead.
#[derive(Clone, Debug, PartialEq, RegisterSetting)]
pub struct RemoteHostSettings {
    pub relay_http_through_client: bool,
}

impl Settings for RemoteHostSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let remote_host = content.remote_host.clone().unwrap_or_default();
        Self {
            relay_http_through_client: remote_host.relay_http_through_client.unwrap_or(true),
        }
    }
}

/// Sends this host's HTTP either through the IDE (ADR-F27) or straight out of
/// this machine, deciding per request.
///
/// Deciding per request rather than at construction is what makes the setting
/// take effect without a reconnect: the client pushes its settings after the
/// connection is up, and pushes them again whenever they change.
struct RemoteHttpClient {
    relay: RemoteProxyHttpClient,
    direct: Arc<dyn HttpClient>,
    relay_enabled: Arc<AtomicBool>,
}

impl HttpClient for RemoteHttpClient {
    fn user_agent(&self) -> Option<&http_client::http::HeaderValue> {
        self.direct.user_agent()
    }

    fn proxy(&self) -> Option<&Url> {
        self.direct.proxy()
    }

    fn send(
        &self,
        request: Request<AsyncBody>,
    ) -> futures::future::BoxFuture<'static, Result<Response<AsyncBody>>> {
        if self.relay_enabled.load(Ordering::Relaxed) {
            self.relay.send(request)
        } else {
            self.direct.send(request)
        }
    }
}

/// Tracks `remote.relay_http_through_client` for [`RemoteHttpClient`].
///
/// Starts out on, which is both the setting's default and the safe answer for
/// the window before the client has pushed anything: a host with no internet
/// access would otherwise spend that window failing downloads it could have
/// relayed.
fn watch_relay_http_setting(cx: &mut App) -> Arc<AtomicBool> {
    let enabled = Arc::new(AtomicBool::new(true));
    cx.observe_global::<SettingsStore>({
        let enabled = enabled.clone();
        move |cx| {
            let relay = RemoteHostSettings::get_global(cx).relay_http_through_client;
            if enabled.swap(relay, Ordering::Relaxed) != relay {
                log::info!(
                    "relaying this host's HTTP through the connected IDE (ADR-F27): {relay}"
                );
            }
        }
    })
    .detach();
    enabled
}

fn read_proxy_settings(cx: &App) -> Option<Url> {
    let proxy_str = ProxySettings::get_global(cx).proxy.to_owned();

    proxy_str
        .as_deref()
        .map(str::trim)
        .filter(|input| !input.is_empty())
        .and_then(|input| {
            input
                .parse::<Url>()
                .inspect_err(|e| log::error!("Error parsing proxy settings: {}", e))
                .ok()
        })
        .or_else(read_proxy_from_env)
}

fn cleanup_old_binaries() -> Result<()> {
    let server_dir = paths::remote_server_dir_relative();
    let release_channel = *release_channel::RELEASE_CHANNEL;

    for entry in std::fs::read_dir(server_dir.as_std_path())? {
        let path = entry?.path();

        if let Some(file_name) = path.file_name()
            && let Some(version) = own_binary_version(&file_name.to_string_lossy(), release_channel)
            && !is_new_version(&version)
            && !is_file_in_use(file_name)
        {
            log::info!("removing old remote server binary: {:?}", path);
            std::fs::remove_file(&path)?;
        }
    }

    Ok(())
}

// Remove this once 223 goes stable, we only have this to clean up old binaries on WSL
// we no longer download them into this folder, we use the same folder as other remote servers
fn cleanup_old_binaries_wsl() {
    let server_dir = paths::remote_wsl_server_dir_relative();
    if let Ok(()) = std::fs::remove_dir_all(server_dir.as_std_path()) {
        log::info!("removing old wsl remote server folder: {:?}", server_dir);
    }
}

/// The version part of `file_name` when it is a server binary of
/// `release_channel` that a build of this fork installed, under its current
/// name or an earlier one. Upstream Zed keeps its own server binaries in the
/// same directory, under the name this fork had before; those lack the
/// `-jp<revision>` ending and are never this fork's to remove.
fn own_binary_version(
    file_name: &str,
    release_channel: release_channel::ReleaseChannel,
) -> Option<String> {
    if !remote::is_fork_server_binary_name(file_name) {
        return None;
    }
    std::iter::once(remote::server_binary_name_prefix(release_channel))
        .chain(remote::legacy_server_binary_name_prefixes(release_channel))
        .find_map(|prefix| file_name.strip_prefix(&prefix).map(str::to_string))
}

fn is_new_version(version: &str) -> bool {
    is_version_at_least(
        version,
        env!("ZED_PKG_VERSION"),
        rpc::proto::jp_enhance::JP_ENHANCE_WIRE_REVISION,
    )
}

/// `version` is what follows the channel in a binary's name, which for this
/// fork ends in `-jp{wire revision}` (`remote::transport::server_binary_version`).
/// Semver reads that as a pre-release, which sorts below the plain version, so
/// the revision is compared on its own: otherwise every binary of this Zed
/// version, a newer revision's just uploaded one included, would count as old.
fn is_version_at_least(version: &str, current_version: &str, current_revision: u32) -> bool {
    let version = version.strip_suffix(".exe").unwrap_or(version);
    let (Ok(version), Ok(current_version)) = (
        semver::Version::from_str(version),
        semver::Version::from_str(current_version),
    ) else {
        return false;
    };
    let revision = version
        .pre
        .as_str()
        .strip_prefix("jp")
        .and_then(|revision| revision.parse::<u32>().ok())
        .unwrap_or(0);
    (version.major, version.minor, version.patch, revision)
        >= (
            current_version.major,
            current_version.minor,
            current_version.patch,
            current_revision,
        )
}

fn is_file_in_use(file_name: &OsStr) -> bool {
    let info = sysinfo::System::new_with_specifics(sysinfo::RefreshKind::nothing().with_processes(
        sysinfo::ProcessRefreshKind::nothing().with_exe(sysinfo::UpdateKind::Always),
    ));

    for process in info.processes().values() {
        if process
            .exe()
            .is_some_and(|exe| exe.file_name().is_some_and(|name| name == file_name))
        {
            return true;
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::test_support::memory_stream_pair;
    use remote::protocol::{decode_envelope, read_message, write_envelope_frame, write_message};
    use rpc::proto::EnvelopedMessage as _;
    use std::cell::RefCell;

    /// Upstream Zed's server binaries share the directory and the prefix this
    /// fork had before it was renamed; only the fork's own, under either name,
    /// are candidates for the old-binary cleanup.
    #[test]
    fn old_binary_cleanup_only_considers_this_forks_binaries() {
        let stable = release_channel::ReleaseChannel::Stable;
        for (file_name, version) in [
            ("forgekairos-remote-server-stable-1.21.0-jp2", "1.21.0-jp2"),
            (
                "forgekairos-remote-server-stable-1.21.0-jp2.exe",
                "1.21.0-jp2.exe",
            ),
            ("zed-remote-server-stable-1.17.2-jp1", "1.17.2-jp1"),
        ] {
            assert_eq!(
                own_binary_version(file_name, stable).as_deref(),
                Some(version),
                "{file_name}"
            );
        }
        for file_name in [
            "zed-remote-server-stable-1.21.0",
            "zed-remote-server-stable-1.21.0.exe",
            "zed-remote-server-stable-1.20.0",
            "forgekairos-remote-server-dev-build-jp2",
            "zed-remote-server-dev-build-jp2",
            "zed-remote-server-stable-1.17.2-jp1-download-7.gz",
            "slirp4netns",
        ] {
            assert_eq!(own_binary_version(file_name, stable), None, "{file_name}");
        }
    }

    #[test]
    fn old_binary_cleanup_compares_the_wire_revision_in_the_name() {
        // What this revision installs, and a newer revision's binary that a
        // client may have just uploaded, are both kept.
        assert!(is_version_at_least("1.17.2-jp1", "1.17.2", 1));
        assert!(is_version_at_least("1.17.2-jp2", "1.17.2", 1));
        assert!(is_version_at_least("1.17.2-jp10", "1.17.2", 2));
        assert!(is_version_at_least("1.17.2-jp2.exe", "1.17.2", 1));
        assert!(is_version_at_least("1.18.0-jp1", "1.17.2", 2));
        // A plain name is from before the revision was part of it, or
        // upstream's own server: a wire format this build cannot speak.
        assert!(!is_version_at_least("1.17.2", "1.17.2", 1));
        assert!(!is_version_at_least("1.17.2.exe", "1.17.2", 1));
        assert!(!is_version_at_least("1.17.2-jp1", "1.17.2", 2));
        assert!(!is_version_at_least("1.17.1-jp9", "1.17.2", 1));
        assert!(!is_version_at_least("build-jp1", "1.17.2", 1));
    }

    #[test]
    fn rotated_remote_log_path_uses_numbered_log_suffix() {
        assert_eq!(
            rotated_log_path(Path::new("server-workspace-12.log")),
            PathBuf::from("server-workspace-12.1.log")
        );
    }

    #[test]
    fn opening_remote_log_rotates_existing_oversized_log() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let log_path = temp_dir.path().join("server-workspace-12.log");
        let rotated_path = temp_dir.path().join("server-workspace-12.1.log");
        let existing_contents = vec![b'x'; REMOTE_SERVER_LOG_MAX_BYTES as usize];
        std::fs::write(&log_path, &existing_contents).expect("write oversized log");

        let _log_file = RotatingLogFile::open(&log_path).expect("open rotating log file");

        assert_eq!(
            std::fs::read(&rotated_path).expect("read rotated log"),
            existing_contents
        );
        assert_eq!(
            std::fs::metadata(&log_path)
                .expect("active log metadata")
                .len(),
            0
        );
    }

    #[test]
    fn writing_remote_log_rotates_before_exceeding_size_limit() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let log_path = temp_dir.path().join("server-workspace-12.log");
        let rotated_path = temp_dir.path().join("server-workspace-12.1.log");
        let existing_contents = vec![b'x'; REMOTE_SERVER_LOG_MAX_BYTES as usize - 1];
        let new_contents = b"yz";
        std::fs::write(&log_path, &existing_contents).expect("write existing log contents");
        let mut log_file = RotatingLogFile::open(&log_path).expect("open rotating log file");

        log_file
            .write_all(new_contents)
            .expect("write log contents");
        log_file.flush().expect("flush log file");

        assert_eq!(
            std::fs::read(&rotated_path).expect("read rotated log"),
            existing_contents
        );
        assert_eq!(
            std::fs::read(&log_path).expect("read active log"),
            new_contents
        );
    }

    /// ADR-F45: several proxies started together have to agree on one daemon,
    /// which is what the lock around starting it is for.
    #[test]
    fn a_second_proxy_waits_for_the_daemon_lock() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let lock_path = temp_dir.path().join("daemon.lock");
        let first = smol::block_on(DaemonLock::acquire(&lock_path, Duration::from_secs(5)))
            .expect("the first proxy should get the lock");
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            drop(first);
        });

        let started = Instant::now();
        let second = smol::block_on(DaemonLock::acquire(&lock_path, Duration::from_secs(5)))
            .expect("the second proxy should get the lock once it is released");
        assert!(
            started.elapsed() >= Duration::from_millis(100),
            "the second proxy must not get the lock while the first holds it"
        );
        releaser.join().expect("the releasing thread should finish");
        drop(second);
    }

    #[test]
    fn a_held_daemon_lock_keeps_the_next_proxy_out_until_it_gives_up() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let lock_path = temp_dir.path().join("daemon.lock");
        let _held = smol::block_on(DaemonLock::acquire(&lock_path, Duration::from_secs(5)))
            .expect("the first proxy should get the lock");
        let second = smol::block_on(DaemonLock::acquire(&lock_path, Duration::from_millis(200)));
        assert!(matches!(second, Err(ExecuteProxyError::DaemonLock { .. })));
    }

    #[test]
    fn the_sweep_sees_a_proxy_holding_the_daemon_lock() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let lock_path = temp_dir.path().join("daemon.lock");
        let held = smol::block_on(DaemonLock::acquire(&lock_path, Duration::from_secs(5)))
            .expect("the proxy should get the lock");
        assert!(stale_files::lock_file_is_held(&lock_path));
        drop(held);
        assert!(!stale_files::lock_file_is_held(&lock_path));
    }

    #[test]
    fn a_running_daemon_keeps_its_state_directory_fresh() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let paths = DaemonPaths::new(temp_dir.path().to_path_buf(), temp_dir.path().join("log"));
        let long_ago = std::time::SystemTime::now() - Duration::from_secs(90 * 24 * 60 * 60);
        let modified = || {
            std::fs::metadata(&paths.pid_file)
                .and_then(|metadata| metadata.modified())
                .expect("read the pid file's modification time")
        };
        let age_pid_file = || {
            std::fs::OpenOptions::new()
                .write(true)
                .open(&paths.pid_file)
                .and_then(|file| file.set_modified(long_ago))
                .expect("age the pid file");
        };
        std::fs::write(&paths.pid_file, "4242").expect("write the pid file");

        age_pid_file();
        paths.mark_in_use_by(4343).expect("mark for another daemon");
        assert_eq!(
            modified(),
            long_ago,
            "another daemon's pid file is left alone"
        );

        paths.mark_in_use_by(4242).expect("mark for this daemon");
        assert!(
            modified() > long_ago + Duration::from_secs(60 * 24 * 60 * 60),
            "this daemon's pid file is freshly modified"
        );
        assert_eq!(read_pid(&paths.pid_file), Some(4242));

        std::fs::remove_file(&paths.pid_file).expect("remove the pid file");
        paths
            .mark_in_use_by(4242)
            .expect("a missing pid file is not an error");
        assert!(!paths.pid_file.exists(), "no pid file is written back");
    }

    #[test]
    fn a_lock_file_left_behind_keeps_no_one_out() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let lock_path = temp_dir.path().join("daemon.lock");
        // What a proxy that died leaves: the file, but no one holding it.
        std::fs::write(&lock_path, "").expect("write a leftover lock file");

        let started = Instant::now();
        let lock = smol::block_on(DaemonLock::acquire(&lock_path, Duration::from_secs(5)))
            .expect("a leftover lock file should not keep the next proxy out");
        assert!(started.elapsed() < Duration::from_secs(2));
        drop(lock);
    }

    #[test]
    fn a_daemon_only_removes_the_files_that_are_still_its_own() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let paths = DaemonPaths::new(
            temp_dir.path().to_path_buf(),
            temp_dir.path().join("daemon.log"),
        );
        std::fs::write(&paths.socket, "").expect("write the socket stand-in");
        std::fs::write(&paths.pid_file, "4242").expect("write the pid file");

        paths.remove_if_owned_by(4243);
        assert!(paths.socket.exists() && paths.pid_file.exists());

        paths.remove_if_owned_by(4242);
        assert!(!paths.socket.exists() && !paths.pid_file.exists());
    }

    #[test]
    fn only_the_daemon_of_this_directory_counts_as_the_daemon() {
        let state_dir = Path::new("/home/user/.local/share/zed/server_state/d-0123abcd");
        let command_line =
            |arguments: &[&str]| arguments.iter().map(OsString::from).collect::<Vec<_>>();
        let state_dir_argument = state_dir.to_string_lossy().into_owned();

        assert!(is_daemon_command_line(
            &command_line(&[
                "/home/user/.zed_server/zed-remote-server-dev-abc",
                "daemon",
                "--log-file",
                "/home/user/.local/share/zed/logs/server-d-0123abcd.log",
                "--state-dir",
                &state_dir_argument,
            ]),
            state_dir
        ));
        assert!(!is_daemon_command_line(
            &command_line(&[
                "/home/user/.zed_server/zed-remote-server-dev-abc",
                "daemon",
                "--state-dir",
                "/home/user/.local/share/zed/server_state/d-ffffffff",
            ]),
            state_dir
        ));
        assert!(!is_daemon_command_line(
            &command_line(&[
                "/home/user/.zed_server/zed-remote-server-dev-abc",
                "proxy",
                "--identifier",
                "0a1b2c3d.workspace-5",
            ]),
            state_dir
        ));
        assert!(!is_daemon_command_line(
            &command_line(&["/usr/bin/sleep", "--state-dir", &state_dir_argument]),
            state_dir
        ));
    }

    #[test]
    fn a_pid_that_is_not_this_daemon_is_left_alone() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let paths = DaemonPaths::new(
            temp_dir.path().to_path_buf(),
            temp_dir.path().join("daemon.log"),
        );
        std::fs::write(&paths.pid_file, std::process::id().to_string())
            .expect("write the pid file");

        kill_daemon(std::process::id(), &paths).expect("killing should succeed or skip");
        assert!(
            !daemon_is_running(&paths),
            "this test process is not a daemon"
        );
        assert!(
            !paths.pid_file.exists(),
            "the stale pid file should still be cleaned up"
        );
    }

    fn test_paths(temp_dir: &tempfile::TempDir) -> DaemonPaths {
        DaemonPaths::new(
            temp_dir.path().to_path_buf(),
            temp_dir.path().join("daemon.log"),
        )
    }

    fn hello(reconnect: bool) -> DaemonHello {
        DaemonHello {
            protocol_version: DAEMON_PROTOCOL_VERSION,
            identifier: "0a1b2c3d.workspace-5".to_string(),
            reconnect,
            ssh_auth_sock: None,
        }
    }

    /// A stand-in daemon that answers every introduction with `outcome` and
    /// keeps the connections it took.
    fn start_answering_daemon(socket: &Path, outcome: DaemonHelloOutcome) -> smol::Task<()> {
        let listener = UnixListener::bind(socket).expect("bind a stand-in daemon");
        smol::spawn(async move {
            let mut connections = Vec::new();
            while let Ok((mut connection, _)) = listener.accept().await {
                if read_json_message::<_, DaemonHello>(&mut connection)
                    .await
                    .is_ok()
                {
                    write_json_message(
                        &mut connection,
                        &DaemonHelloReply {
                            outcome,
                            message: None,
                        },
                    )
                    .await
                    .ok();
                }
                connections.push(connection);
            }
        })
    }

    /// A stand-in for a hung daemon: takes one connection and never answers.
    /// It lets go of its socket once it has the connection, so that a
    /// successor can be bound at the same path — Windows will not remove a
    /// socket file that is still bound.
    fn start_silent_daemon(socket: &Path) -> smol::Task<()> {
        let listener = UnixListener::bind(socket).expect("bind a stand-in daemon");
        smol::spawn(async move {
            let Ok((connection, _)) = listener.accept().await else {
                return;
            };
            drop(listener);
            let _connection = connection;
            futures::future::pending::<()>().await;
        })
    }

    /// A connector whose "spawning a daemon" starts an answering stand-in, and
    /// that counts how often it had to.
    fn test_connector(
        paths: DaemonPaths,
        spawned: Rc<RefCell<Vec<smol::Task<()>>>>,
    ) -> DaemonConnector {
        DaemonConnector {
            paths,
            spawn: Box::new(move |paths| {
                spawned.borrow_mut().push(start_answering_daemon(
                    &paths.socket,
                    DaemonHelloOutcome::Accepted,
                ));
                Ok(())
            }),
            reply_timeout: Duration::from_millis(300),
            start_timeout: Duration::from_secs(5),
            lock_timeout: Duration::from_secs(5),
        }
    }

    #[test]
    fn a_reconnect_with_no_daemon_reports_the_server_gone() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let spawned = Rc::new(RefCell::new(Vec::new()));
        let connector = test_connector(test_paths(&temp_dir), spawned.clone());

        let Err(error) = smol::block_on(connector.connect(&hello(true))) else {
            panic!("there is no daemon to reconnect to");
        };
        assert_eq!(error.to_exit_code(), 90);
        assert!(
            spawned.borrow().is_empty(),
            "a reconnect never starts a daemon"
        );
    }

    #[test]
    fn a_reconnect_to_a_daemon_without_the_session_reports_the_server_gone() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let paths = test_paths(&temp_dir);
        let _daemon = start_answering_daemon(&paths.socket, DaemonHelloOutcome::SessionNotFound);
        let connector = test_connector(paths, Rc::new(RefCell::new(Vec::new())));

        let Err(error) = smol::block_on(connector.connect(&hello(true))) else {
            panic!("the daemon has no such session");
        };
        assert_eq!(error.to_exit_code(), 90);
    }

    #[test]
    fn a_fresh_connection_starts_the_daemon_when_none_runs() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let spawned = Rc::new(RefCell::new(Vec::new()));
        let connector = test_connector(test_paths(&temp_dir), spawned.clone());

        smol::block_on(connector.connect(&hello(false)))
            .expect("the proxy should start a daemon and be accepted by it");
        assert_eq!(spawned.borrow().len(), 1);
    }

    #[test]
    fn a_daemon_that_does_not_answer_is_replaced() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let paths = test_paths(&temp_dir);
        let _hung = start_silent_daemon(&paths.socket);
        // The pid file names a live process that is not a daemon, which the
        // replacement must not kill.
        std::fs::write(&paths.pid_file, std::process::id().to_string())
            .expect("write the pid file");
        let spawned = Rc::new(RefCell::new(Vec::new()));
        let connector = test_connector(paths, spawned.clone());

        smol::block_on(connector.connect(&hello(false)))
            .expect("the proxy should replace the hung daemon and be accepted");
        assert_eq!(spawned.borrow().len(), 1);
    }

    #[test]
    fn a_daemon_another_proxy_has_already_replaced_is_left_running() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let paths = test_paths(&temp_dir);
        let _successor = start_answering_daemon(&paths.socket, DaemonHelloOutcome::Accepted);
        std::fs::write(&paths.pid_file, std::process::id().to_string())
            .expect("write the pid file");
        let spawned = Rc::new(RefCell::new(Vec::new()));
        let connector = test_connector(paths.clone(), spawned.clone());

        let found_hung = DaemonIdentity {
            pid: Some(u32::MAX - 1),
            socket: None,
        };
        smol::block_on(connector.restart(found_hung))
            .expect("finding a successor is not a failure");
        assert!(spawned.borrow().is_empty(), "the successor must be kept");
        assert!(paths.pid_file.exists() && paths.socket.exists());
        smol::block_on(connector.connect(&hello(false)))
            .expect("the successor should still answer");
    }

    #[test]
    fn the_proxy_turns_stdio_into_frames_and_frames_back_into_stdio() {
        smol::block_on(async {
            let (mut daemon_end, proxy_connection) = memory_stream_pair();
            let (mut client_stdin, proxy_stdin) = memory_stream_pair();
            let (proxy_stdout, mut client_stdout) = memory_stream_pair();
            let (proxy_stderr, mut client_stderr) = memory_stream_pair();
            let forwarding = smol::spawn(forward(
                proxy_connection,
                proxy_stdin,
                proxy_stdout,
                proxy_stderr,
            ));

            let mut buffer = Vec::new();
            let ping = proto::Ping {}.into_envelope(1, None, None);
            write_message(&mut client_stdin, &mut buffer, ping.clone())
                .await
                .expect("the client writes to the proxy's stdin");
            client_stdin.flush().await.expect("flush stdin");
            assert_eq!(
                read_daemon_frame(&mut daemon_end, &mut buffer)
                    .await
                    .expect("the daemon should receive a frame"),
                DaemonFrameKind::Envelope
            );
            assert_eq!(
                decode_envelope(&buffer).expect("the frame carries the envelope"),
                ping
            );

            let ack = proto::Ack {}.into_envelope(2, Some(1), None);
            write_envelope_frame(&mut daemon_end, &mut buffer, ack.clone())
                .await
                .expect("the daemon answers");
            write_daemon_frame(&mut daemon_end, DaemonFrameKind::Log, b"log line\n")
                .await
                .expect("the daemon logs");
            daemon_end.flush().await.expect("flush the daemon's end");
            assert_eq!(
                read_message(&mut client_stdout, &mut buffer)
                    .await
                    .expect("the client should read the answer from stdout"),
                ack
            );
            let mut log = vec![0u8; b"log line\n".len()];
            futures::io::AsyncReadExt::read_exact(&mut client_stderr, &mut log)
                .await
                .expect("the client should read the log from stderr");
            assert_eq!(log, b"log line\n");

            drop(client_stdin);
            assert!(matches!(
                forwarding.await,
                Err(ExecuteProxyError::StdinTask(_))
            ));
        });
    }
}
