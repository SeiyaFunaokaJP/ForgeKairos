//! ADR-F50: a worktree an agent wrote into runs none of its own code until
//! the user resumes it.

use std::{
    any::Any,
    cell::RefCell,
    collections::BTreeSet,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
};

use collections::HashSet;
use fs::{FakeFs, Fs as _};
use futures::{FutureExt as _, StreamExt as _, channel::mpsc::UnboundedReceiver};
use gpui::{Entity, TestAppContext, UpdateGlobal as _};
use language::{Buffer, FakeLspAdapter, WorktreeCodePausedMode, rust_lang};
use lsp::{DEFAULT_LSP_REQUEST_TIMEOUT, FakeLanguageServer, LanguageServerName};
use parking_lot::Mutex;
use project::{
    AgentWriteScope, AgentWriterKind, Project, WorktreeId,
    lsp_store::{
        FormatTrigger, LspFormatTarget, LspStoreEvent, language_server_settings_in_worktree,
    },
    worktree_code_execution::{
        GuardChecksWaitForTests, PausedLanguageServer, ResumeOutcome,
        StoredPausesAreNotKeptForTests, WorktreeCodeExecutionEvent, WorktreeCodeState,
        external_formatter_may_run_while_paused,
    },
};
use serde_json::json;
use settings::SettingsStore;
use util::{path, rel_path::rel_path};

fn only_worktree(project: &Entity<Project>, cx: &mut TestAppContext) -> WorktreeId {
    project.read_with(cx, |project, cx| {
        project
            .worktrees(cx)
            .next()
            .expect("the project has a worktree")
            .read(cx)
            .id()
    })
}

fn code_state(project: &Entity<Project>, cx: &mut TestAppContext) -> WorktreeCodeState {
    let worktree_id = only_worktree(project, cx);
    project.read_with(cx, |project, cx| {
        project
            .worktree_code_execution()
            .read(cx)
            .state(worktree_id)
    })
}

fn note_agent_write(project: &Entity<Project>, cx: &mut TestAppContext) {
    project
        .update(cx, |project, cx| {
            project.note_agent_write_to_path(Path::new(path!("/root/src/main.rs")), cx)
        })
        .detach();
}

/// What the resume dialog does with what it showed.
async fn resume(project: &Entity<Project>, cx: &mut TestAppContext) -> ResumeOutcome {
    let worktree_id = only_worktree(project, cx);
    let execution = project.read_with(cx, |project, _| project.worktree_code_execution().clone());
    execution
        .update(cx, |execution, cx| {
            let generation = execution
                .generation(worktree_id)
                .expect("the worktree is known");
            execution.resume(worktree_id, generation, cx)
        })
        .await
}

fn running_language_servers(
    project: &Entity<Project>,
    cx: &mut TestAppContext,
) -> BTreeSet<String> {
    project.read_with(cx, |project, cx| {
        project
            .lsp_store()
            .read(cx)
            .language_server_statuses()
            .map(|(_, status)| status.name.to_string())
            .collect()
    })
}

fn paused_language_servers(
    project: &Entity<Project>,
    cx: &mut TestAppContext,
) -> Vec<(String, PausedLanguageServer)> {
    let worktree_id = only_worktree(project, cx);
    project.read_with(cx, |project, cx| {
        project
            .worktree_code_execution()
            .read(cx)
            .details(worktree_id)
            .expect("the worktree is known")
            .language_servers
            .into_iter()
            .map(|(name, how)| (name.to_string(), how))
            .collect()
    })
}

/// The pause is off by default: the tests here are about what it does once
/// the user turned it on.
fn init_test(cx: &mut TestAppContext) {
    crate::init_test(cx);
    set_pause_in_user_settings(true, cx);
}

fn set_pause_in_user_settings(pause: bool, cx: &mut TestAppContext) {
    cx.update(|cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |settings| {
                settings
                    .agent
                    .get_or_insert_default()
                    .pause_worktree_code_after_agent_writes = Some(pause);
            })
        })
    });
}

fn skipped_formatters(project: &Entity<Project>, cx: &mut TestAppContext) -> BTreeSet<String> {
    let worktree_id = only_worktree(project, cx);
    project.read_with(cx, |project, cx| {
        project
            .worktree_code_execution()
            .read(cx)
            .details(worktree_id)
            .expect("the worktree is known")
            .skipped_formatters
    })
}

fn use_formatter_for_saving(formatter: serde_json::Value, cx: &mut TestAppContext) {
    cx.update(|cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |settings| {
                settings.project.all_languages.defaults.format_on_save =
                    Some(serde_json::from_value(json!("on")).expect("a format-on-save setting"));
                settings.project.all_languages.defaults.formatter =
                    Some(serde_json::from_value(formatter).expect("a formatter setting"));
            });
        });
    });
}

fn use_formatter(command: &str, cx: &mut TestAppContext) {
    use_formatter_for_saving(
        json!({ "external": { "command": command, "arguments": [] } }),
        cx,
    );
}

async fn format_for_saving(
    project: &Entity<Project>,
    buffer: &Entity<Buffer>,
    cx: &mut TestAppContext,
) -> anyhow::Result<()> {
    project
        .update(cx, |project, cx| {
            project.format(
                HashSet::from_iter([buffer.clone()]),
                LspFormatTarget::Buffers,
                true,
                FormatTrigger::Save,
                cx,
            )
        })
        .await
        .map(|_| ())
}

#[gpui::test]
async fn test_an_agent_write_pauses_the_worktree_until_the_user_resumes(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/root"),
        json!({ "src": { "main.rs": "fn main() {}\n" } }),
    )
    .await;
    let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
    cx.run_until_parked();
    let worktree_id = only_worktree(&project, cx);
    let execution = project.read_with(cx, |project, _| project.worktree_code_execution().clone());
    let events = Rc::new(RefCell::new(Vec::new()));
    let _subscription = cx.update(|cx| {
        let events = events.clone();
        cx.subscribe(
            &execution,
            move |_, event: &WorktreeCodeExecutionEvent, _| events.borrow_mut().push(event.clone()),
        )
    });
    assert_eq!(code_state(&project, cx), WorktreeCodeState::Allowed);

    // The user's own edit and save go nowhere near the pause.
    let buffer = project
        .update(cx, |project, cx| {
            project.open_local_buffer(path!("/root/src/main.rs"), cx)
        })
        .await
        .unwrap();
    buffer.update(cx, |buffer, cx| {
        buffer.edit([(0..0, "// mine\n")], None, cx)
    });
    project
        .update(cx, |project, cx| project.save_buffer(buffer.clone(), cx))
        .await
        .unwrap();
    cx.run_until_parked();
    assert_eq!(code_state(&project, cx), WorktreeCodeState::Allowed);

    // A path in no worktree has no worktree code to pause.
    project
        .update(cx, |project, cx| {
            project.note_agent_write_to_path(Path::new(path!("/elsewhere/file.rs")), cx)
        })
        .await;
    assert_eq!(code_state(&project, cx), WorktreeCodeState::Allowed);

    note_agent_write(&project, cx);
    assert_eq!(code_state(&project, cx), WorktreeCodeState::Paused);
    let shown_generation = execution
        .read_with(cx, |execution, _| execution.generation(worktree_id))
        .expect("the worktree is known");

    // The agent writes again while the dialog shows the first write.
    note_agent_write(&project, cx);
    let outcome = execution
        .update(cx, |execution, cx| {
            execution.resume(worktree_id, shown_generation, cx)
        })
        .await;
    assert_eq!(
        outcome,
        ResumeOutcome::WrittenSinceShown,
        "a resume confirms only what the user was shown"
    );
    assert_eq!(code_state(&project, cx), WorktreeCodeState::Paused);

    assert_eq!(resume(&project, cx).await, ResumeOutcome::Resumed);
    assert_eq!(code_state(&project, cx), WorktreeCodeState::Allowed);
    cx.run_until_parked();
    let transitions = events
        .borrow()
        .iter()
        .filter(|event| {
            matches!(
                event,
                WorktreeCodeExecutionEvent::Paused(_) | WorktreeCodeExecutionEvent::Resumed(_)
            )
        })
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(
        transitions,
        vec![
            WorktreeCodeExecutionEvent::Paused(worktree_id),
            WorktreeCodeExecutionEvent::Resumed(worktree_id),
        ],
        "one pause per confirmation, however many writes follow it"
    );
}

#[gpui::test]
async fn test_format_on_save_skips_formatters_that_run_worktree_code_while_paused(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/root"),
        json!({ "src": { "main.rs": "fn main() {}\n" } }),
    )
    .await;
    let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
    project.read_with(cx, |project, _| project.languages().add(rust_lang()));
    // In the user's own settings, so that no project approval is involved.
    use_formatter("scripts/format", cx);
    let buffer = project
        .update(cx, |project, cx| {
            project.open_local_buffer(path!("/root/src/main.rs"), cx)
        })
        .await
        .unwrap();
    buffer.update(cx, |buffer, cx| {
        buffer.edit([(0..0, "// edited\n")], None, cx)
    });

    // What the agent's edit tool does: pause, format for saving, save.
    note_agent_write(&project, cx);
    format_for_saving(&project, &buffer, cx)
        .await
        .expect("a formatter from the worktree is skipped rather than failing the save");
    project
        .update(cx, |project, cx| project.save_buffer(buffer.clone(), cx))
        .await
        .expect("the save goes through");
    assert_eq!(
        fs.load(path!("/root/src/main.rs").as_ref()).await.unwrap(),
        "// edited\nfn main() {}\n"
    );

    // A formatter from outside the worktree that is not known to run no
    // project code is skipped too.
    use_formatter(path!("/elsewhere/format"), cx);
    format_for_saving(&project, &buffer, cx)
        .await
        .expect("skipped rather than run");
    assert_eq!(
        skipped_formatters(&project, cx),
        BTreeSet::from_iter([
            "scripts/format".to_string(),
            path!("/elsewhere/format").to_string()
        ])
    );

    // One that is keeps running while the worktree is paused: it is not
    // installed, so the format fails.
    use_formatter(path!("/elsewhere/gofmt"), cx);
    assert!(
        format_for_saving(&project, &buffer, cx).await.is_err(),
        "gofmt from outside the worktree should have run"
    );

    // Resumed, the worktree's own formatter runs again.
    use_formatter("scripts/format", cx);
    assert_eq!(resume(&project, cx).await, ResumeOutcome::Resumed);
    assert!(
        format_for_saving(&project, &buffer, cx).await.is_err(),
        "the worktree's formatter should run once resumed"
    );
}

#[gpui::test]
fn test_only_formatters_known_to_run_no_project_code_run_while_paused() {
    let root = Path::new(path!("/root"));
    let outside = Path::new(path!("/opt/tools"));
    let at =
        |directory: &Path, program: &str| directory.join(program).to_string_lossy().into_owned();
    for program in [
        "clang-format",
        "clang-format-18",
        "gofmt",
        "shfmt",
        "stylua",
        "taplo",
        "ruff",
        "biome",
        "nixfmt",
        "alejandra",
    ] {
        assert!(
            external_formatter_may_run_while_paused(&at(outside, program), None, root),
            "{program}"
        );
        assert!(
            !external_formatter_may_run_while_paused(&at(&root.join("bin"), program), None, root),
            "{program} from the worktree"
        );
    }
    // Commands whose program runs whatever the project names: `poetry run
    // black -`, `bunx prettier`, and the like.
    for program in [
        "poetry",
        "bunx",
        "npx",
        "node",
        "python3",
        "black",
        "prettier",
        "rustfmt",
        "cargo",
        "mix",
        "rubocop",
        "standardrb",
        "bundle",
        "dotnet",
        "dprint",
        "php-cs-fixer",
        "clang-format-custom",
    ] {
        assert!(
            !external_formatter_may_run_while_paused(&at(outside, program), None, root),
            "{program}"
        );
    }
    // Relative to the worktree, where formatters start.
    assert!(!external_formatter_may_run_while_paused(
        "bin/gofmt",
        None,
        root
    ));

    // A bare name counts where `PATH` finds it.
    let tools = tempfile::tempdir().unwrap();
    let worktree = tempfile::tempdir().unwrap();
    let executable = if cfg!(windows) { "gofmt.exe" } else { "gofmt" };
    for directory in [tools.path().to_path_buf(), worktree.path().join("bin")] {
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join(executable), "").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(
                directory.join(executable),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }
    }
    let search_path = |entries: &[PathBuf]| {
        std::env::join_paths(entries)
            .unwrap()
            .to_string_lossy()
            .into_owned()
    };
    assert!(external_formatter_may_run_while_paused(
        "gofmt",
        Some(&search_path(&[tools.path().to_path_buf()])),
        worktree.path()
    ));
    assert!(
        !external_formatter_may_run_while_paused("gofmt", None, worktree.path()),
        "nowhere to find it"
    );
    assert!(
        !external_formatter_may_run_while_paused(
            "gofmt",
            Some(&search_path(&[
                worktree.path().join("bin"),
                tools.path().to_path_buf()
            ])),
            worktree.path()
        ),
        "found in the worktree first"
    );
    assert!(
        !external_formatter_may_run_while_paused(
            "gofmt",
            Some(&search_path(&[
                PathBuf::from("bin"),
                tools.path().to_path_buf()
            ])),
            worktree.path()
        ),
        "a relative entry is searched from the worktree"
    );
}

#[gpui::test]
async fn test_language_servers_follow_the_worktree_code_pause(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/root"),
        json!({
            ".cargo": { "config.toml": "[build]\n" },
            "src": { "main.rs": "fn main() {}\n" },
        }),
    )
    .await;
    let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
    let language_registry = project.read_with(cx, |project, _| project.languages().clone());
    language_registry.add(rust_lang());
    let sent_initialization_options = Arc::new(Mutex::new(Vec::new()));
    let mut restricted_servers = language_registry.register_fake_lsp(
        "Rust",
        FakeLspAdapter {
            name: "restricted-server",
            worktree_code_paused_mode: WorktreeCodePausedMode::Restricted {
                initialization_options: json!({ "buildScripts": { "enable": false } }),
                workspace_configuration: json!({}),
                unchanged_files: &[".cargo/config.toml"],
                formatting_runs_programs: false,
            },
            initializer: Some(Box::new({
                let sent_initialization_options = sent_initialization_options.clone();
                move |fake_server| {
                    let sent_initialization_options = sent_initialization_options.clone();
                    fake_server.set_request_handler::<lsp::request::Initialize, _, _>(
                        move |params, _| {
                            sent_initialization_options
                                .lock()
                                .push(params.initialization_options);
                            async move { Ok(lsp::InitializeResult::default()) }
                        },
                    );
                }
            })),
            ..FakeLspAdapter::default()
        },
    );
    let mut stopped_servers = language_registry.register_fake_lsp(
        "Rust",
        FakeLspAdapter {
            name: "stopped-server",
            worktree_code_paused_mode: WorktreeCodePausedMode::Stopped,
            ..FakeLspAdapter::default()
        },
    );
    let mut unaffected_servers = language_registry.register_fake_lsp(
        "Rust",
        FakeLspAdapter {
            name: "unaffected-server",
            ..FakeLspAdapter::default()
        },
    );
    cx.run_until_parked();

    let _buffer = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/root/src/main.rs"), cx)
        })
        .await
        .unwrap();
    assert!(restricted_servers.next().await.is_some());
    assert!(stopped_servers.next().await.is_some());
    assert!(unaffected_servers.next().await.is_some());
    cx.run_until_parked();
    assert_eq!(
        sent_initialization_options
            .lock()
            .drain(..)
            .collect::<Vec<_>>(),
        vec![None]
    );

    note_agent_write(&project, cx);
    cx.run_until_parked();
    assert!(
        restricted_servers.next().await.is_some(),
        "the restricted server restarts in its paused mode"
    );
    cx.run_until_parked();
    assert_eq!(
        sent_initialization_options
            .lock()
            .drain(..)
            .collect::<Vec<_>>(),
        vec![Some(json!({ "buildScripts": { "enable": false } }))]
    );
    assert!(
        stopped_servers.try_recv().is_err(),
        "a server that runs the worktree's code waits until it is resumed"
    );
    assert!(
        unaffected_servers.try_recv().is_err(),
        "a server that runs none of the worktree's code is not restarted"
    );
    assert_eq!(
        running_language_servers(&project, cx),
        BTreeSet::from_iter([
            "restricted-server".to_string(),
            "unaffected-server".to_string()
        ])
    );
    assert_eq!(
        paused_language_servers(&project, cx),
        vec![
            (
                "restricted-server".to_string(),
                PausedLanguageServer::Restricted
            ),
            ("stopped-server".to_string(), PausedLanguageServer::Stopped),
        ]
    );

    // A file the restricted mode depends on changes while paused.
    fs.insert_file(
        path!("/root/.cargo/config.toml"),
        b"[build]\nrustc-wrapper = \"./wrapper\"\n".to_vec(),
    )
    .await;
    cx.run_until_parked();
    assert!(
        restricted_servers.try_recv().is_err(),
        "the restricted server stops once Cargo's configuration differs from what was confirmed"
    );
    assert_eq!(
        running_language_servers(&project, cx),
        BTreeSet::from_iter(["unaffected-server".to_string()])
    );

    assert_eq!(resume(&project, cx).await, ResumeOutcome::Resumed);
    cx.run_until_parked();
    assert!(restricted_servers.next().await.is_some());
    assert!(stopped_servers.next().await.is_some());
    cx.run_until_parked();
    assert_eq!(
        sent_initialization_options
            .lock()
            .drain(..)
            .collect::<Vec<_>>(),
        vec![None],
        "resumed, the server runs as configured"
    );
    assert!(unaffected_servers.try_recv().is_err());
}

const RUST_GUARD_FILES: &[&str] = &[
    ".cargo/config",
    ".cargo/config.toml",
    "rust-toolchain",
    "rust-toolchain.toml",
    "rust-analyzer.toml",
    "rust-project.json",
    ".rust-project.json",
];

struct RestrictedServer {
    project: Entity<Project>,
    buffer: Entity<Buffer>,
    /// The instance running restricted.
    server: FakeLanguageServer,
    servers: UnboundedReceiver<FakeLanguageServer>,
    sent_initialization_options: Arc<Mutex<Vec<Option<serde_json::Value>>>>,
    _registration: Box<dyn Any>,
}

/// A project at `/root`, which `fs` holds, whose rust-analyzer-like server
/// runs in its restricted mode after an agent wrote into the worktree.
async fn paused_with_a_restricted_server(
    fs: &Arc<FakeFs>,
    cx: &mut TestAppContext,
) -> RestrictedServer {
    let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
    let language_registry = project.read_with(cx, |project, _| project.languages().clone());
    language_registry.add(rust_lang());
    let sent_initialization_options = Arc::new(Mutex::new(Vec::new()));
    let mut servers = language_registry.register_fake_lsp(
        "Rust",
        FakeLspAdapter {
            name: "restricted-server",
            worktree_code_paused_mode: WorktreeCodePausedMode::Restricted {
                initialization_options: json!({ "buildScripts": { "enable": false } }),
                workspace_configuration: json!({}),
                unchanged_files: RUST_GUARD_FILES,
                formatting_runs_programs: false,
            },
            initializer: Some(Box::new({
                let sent_initialization_options = sent_initialization_options.clone();
                move |fake_server| {
                    let sent_initialization_options = sent_initialization_options.clone();
                    fake_server.set_request_handler::<lsp::request::Initialize, _, _>(
                        move |params, _| {
                            sent_initialization_options
                                .lock()
                                .push(params.initialization_options);
                            async move { Ok(lsp::InitializeResult::default()) }
                        },
                    );
                }
            })),
            ..FakeLspAdapter::default()
        },
    );
    cx.run_until_parked();
    let (buffer, registration) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/root/src/main.rs"), cx)
        })
        .await
        .unwrap();
    servers.next().await.expect("the server starts");
    cx.run_until_parked();
    note_agent_write(&project, cx);
    cx.run_until_parked();
    let server = servers
        .next()
        .await
        .expect("the server restarts in its paused mode");
    cx.run_until_parked();
    assert!(restricted_server_running(&project, cx));
    RestrictedServer {
        project,
        buffer,
        server,
        servers,
        sent_initialization_options,
        _registration: Box::new(registration),
    }
}

fn restricted_server_running(project: &Entity<Project>, cx: &mut TestAppContext) -> bool {
    running_language_servers(project, cx).contains("restricted-server")
}

/// An agent writes into the paused worktree again, where the scan may not
/// see it, so its guard files are checked on disk.
fn check_guard_files_after_an_agent_write(project: &Entity<Project>, cx: &mut TestAppContext) {
    note_agent_write(project, cx);
    cx.run_until_parked();
}

fn rust_worktree() -> serde_json::Value {
    json!({
        "Cargo.toml": "[package]\nname = \"app\"\n",
        "Cargo.lock": "",
        "src": { "main.rs": "fn main() {}\n" },
    })
}

#[gpui::test]
async fn test_a_guard_file_counts_whatever_its_case(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    let mut tree = rust_worktree();
    tree[".CARGO"] = json!({ "CONFIG.TOML": "[build]\n" });
    fs.insert_tree(path!("/root"), tree).await;
    let mut restricted = paused_with_a_restricted_server(&fs, cx).await;

    fs.insert_file(
        path!("/root/.CARGO/CONFIG.TOML"),
        b"[build]\nrustc-wrapper = \"./wrapper\"\n".to_vec(),
    )
    .await;
    check_guard_files_after_an_agent_write(&restricted.project, cx);
    let case_insensitive = cfg!(any(target_os = "windows", target_os = "macos"));
    assert_eq!(
        restricted_server_running(&restricted.project, cx),
        !case_insensitive,
        "Cargo reads `.CARGO/CONFIG.TOML` where the filesystem ignores case"
    );
    if case_insensitive {
        assert_eq!(
            paused_language_servers(&restricted.project, cx),
            vec![(
                "restricted-server".to_string(),
                PausedLanguageServer::Stopped
            )]
        );
    } else {
        assert!(restricted.servers.try_recv().is_err());
    }
}

#[gpui::test]
async fn test_a_guard_file_the_scan_ignores_is_checked_on_disk(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    let mut tree = rust_worktree();
    tree[".git"] = json!({});
    tree[".gitignore"] = json!(".cargo/\n");
    tree[".cargo"] = json!({ "config.toml": "[build]\n" });
    fs.insert_tree(path!("/root"), tree).await;
    let mut restricted = paused_with_a_restricted_server(&fs, cx).await;
    let scanned = restricted.project.read_with(cx, |project, cx| {
        project
            .worktrees(cx)
            .next()
            .expect("the project has a worktree")
            .read(cx)
            .entry_for_path(rel_path(".cargo/config.toml"))
            .is_some()
    });
    assert!(!scanned, "an ignored directory is not scanned");

    // Nothing changed: a check on disk restarts nothing.
    check_guard_files_after_an_agent_write(&restricted.project, cx);
    assert!(restricted.servers.try_recv().is_err());
    assert!(restricted_server_running(&restricted.project, cx));

    fs.insert_file(
        path!("/root/.cargo/config.toml"),
        b"[build]\nrustc-wrapper = \"./wrapper\"\n".to_vec(),
    )
    .await;
    check_guard_files_after_an_agent_write(&restricted.project, cx);
    assert!(!restricted_server_running(&restricted.project, cx));
}

#[gpui::test]
async fn test_a_linked_guard_file_is_checked_through_the_link(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/root"), rust_worktree()).await;
    fs.insert_tree(
        path!("/outside"),
        json!({ "toolchain.toml": "[toolchain]\nchannel = \"stable\"\n" }),
    )
    .await;
    fs.create_symlink(
        Path::new(path!("/root/rust-toolchain.toml")),
        PathBuf::from(path!("/outside/toolchain.toml")),
    )
    .await
    .unwrap();
    let restricted = paused_with_a_restricted_server(&fs, cx).await;

    // The link's target, outside the worktree, changes.
    fs.insert_file(
        path!("/outside/toolchain.toml"),
        b"[toolchain]\npath = \"/tmp/toolchain\"\n".to_vec(),
    )
    .await;
    check_guard_files_after_an_agent_write(&restricted.project, cx);
    assert!(!restricted_server_running(&restricted.project, cx));
}

#[gpui::test]
async fn test_rust_project_descriptions_are_guard_files(cx: &mut TestAppContext) {
    init_test(cx);
    for name in ["rust-project.json", ".rust-project.json"] {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), rust_worktree()).await;
        let restricted = paused_with_a_restricted_server(&fs, cx).await;

        // An agent adds a project description, which can name build commands.
        fs.insert_file(
            Path::new(path!("/root")).join(name),
            br#"{ "crates": [] }"#.to_vec(),
        )
        .await;
        check_guard_files_after_an_agent_write(&restricted.project, cx);
        assert!(
            !restricted_server_running(&restricted.project, cx),
            "{name}"
        );
    }
}

#[gpui::test]
async fn test_a_restricted_server_goes_without_project_settings(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    let project_settings = json!({
        "lsp": {
            "restricted-server": {
                "initialization_options": { "fromProject": true },
                "settings": { "fromProject": true },
            }
        }
    });
    let mut tree = rust_worktree();
    tree[".zed"] = json!({ "settings.json": project_settings.to_string() });
    fs.insert_tree(path!("/root"), tree).await;
    let restricted = paused_with_a_restricted_server(&fs, cx).await;
    let worktree_id = only_worktree(&restricted.project, cx);
    let server_name = LanguageServerName("restricted-server".into());
    let settings_from_project = |cx: &mut TestAppContext| {
        cx.update(|cx| {
            language_server_settings_in_worktree(worktree_id, &server_name, cx)
                .and_then(|settings| settings.settings.clone())
        })
    };

    assert_eq!(
        restricted
            .sent_initialization_options
            .lock()
            .drain(..)
            .collect::<Vec<_>>(),
        vec![
            Some(json!({ "fromProject": true })),
            Some(json!({ "buildScripts": { "enable": false } })),
        ],
        "allowed, the server had the project's options; restricted, only its paused mode's"
    );
    assert_eq!(settings_from_project(cx), None);

    assert_eq!(
        resume(&restricted.project, cx).await,
        ResumeOutcome::Resumed
    );
    cx.run_until_parked();
    assert_eq!(
        settings_from_project(cx),
        Some(json!({ "fromProject": true }))
    );
}

#[gpui::test]
async fn test_a_restricted_server_hears_of_changes_only_once_its_guard_files_are_checked(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    let mut tree = rust_worktree();
    tree[".git"] = json!({});
    tree[".gitignore"] = json!(".cargo/\n");
    tree[".cargo"] = json!({ "config.toml": "[build]\n" });
    fs.insert_tree(path!("/root"), tree).await;
    let mut restricted = paused_with_a_restricted_server(&fs, cx).await;
    let heard = listen_to_the_restricted_server(&restricted, cx).await;

    // The guard files are as confirmed: a change reaches the server once a
    // check has passed.
    fs.insert_tree(
        path!("/root/src/bin"),
        json!({ "tool.rs": "fn main() {}\n" }),
    )
    .await;
    cx.run_until_parked();
    assert!(
        heard.lock().iter().any(|uri| uri.ends_with("tool.rs")),
        "{:?}",
        heard.lock()
    );
    heard.lock().clear();

    // Cargo's configuration changes where the scan does not look, and a
    // change that would have rust-analyzer load the workspace follows: none
    // of it reaches the server, which stops.
    fs.insert_file(
        path!("/root/.cargo/config.toml"),
        b"[build]\nrustc-wrapper = \"./wrapper\"\n".to_vec(),
    )
    .await;
    cx.run_until_parked();
    fs.insert_file(path!("/root/Cargo.lock"), b"# changed\n".to_vec())
        .await;
    restricted.buffer.update(cx, |buffer, cx| {
        buffer.edit([(0..0, "// edited\n")], None, cx)
    });
    restricted
        .project
        .update(cx, |project, cx| {
            project.save_buffer(restricted.buffer.clone(), cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();
    assert_eq!(heard.lock().clone(), Vec::<String>::new());
    assert!(!restricted_server_running(&restricted.project, cx));

    // Put back as confirmed, the next check lets it start again.
    fs.insert_file(path!("/root/.cargo/config.toml"), b"[build]\n".to_vec())
        .await;
    fs.insert_file(path!("/root/src/lib.rs"), b"// a library\n".to_vec())
        .await;
    cx.run_until_parked();
    assert!(
        restricted.servers.next().await.is_some(),
        "started again once a check passed"
    );
    cx.run_until_parked();
    assert!(restricted_server_running(&restricted.project, cx));
}

/// Registers the restricted server for changes to `Cargo.lock` and Rust
/// files, and gathers what it hears of changed and saved files.
async fn listen_to_the_restricted_server(
    restricted: &RestrictedServer,
    cx: &mut TestAppContext,
) -> Arc<Mutex<Vec<String>>> {
    let heard = Arc::new(Mutex::new(Vec::<String>::new()));
    restricted
        .server
        .request::<lsp::request::RegisterCapability>(
            lsp::RegistrationParams {
                registrations: vec![lsp::Registration {
                    id: "watch".to_string(),
                    method: "workspace/didChangeWatchedFiles".to_string(),
                    register_options: serde_json::to_value(
                        lsp::DidChangeWatchedFilesRegistrationOptions {
                            watchers: vec![
                                lsp::FileSystemWatcher {
                                    glob_pattern: lsp::GlobPattern::String(
                                        path!("/root/Cargo.lock").to_string(),
                                    ),
                                    kind: None,
                                },
                                lsp::FileSystemWatcher {
                                    glob_pattern: lsp::GlobPattern::String(
                                        path!("/root/src/**/*.rs").to_string(),
                                    ),
                                    kind: None,
                                },
                            ],
                        },
                    )
                    .ok(),
                }],
            },
            DEFAULT_LSP_REQUEST_TIMEOUT,
        )
        .await
        .into_response()
        .unwrap();
    restricted
        .server
        .handle_notification::<lsp::notification::DidChangeWatchedFiles, _>({
            let heard = heard.clone();
            move |params, _| {
                heard.lock().extend(
                    params
                        .changes
                        .into_iter()
                        .map(|change| change.uri.to_string()),
                );
            }
        });
    restricted
        .server
        .handle_notification::<lsp::notification::DidSaveTextDocument, _>({
            let heard = heard.clone();
            move |params, _| {
                heard
                    .lock()
                    .push(format!("saved {}", params.text_document.uri.as_str()));
            }
        });
    cx.run_until_parked();
    heard
}

#[gpui::test]
async fn test_a_held_change_is_not_forwarded_when_a_write_lands_during_its_check(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    let mut tree = rust_worktree();
    tree[".cargo"] = json!({ "config.toml": "[build]\n" });
    fs.insert_tree(path!("/root"), tree).await;
    let restricted = paused_with_a_restricted_server(&fs, cx).await;
    let heard = listen_to_the_restricted_server(&restricted, cx).await;
    let (mut finish_checks, checks_wait) = watch::channel(false);
    cx.update(|cx| cx.set_global(GuardChecksWaitForTests(checks_wait)));

    // A change that would have rust-analyzer load the workspace: its check
    // reads the guard files as confirmed, and waits.
    fs.insert_file(path!("/root/Cargo.lock"), b"# changed\n".to_vec())
        .await;
    cx.run_until_parked();
    assert_eq!(
        heard.lock().clone(),
        Vec::<String>::new(),
        "held while its check runs"
    );

    // Cargo's configuration is written before the check is over.
    fs.insert_file(
        path!("/root/.cargo/config.toml"),
        b"[build]\nrustc-wrapper = \"./wrapper\"\n".to_vec(),
    )
    .await;
    cx.run_until_parked();
    finish_checks.send(true).unwrap();
    cx.run_until_parked();
    assert_eq!(
        heard.lock().clone(),
        Vec::<String>::new(),
        "a check that cannot have seen the write lets nothing through"
    );
    assert!(
        !restricted_server_running(&restricted.project, cx),
        "and the check after it stops the server"
    );
}

#[gpui::test]
async fn test_nothing_held_is_forwarded_while_a_command_can_write_the_worktree(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/root"), rust_worktree()).await;
    let restricted = paused_with_a_restricted_server(&fs, cx).await;
    let heard = listen_to_the_restricted_server(&restricted, cx).await;

    // A command can write where no change shows, so a check that passes while
    // it runs lets nothing through.
    let (command, _) = restricted.project.update(cx, |project, cx| {
        project.register_agent_writer(
            AgentWriteScope::Paths(vec![PathBuf::from(path!("/root"))]),
            AgentWriterKind::Command,
            cx,
        )
    });
    fs.insert_file(path!("/root/Cargo.lock"), b"# changed\n".to_vec())
        .await;
    cx.run_until_parked();
    assert_eq!(heard.lock().clone(), Vec::<String>::new());
    assert!(
        restricted_server_running(&restricted.project, cx),
        "its guard files are as confirmed"
    );

    // Once it has ended and a check after it passed, changes get through.
    drop(command);
    cx.run_until_parked();
    fs.insert_file(path!("/root/src/lib.rs"), b"// a library\n".to_vec())
        .await;
    cx.run_until_parked();
    assert!(
        heard.lock().iter().any(|uri| uri.ends_with("lib.rs")),
        "{:?}",
        heard.lock()
    );
}

#[gpui::test]
async fn test_formatting_through_programs_the_worktree_picks_is_skipped_while_paused(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/root"), rust_worktree()).await;
    let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
    let language_registry = project.read_with(cx, |project, _| project.languages().clone());
    language_registry.add(rust_lang());
    let formatted_by = Arc::new(Mutex::new(Vec::<&'static str>::new()));
    let formatting_server = |name: &'static str, formatting_runs_programs: bool| FakeLspAdapter {
        name,
        worktree_code_paused_mode: WorktreeCodePausedMode::Restricted {
            initialization_options: json!({}),
            workspace_configuration: json!({}),
            unchanged_files: &[],
            formatting_runs_programs,
        },
        initializer: Some(Box::new({
            let formatted_by = formatted_by.clone();
            move |fake_server| {
                let formatted_by = formatted_by.clone();
                fake_server.set_request_handler::<lsp::request::Formatting, _, _>(move |_, _| {
                    formatted_by.lock().push(name);
                    async move { Ok(None) }
                });
            }
        })),
        ..FakeLspAdapter::default()
    };
    let _through_programs =
        language_registry.register_fake_lsp("Rust", formatting_server("rustfmt-runner", true));
    let _in_process =
        language_registry.register_fake_lsp("Rust", formatting_server("in-process", false));
    cx.run_until_parked();
    let (buffer, _registration) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/root/src/main.rs"), cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();
    note_agent_write(&project, cx);
    cx.run_until_parked();

    use_formatter_for_saving(
        json!({ "language_server": { "name": "rustfmt-runner" } }),
        cx,
    );
    format_for_saving(&project, &buffer, cx)
        .await
        .expect("skipped rather than failing the save");
    use_formatter_for_saving(json!({ "language_server": { "name": "in-process" } }), cx);
    format_for_saving(&project, &buffer, cx)
        .await
        .expect("formatted in the server's own process");
    assert_eq!(formatted_by.lock().clone(), vec!["in-process"]);
    assert_eq!(
        skipped_formatters(&project, cx),
        BTreeSet::from_iter(["rustfmt-runner".to_string()])
    );
}

#[gpui::test]
async fn test_guard_files_changed_during_an_external_agent_session_are_not_taken_as_confirmed(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    let mut tree = rust_worktree();
    tree[".cargo"] = json!({ "config.toml": "[build]\n" });
    fs.insert_tree(path!("/root"), tree).await;
    let restricted = paused_with_a_restricted_server(&fs, cx).await;

    // The user's own change, while the worktree runs its code, is theirs.
    assert_eq!(
        resume(&restricted.project, cx).await,
        ResumeOutcome::Resumed
    );
    cx.run_until_parked();
    fs.insert_file(
        path!("/root/.cargo/config.toml"),
        b"[build]\njobs = 2\n".to_vec(),
    )
    .await;
    cx.run_until_parked();
    check_guard_files_after_an_agent_write(&restricted.project, cx);
    assert!(
        restricted_server_running(&restricted.project, cx),
        "restricted again, with the user's change confirmed"
    );

    // While an external agent's session lasts, a change between its turns
    // can come from what it left running.
    assert_eq!(
        resume(&restricted.project, cx).await,
        ResumeOutcome::Resumed
    );
    cx.run_until_parked();
    let session = restricted.project.update(cx, |project, cx| {
        project.register_external_agent_session(cx)
    });
    fs.insert_file(
        path!("/root/.cargo/config.toml"),
        b"[build]\nrustc-wrapper = \"./wrapper\"\n".to_vec(),
    )
    .await;
    cx.run_until_parked();
    check_guard_files_after_an_agent_write(&restricted.project, cx);
    assert!(!restricted_server_running(&restricted.project, cx));
    drop(session);
}

#[gpui::test]
async fn test_extension_language_servers_stop_while_paused_unless_the_user_allows_them(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    let allow = |names: &[&str], cx: &mut TestAppContext| {
        let names = names
            .iter()
            .map(|name| name.to_string())
            .collect::<Vec<_>>();
        cx.update(|cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings
                        .agent
                        .get_or_insert_default()
                        .language_servers_allowed_while_paused = Some(names);
                })
            })
        });
    };
    allow(&["gopls"], cx);
    let fs = FakeFs::new(cx.executor());
    let mut tree = rust_worktree();
    // Project settings cannot allow one.
    tree[".zed"] = json!({
        "settings.json": json!({
            "agent": { "language_servers_allowed_while_paused": ["some-extension-server"] }
        })
        .to_string()
    });
    fs.insert_tree(path!("/root"), tree).await;
    let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
    let language_registry = project.read_with(cx, |project, _| project.languages().clone());
    language_registry.add(rust_lang());
    let mut allowed_servers = language_registry.register_fake_lsp(
        "Rust",
        FakeLspAdapter {
            name: "gopls",
            is_extension: true,
            ..FakeLspAdapter::default()
        },
    );
    let mut other_servers = language_registry.register_fake_lsp(
        "Rust",
        FakeLspAdapter {
            name: "some-extension-server",
            is_extension: true,
            ..FakeLspAdapter::default()
        },
    );
    cx.run_until_parked();
    let _buffer = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/root/src/main.rs"), cx)
        })
        .await
        .unwrap();
    assert!(allowed_servers.next().await.is_some());
    assert!(other_servers.next().await.is_some());
    cx.run_until_parked();

    note_agent_write(&project, cx);
    cx.run_until_parked();
    assert!(allowed_servers.try_recv().is_err(), "left running");
    assert!(other_servers.try_recv().is_err(), "not started again");
    assert_eq!(
        running_language_servers(&project, cx),
        BTreeSet::from_iter(["gopls".to_string()])
    );
    assert_eq!(
        paused_language_servers(&project, cx),
        vec![(
            "some-extension-server".to_string(),
            PausedLanguageServer::Stopped
        )]
    );

    // The user's list takes effect at once.
    allow(&["some-extension-server"], cx);
    cx.run_until_parked();
    assert!(other_servers.next().await.is_some(), "allowed now");
    cx.run_until_parked();
    assert_eq!(
        running_language_servers(&project, cx),
        BTreeSet::from_iter(["some-extension-server".to_string()])
    );
}

#[gpui::test]
async fn test_a_write_pauses_every_open_project_that_holds_the_path(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/root"),
        json!({
            "app": { "crate": { "src": { "lib.rs": "" } } },
            "notes.md": "",
        }),
    )
    .await;
    fs.insert_tree(path!("/other"), json!({ "data": { "file.txt": "" } }))
        .await;
    let app = Project::test(fs.clone(), [path!("/root/app").as_ref()], cx).await;
    let inner = Project::test(fs.clone(), [path!("/root/app/crate").as_ref()], cx).await;
    let outer = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
    let other = Project::test(fs.clone(), [path!("/other").as_ref()], cx).await;
    cx.run_until_parked();
    let projects = [&app, &inner, &outer, &other];
    let states = |cx: &mut TestAppContext| projects.map(|project| code_state(project, cx));
    use WorktreeCodeState::{Allowed, Paused};

    // Written through one project, the file is in three of them.
    app.update(cx, |project, cx| {
        project.note_agent_write_to_path(Path::new(path!("/root/app/crate/src/lib.rs")), cx)
    })
    .await;
    assert_eq!(states(cx), [Paused, Paused, Paused, Allowed]);
    for project in [&app, &inner, &outer] {
        assert_eq!(resume(project, cx).await, ResumeOutcome::Resumed);
    }

    // A directory changed as a whole reaches the worktrees inside it.
    outer
        .update(cx, |project, cx| {
            project.note_agent_write_to_path(Path::new(path!("/root/app")), cx)
        })
        .await;
    assert_eq!(states(cx), [Paused, Paused, Paused, Allowed]);
    for project in [&app, &inner, &outer] {
        assert_eq!(resume(project, cx).await, ResumeOutcome::Resumed);
    }

    // A command of one project granted a path that another project holds.
    let (writer, _) = app.update(cx, |project, cx| {
        project.register_agent_writer(
            AgentWriteScope::Paths(vec![
                PathBuf::from(path!("/root/app")),
                PathBuf::from(path!("/other/data")),
            ]),
            AgentWriterKind::Command,
            cx,
        )
    });
    assert_eq!(states(cx), [Paused, Paused, Paused, Paused]);
    assert_eq!(
        resume(&other, cx).await,
        ResumeOutcome::WriterRunning,
        "not while the command runs"
    );
    drop(writer);
    cx.run_until_parked();
    for project in projects {
        assert_eq!(resume(project, cx).await, ResumeOutcome::Resumed);
    }

    // Without a sandbox, a command can write anywhere on its host.
    let (writer, _) = inner.update(cx, |project, cx| {
        project.register_agent_writer(AgentWriteScope::Anywhere, AgentWriterKind::Command, cx)
    });
    assert_eq!(states(cx), [Paused, Paused, Paused, Paused]);
    drop(writer);
}

#[gpui::test]
async fn test_a_write_reaches_worktrees_opened_through_another_spelling(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/real"), json!({ "src": { "main.rs": "" } }))
        .await;
    fs.create_symlink(Path::new(path!("/link")), PathBuf::from(path!("/real")))
        .await
        .unwrap();
    let through_link = Project::test(fs.clone(), [path!("/link").as_ref()], cx).await;
    let directly = Project::test(fs.clone(), [path!("/real").as_ref()], cx).await;
    cx.run_until_parked();
    let projects = [&through_link, &directly];
    let states = |cx: &mut TestAppContext| projects.map(|project| code_state(project, cx));
    use WorktreeCodeState::Paused;
    async fn resume_both(projects: [&Entity<Project>; 2], cx: &mut TestAppContext) {
        for project in projects {
            assert_eq!(resume(project, cx).await, ResumeOutcome::Resumed);
        }
    }

    // Through the link, to a path that does not exist yet.
    through_link
        .update(cx, |project, cx| {
            project.note_agent_write_to_path(Path::new(path!("/link/src/new/module.rs")), cx)
        })
        .await;
    assert_eq!(states(cx), [Paused, Paused]);
    resume_both(projects, cx).await;

    // As resolved.
    directly
        .update(cx, |project, cx| {
            project.note_agent_write_to_path(Path::new(path!("/real/src/main.rs")), cx)
        })
        .await;
    assert_eq!(states(cx), [Paused, Paused]);
    resume_both(projects, cx).await;

    // A command that can write through the link.
    let (writer, resolved) = through_link.update(cx, |project, cx| {
        project.register_agent_writer(
            AgentWriteScope::Paths(vec![PathBuf::from(path!("/link"))]),
            AgentWriterKind::Command,
            cx,
        )
    });
    resolved.await;
    assert_eq!(states(cx), [Paused, Paused]);
    assert_eq!(
        resume(&directly, cx).await,
        ResumeOutcome::WriterRunning,
        "the command covers the folder however it was opened"
    );
    drop(writer);
}

#[gpui::test]
async fn test_resume_waits_for_running_writers(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/root"),
        json!({ "src": { "main.rs": "fn main() {}\n" } }),
    )
    .await;
    let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
    cx.run_until_parked();
    let worktree_id = only_worktree(&project, cx);
    let execution = project.read_with(cx, |project, _| project.worktree_code_execution().clone());
    let writer_running = |cx: &mut TestAppContext| {
        execution.read_with(cx, |execution, _| {
            execution
                .details(worktree_id)
                .expect("the worktree is known")
                .sandboxed_writer_running
        })
    };
    let generation = |cx: &mut TestAppContext| {
        execution
            .read_with(cx, |execution, _| execution.generation(worktree_id))
            .expect("the worktree is known")
    };

    let (writer, _) = project.update(cx, |project, cx| {
        project.register_agent_writer(
            AgentWriteScope::Paths(vec![PathBuf::from(path!("/root"))]),
            AgentWriterKind::Command,
            cx,
        )
    });
    assert_eq!(
        code_state(&project, cx),
        WorktreeCodeState::Paused,
        "the writer's start is a write"
    );
    assert!(writer_running(cx));
    assert_eq!(resume(&project, cx).await, ResumeOutcome::WriterRunning);

    let before = generation(cx);
    fs.insert_file(path!("/root/src/generated.rs"), b"// generated\n".to_vec())
        .await;
    cx.run_until_parked();
    assert!(
        generation(cx) > before,
        "each change it makes is one of its writes"
    );

    drop(writer);
    cx.run_until_parked();
    assert!(!writer_running(cx));
    assert_eq!(resume(&project, cx).await, ResumeOutcome::Resumed);
    fs.insert_file(path!("/root/src/later.rs"), b"// mine\n".to_vec())
        .await;
    cx.run_until_parked();
    assert_eq!(
        code_state(&project, cx),
        WorktreeCodeState::Allowed,
        "once the writer exited, changes are no longer taken for its writes"
    );
}

#[gpui::test]
async fn test_a_paused_worktree_stays_paused_after_a_restart(cx: &mut TestAppContext) {
    init_test(cx);
    cx.update(|cx| cx.set_global(db::AppDatabase::test_new()));
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/root"),
        json!({ "src": { "main.rs": "fn main() {}\n" } }),
    )
    .await;

    let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
    cx.run_until_parked();
    assert_eq!(code_state(&project, cx), WorktreeCodeState::Allowed);
    note_agent_write(&project, cx);
    cx.run_until_parked();
    drop(project);
    cx.run_until_parked();

    let reopened = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
    cx.run_until_parked();
    assert_eq!(
        code_state(&reopened, cx),
        WorktreeCodeState::Paused,
        "a restart does not resume what the user has not"
    );
    assert_eq!(resume(&reopened, cx).await, ResumeOutcome::Resumed);
    cx.run_until_parked();
    drop(reopened);
    cx.run_until_parked();

    let reopened_again = Project::test(fs, [path!("/root").as_ref()], cx).await;
    cx.run_until_parked();
    assert_eq!(code_state(&reopened_again, cx), WorktreeCodeState::Allowed);
}

#[gpui::test]
async fn test_a_pause_is_kept_under_the_canonical_root(cx: &mut TestAppContext) {
    init_test(cx);
    cx.update(|cx| cx.set_global(db::AppDatabase::test_new()));
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/Work/Project"),
        json!({ "src": { "main.rs": "fn main() {}\n" } }),
    )
    .await;
    fs.create_symlink(
        Path::new(path!("/link")),
        PathBuf::from(path!("/Work/Project")),
    )
    .await
    .unwrap();

    let through_link = Project::test(fs.clone(), [path!("/link").as_ref()], cx).await;
    cx.run_until_parked();
    through_link
        .update(cx, |project, cx| {
            project.note_agent_write_to_path(Path::new(path!("/link/src/main.rs")), cx)
        })
        .await;
    cx.run_until_parked();
    drop(through_link);
    cx.run_until_parked();

    let keys = cx.update(|cx| {
        db::kvp::KeyValueStore::from_app_db(cx.global::<db::AppDatabase>())
            .scoped("worktree_code_execution")
            .read_all()
            .expect("the stored pauses are readable")
            .into_iter()
            .map(|(key, _)| key)
            .collect::<Vec<_>>()
    });
    let root = path!("/Work/Project");
    let root = if cfg!(any(target_os = "windows", target_os = "macos")) {
        root.to_lowercase()
    } else {
        root.to_string()
    };
    assert_eq!(keys, vec![format!("this-machine\n{root}")]);

    let directly = Project::test(fs, [path!("/Work/Project").as_ref()], cx).await;
    cx.run_until_parked();
    assert_eq!(
        code_state(&directly, cx),
        WorktreeCodeState::Paused,
        "the same folder, however it was opened"
    );
}

#[gpui::test]
async fn test_without_a_lasting_database_every_worktree_starts_paused(cx: &mut TestAppContext) {
    init_test(cx);
    cx.update(|cx| {
        cx.set_global(db::AppDatabase::test_new());
        cx.set_global(StoredPausesAreNotKeptForTests);
    });
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/root"),
        json!({ "src": { "main.rs": "fn main() {}\n" } }),
    )
    .await;
    let project = Project::test(fs, [path!("/root").as_ref()], cx).await;
    cx.run_until_parked();
    assert_eq!(
        code_state(&project, cx),
        WorktreeCodeState::Paused,
        "what was left paused cannot be known"
    );
    assert_eq!(resume(&project, cx).await, ResumeOutcome::Resumed);
}

#[gpui::test]
async fn test_only_the_user_settings_can_turn_the_pause_off(cx: &mut TestAppContext) {
    init_test(cx);
    let turned_off = json!({ "agent": { "pause_worktree_code_after_agent_writes": false } });
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/root"),
        json!({
            ".zed": { "settings.json": turned_off.to_string() },
            "src": { "main.rs": "fn main() {}\n" },
        }),
    )
    .await;
    let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
    cx.run_until_parked();

    note_agent_write(&project, cx);
    assert_eq!(
        code_state(&project, cx),
        WorktreeCodeState::Paused,
        "project settings cannot turn the pause off"
    );

    cx.update(|cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store
                .set_server_settings(&turned_off.to_string(), cx)
                .expect("valid server settings")
        })
    });
    cx.run_until_parked();
    assert_eq!(resume(&project, cx).await, ResumeOutcome::Resumed);
    note_agent_write(&project, cx);
    assert_eq!(
        code_state(&project, cx),
        WorktreeCodeState::Paused,
        "neither can a remote host's server settings"
    );

    set_pause_in_user_settings(false, cx);
    cx.run_until_parked();
    assert_eq!(
        code_state(&project, cx),
        WorktreeCodeState::Allowed,
        "turned off in the user's own settings, the pause lets go of what it held"
    );
    note_agent_write(&project, cx);
    assert_eq!(code_state(&project, cx), WorktreeCodeState::Allowed);
}

#[gpui::test]
async fn test_the_pause_is_off_until_the_user_settings_turn_it_on(cx: &mut TestAppContext) {
    crate::init_test(cx);
    let turned_on = json!({ "agent": { "pause_worktree_code_after_agent_writes": true } });
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/root"),
        json!({
            ".zed": { "settings.json": turned_on.to_string() },
            "src": { "main.rs": "fn main() {}
        " },
        }),
    )
    .await;
    let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
    cx.run_until_parked();

    note_agent_write(&project, cx);
    assert_eq!(
        code_state(&project, cx),
        WorktreeCodeState::Allowed,
        "off by default, and project settings cannot turn it on"
    );

    set_pause_in_user_settings(true, cx);
    cx.run_until_parked();
    note_agent_write(&project, cx);
    assert_eq!(code_state(&project, cx), WorktreeCodeState::Paused);
}

// The pause restarts the servers that run the worktree's code. It reopens in
// them only the buffers still held once the old ones are stopped: an excerpt
// an editor lets go of when its server goes away would otherwise stay open in
// the restarted server, as nothing would ever close it there.
#[gpui::test]
async fn test_the_pause_reopens_only_the_buffers_still_held(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    let mut tree = rust_worktree();
    tree["src"]["scrolled_away.rs"] = json!("fn scrolled_away() {}\n");
    fs.insert_tree(path!("/root"), tree).await;
    let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
    let language_registry = project.read_with(cx, |project, _| project.languages().clone());
    language_registry.add(rust_lang());
    let mut servers = language_registry.register_fake_lsp(
        "Rust",
        FakeLspAdapter {
            name: "restricted-server",
            worktree_code_paused_mode: WorktreeCodePausedMode::Restricted {
                initialization_options: json!({ "buildScripts": { "enable": false } }),
                workspace_configuration: json!({}),
                unchanged_files: RUST_GUARD_FILES,
                formatting_runs_programs: false,
            },
            ..FakeLspAdapter::default()
        },
    );
    cx.run_until_parked();
    let lsp_store = project.read_with(cx, |project, _| project.lsp_store());
    let (_held, _held_handle) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/root/src/main.rs"), cx)
        })
        .await
        .expect("opening main.rs");
    let (_scrolled_away, scrolled_away_handle) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/root/src/scrolled_away.rs"), cx)
        })
        .await
        .expect("opening scrolled_away.rs");
    let mut first = servers.next().await.expect("the server starts");
    cx.run_until_parked();
    let mut opened_in_first = Vec::new();
    while let Some(Some(params)) = first
        .try_receive_notification::<lsp::notification::DidOpenTextDocument>()
        .now_or_never()
    {
        opened_in_first.push(params.text_document.uri);
    }
    assert_eq!(opened_in_first.len(), 2);

    let scrolled_away_handle = Rc::new(RefCell::new(Some(scrolled_away_handle)));
    let _subscription = cx.update(|cx| {
        let scrolled_away_handle = scrolled_away_handle.clone();
        cx.subscribe(&lsp_store, move |_, event, _| {
            if let LspStoreEvent::LanguageServerRemoved(_) = event {
                scrolled_away_handle.borrow_mut().take();
            }
        })
    });
    note_agent_write(&project, cx);
    cx.run_until_parked();
    let mut restricted = servers
        .next()
        .await
        .expect("the server restarts in its paused mode");
    cx.run_until_parked();
    assert!(
        scrolled_away_handle.borrow().is_none(),
        "the old server was removed"
    );
    let mut opened_in_restricted = Vec::new();
    while let Some(Some(params)) = restricted
        .try_receive_notification::<lsp::notification::DidOpenTextDocument>()
        .now_or_never()
    {
        opened_in_restricted.push(params.text_document.uri);
    }
    assert_eq!(
        opened_in_restricted,
        [lsp::Uri::from_file_path(path!("/root/src/main.rs")).expect("an absolute path")]
    );
}
