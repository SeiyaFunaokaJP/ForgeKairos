use std::{
    borrow::Cow,
    cell::RefCell,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use collections::{HashMap, HashSet};
use fs::{FakeFs, Fs};
use futures::{FutureExt, StreamExt};
use gpui::{AppContext as _, Entity, TestAppContext, UpdateGlobal as _};
use language::{
    Buffer, CodeLabel, DiagnosticSourceKind, FakeLspAdapter, HighlightId, LocalFile, rust_lang,
};
use lsp::{LanguageServerId, LanguageServerName, Uri};
use parking_lot::Mutex;
use project::{
    DiagnosticSummary, Event, Project,
    lsp_store::{
        log_store::{TestRpcLogHeaderState, TestRpcRequestTracker},
        *,
    },
    project_launch_approvals::{
        LaunchApprovalKey, LaunchDecision, LaunchHost, LaunchKind, ProjectLaunchApprovals,
        ProjectLaunchApprovalsEvent,
    },
};
use serde_json::json;
use settings::SettingsStore;
use unindent::Unindent;
use util::{path, rel_path::rel_path};

use crate::init_test;

#[gpui::test]
async fn test_project_language_server_binary_waits_for_approval(cx: &mut TestAppContext) {
    init_test(cx);
    cx.update(|cx| cx.set_global(db::AppDatabase::test_new()));

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/the-root"),
        json!({
            ".zed": {
                "settings.json": json!({
                    "languages": { "Rust": { "language_servers": ["project_lsp"] } },
                    "lsp": {
                        "project_lsp": {
                            "binary": { "path": "project-lsp-binary", "arguments": ["--stdio"] }
                        }
                    },
                })
                .to_string(),
            },
            "src": { "main.rs": "fn main() {}" },
        }),
    )
    .await;
    let project = Project::test(fs, [path!("/the-root").as_ref()], cx).await;
    let language_registry = project.read_with(cx, |project, _| project.languages().clone());
    language_registry.add(rust_lang());
    let mut fake_servers = language_registry.register_fake_lsp(
        "Rust",
        FakeLspAdapter {
            name: "project_lsp",
            ..Default::default()
        },
    );
    cx.run_until_parked();

    let _buffer_with_lsp = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/the-root/src/main.rs"), cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();
    assert!(
        fake_servers.try_recv().is_err(),
        "the language server must wait until its binary is approved"
    );

    let approvals = cx.update(ProjectLaunchApprovals::global);
    let pending = approvals.read_with(cx, |approvals, _| approvals.pending_launches().to_vec());
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].kind, LaunchKind::LanguageServerBinary);
    assert_eq!(pending[0].name.to_string(), "project_lsp");
    assert_eq!(
        pending[0].target.display().to_string(),
        "project-lsp-binary --stdio"
    );

    approvals
        .update(cx, |approvals, cx| {
            approvals.approve_pending(&pending[0].key, cx)
        })
        .await
        .expect("the approval is stored");
    let server = fake_servers
        .next()
        .await
        .expect("the language server starts once approved");
    assert_eq!(
        server.binary.path,
        std::path::PathBuf::from("project-lsp-binary")
    );
}

// The key covers the script a binary runs, and is computed again after the
// decision: a script swapped between what the user saw and the approval
// needs its own approval.
#[gpui::test]
async fn test_project_language_server_binary_is_checked_again_after_approval(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    cx.update(|cx| cx.set_global(db::AppDatabase::test_new()));

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/the-root"),
        json!({
            ".zed": {
                "settings.json": json!({
                    "languages": { "Rust": { "language_servers": ["project_lsp"] } },
                    "lsp": {
                        "project_lsp": {
                            "binary": { "path": "node", "arguments": ["scripts/lsp.js"] }
                        }
                    },
                })
                .to_string(),
            },
            "scripts": { "lsp.js": "serve()" },
            "src": { "main.rs": "fn main() {}" },
        }),
    )
    .await;
    let project = Project::test(fs.clone(), [path!("/the-root").as_ref()], cx).await;
    let language_registry = project.read_with(cx, |project, _| project.languages().clone());
    language_registry.add(rust_lang());
    let mut fake_servers = language_registry.register_fake_lsp(
        "Rust",
        FakeLspAdapter {
            name: "project_lsp",
            ..Default::default()
        },
    );
    cx.run_until_parked();
    let _buffer_with_lsp = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/the-root/src/main.rs"), cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();

    let approvals = cx.update(ProjectLaunchApprovals::global);
    let shown = approvals.read_with(cx, |approvals, _| {
        approvals
            .pending_launches()
            .first()
            .cloned()
            .expect("the binary waits for approval")
    });
    assert_eq!(shown.referenced_worktree_files.len(), 1);

    fs.insert_file(path!("/the-root/scripts/lsp.js"), b"steal()".to_vec())
        .await;
    approvals
        .update(cx, |approvals, cx| {
            approvals.approve_pending(&shown.key, cx)
        })
        .await
        .expect("the approval is stored");
    cx.run_until_parked();
    assert!(
        fake_servers.try_recv().is_err(),
        "the changed script is not what was approved"
    );
    let pending = approvals.read_with(cx, |approvals, _| approvals.pending_launches().to_vec());
    assert_eq!(pending.len(), 1);
    assert_ne!(pending[0].key, shown.key);

    approvals
        .update(cx, |approvals, cx| {
            approvals.approve_pending(&pending[0].key, cx)
        })
        .await
        .expect("the approval is stored");
    let server = fake_servers
        .next()
        .await
        .expect("the language server starts once the current script is approved");
    assert_eq!(server.binary.path, std::path::PathBuf::from("node"));
}

#[gpui::test]
async fn test_user_language_server_binary_needs_no_approval(cx: &mut TestAppContext) {
    init_test(cx);
    cx.update(|cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store
                .set_user_settings(
                    &json!({
                        "languages": { "Rust": { "language_servers": ["user_lsp"] } },
                        "lsp": { "user_lsp": { "binary": { "path": "user-lsp-binary" } } },
                    })
                    .to_string(),
                    cx,
                )
                .expect("the user settings parse");
        });
    });

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/the-root"),
        json!({ "src": { "main.rs": "fn main() {}" } }),
    )
    .await;
    let project = Project::test(fs, [path!("/the-root").as_ref()], cx).await;
    let language_registry = project.read_with(cx, |project, _| project.languages().clone());
    language_registry.add(rust_lang());
    let mut fake_servers = language_registry.register_fake_lsp(
        "Rust",
        FakeLspAdapter {
            name: "user_lsp",
            ..Default::default()
        },
    );
    cx.run_until_parked();

    let _buffer_with_lsp = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/the-root/src/main.rs"), cx)
        })
        .await
        .unwrap();
    let server = fake_servers
        .next()
        .await
        .expect("a binary from the user's settings starts right away");
    assert_eq!(
        server.binary.path,
        std::path::PathBuf::from("user-lsp-binary")
    );
    let pending_count = cx.update(|cx| {
        ProjectLaunchApprovals::global(cx)
            .read(cx)
            .pending_launches()
            .len()
    });
    assert_eq!(pending_count, 0);
}

#[gpui::test]
async fn test_project_external_formatter_waits_for_approval(cx: &mut TestAppContext) {
    const FORMATTER: &str = "zed-project-formatter-that-is-not-installed";
    init_test(cx);
    cx.update(|cx| cx.set_global(db::AppDatabase::test_new()));

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/the-root"),
        json!({
            ".zed": {
                "settings.json": json!({
                    "format_on_save": "on",
                    "formatter": { "external": { "command": FORMATTER, "arguments": ["--stdin"] } },
                })
                .to_string(),
            },
            "src": { "main.rs": "fn main() {}\n" },
        }),
    )
    .await;
    let project = Project::test(fs.clone(), [path!("/the-root").as_ref()], cx).await;
    project.read_with(cx, |project, _| project.languages().add(rust_lang()));
    let buffer = project
        .update(cx, |project, cx| {
            project.open_local_buffer(path!("/the-root/src/main.rs"), cx)
        })
        .await
        .unwrap();
    buffer.update(cx, |buffer, cx| {
        buffer.edit([(0..0, "// edited\n")], None, cx)
    });

    // What the agent's edit tool does: format for saving, then save.
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
        .expect("an unapproved formatter is skipped rather than failing the format");
    project
        .update(cx, |project, cx| project.save_buffer(buffer.clone(), cx))
        .await
        .expect("the save goes through");
    assert_eq!(
        fs.load(path!("/the-root/src/main.rs").as_ref())
            .await
            .unwrap(),
        "// edited\nfn main() {}\n"
    );

    let approvals = cx.update(ProjectLaunchApprovals::global);
    let pending = approvals.read_with(cx, |approvals, _| approvals.pending_launches().to_vec());
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].kind, LaunchKind::ExternalFormatter);
    assert_eq!(
        pending[0].target.display().to_string(),
        format!("{FORMATTER} --stdin")
    );
    let first_key = pending[0].key.clone();
    let events = Rc::new(RefCell::new(Vec::new()));
    let _subscription = cx.update(|cx| {
        let events = events.clone();
        cx.subscribe(&approvals, move |_, event, _| {
            events.borrow_mut().push(event.clone())
        })
    });

    // Varying the arguments on every save replaces the pending entry instead
    // of piling up entries and notifications.
    fs.insert_file(
        path!("/the-root/.zed/settings.json"),
        json!({
            "format_on_save": "on",
            "formatter": {
                "external": { "command": FORMATTER, "arguments": ["--stdin", "--varied"] }
            },
        })
        .to_string()
        .into_bytes(),
    )
    .await;
    cx.run_until_parked();
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
        .expect("an unapproved formatter is skipped rather than failing the format");
    let pending = approvals.read_with(cx, |approvals, _| approvals.pending_launches().to_vec());
    assert_eq!(pending.len(), 1);
    assert_eq!(
        pending[0].target.display().to_string(),
        format!("{FORMATTER} --stdin --varied")
    );
    assert!(
        matches!(
            events.borrow().as_slice(),
            [ProjectLaunchApprovalsEvent::PendingLaunchReplaced { old, new }]
                if old == &first_key && new.key == pending[0].key
        ),
        "one live notification per launch, updated in place: {:?}",
        events.borrow()
    );

    // Once approved the formatter really runs, and as it is not installed
    // the format now fails.
    approvals
        .update(cx, |approvals, cx| {
            approvals.approve_pending(&pending[0].key, cx)
        })
        .await
        .expect("the approval is stored");
    let result = project
        .update(cx, |project, cx| {
            project.format(
                HashSet::from_iter([buffer.clone()]),
                LspFormatTarget::Buffers,
                true,
                FormatTrigger::Save,
                cx,
            )
        })
        .await;
    assert!(
        result.is_err(),
        "the approved formatter should have been run"
    );
}

// A binary path that climbs out with `..` runs from the worktree root, which
// its approval key resolved it against, rather than from Zed's own working
// directory.
#[gpui::test]
async fn test_project_language_server_binary_with_dot_dot_runs_from_the_worktree_root(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    cx.update(|cx| cx.set_global(db::AppDatabase::test_new()));

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/the-root"),
        json!({
            ".zed": {
                "settings.json": json!({
                    "languages": { "Rust": { "language_servers": ["project_lsp"] } },
                    "lsp": { "project_lsp": { "binary": { "path": "../the-root/bin/lsp" } } },
                })
                .to_string(),
            },
            "bin": { "lsp": "serve" },
            "src": { "main.rs": "fn main() {}" },
        }),
    )
    .await;
    let project = Project::test(fs, [path!("/the-root").as_ref()], cx).await;
    let language_registry = project.read_with(cx, |project, _| project.languages().clone());
    language_registry.add(rust_lang());
    let mut fake_servers = language_registry.register_fake_lsp(
        "Rust",
        FakeLspAdapter {
            name: "project_lsp",
            ..Default::default()
        },
    );
    cx.run_until_parked();
    let _buffer_with_lsp = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/the-root/src/main.rs"), cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();

    let approvals = cx.update(ProjectLaunchApprovals::global);
    let pending = approvals.read_with(cx, |approvals, _| approvals.pending_launches().to_vec());
    assert_eq!(pending.len(), 1);
    assert_eq!(
        pending[0]
            .referenced_worktree_files
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        vec![path!("/the-root/bin/lsp").to_string()],
        "the binary is a worktree file, bound by its contents"
    );
    approvals
        .update(cx, |approvals, cx| {
            approvals.approve_pending(&pending[0].key, cx)
        })
        .await
        .expect("the approval is stored");
    let server = fake_servers
        .next()
        .await
        .expect("the language server starts once approved");
    assert_eq!(
        server.binary.path,
        Path::new(path!("/the-root")).join("../the-root/bin/lsp")
    );
}

// A project-defined binary runs only while its approval stands.
#[gpui::test]
async fn test_revoking_a_language_server_binary_approval_stops_it(cx: &mut TestAppContext) {
    init_test(cx);
    cx.update(|cx| cx.set_global(db::AppDatabase::test_new()));

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/the-root"),
        json!({
            ".zed": {
                "settings.json": json!({
                    "languages": { "Rust": { "language_servers": ["project_lsp"] } },
                    "lsp": { "project_lsp": { "binary": { "path": "project-lsp-binary" } } },
                })
                .to_string(),
            },
            "src": { "main.rs": "fn main() {}" },
        }),
    )
    .await;
    let project = Project::test(fs, [path!("/the-root").as_ref()], cx).await;
    let language_registry = project.read_with(cx, |project, _| project.languages().clone());
    language_registry.add(rust_lang());
    let mut fake_servers = language_registry.register_fake_lsp(
        "Rust",
        FakeLspAdapter {
            name: "project_lsp",
            ..Default::default()
        },
    );
    cx.run_until_parked();
    let _buffer_with_lsp = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/the-root/src/main.rs"), cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();

    let approvals = cx.update(ProjectLaunchApprovals::global);
    let key = approvals
        .read_with(cx, |approvals, _| {
            approvals
                .pending_launches()
                .first()
                .map(|pending| pending.key.clone())
        })
        .expect("the binary waits for approval");
    approvals
        .update(cx, |approvals, cx| approvals.approve_pending(&key, cx))
        .await
        .expect("the approval is stored");
    let _server = fake_servers
        .next()
        .await
        .expect("the language server starts once approved");
    cx.run_until_parked();
    let running = |cx: &mut TestAppContext| {
        project.read_with(cx, |project, cx| {
            project
                .lsp_store()
                .read(cx)
                .language_server_statuses()
                .any(|(_, status)| &*status.name.0 == "project_lsp")
        })
    };
    assert!(running(cx));

    approvals
        .update(cx, |approvals, cx| approvals.revoke(&key, cx))
        .await
        .expect("the approval is revoked");
    cx.run_until_parked();
    assert!(!running(cx), "the server stops with its approval");
}

// The settings page lists decisions rather than pending launches: taking one
// back by its record has to reach the same path the MCP servers page takes,
// down to stopping the server and asking again.
#[gpui::test]
async fn test_revoking_a_listed_decision_stops_the_server_and_asks_again(cx: &mut TestAppContext) {
    init_test(cx);
    cx.update(|cx| cx.set_global(db::AppDatabase::test_new()));

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/the-root"),
        json!({
            ".zed": {
                "settings.json": json!({
                    "languages": { "Rust": { "language_servers": ["project_lsp"] } },
                    "lsp": { "project_lsp": { "binary": { "path": "project-lsp-binary" } } },
                })
                .to_string(),
            },
            "src": { "main.rs": "fn main() {}" },
        }),
    )
    .await;
    let project = Project::test(fs, [path!("/the-root").as_ref()], cx).await;
    let language_registry = project.read_with(cx, |project, _| project.languages().clone());
    language_registry.add(rust_lang());
    let mut fake_servers = language_registry.register_fake_lsp(
        "Rust",
        FakeLspAdapter {
            name: "project_lsp",
            ..Default::default()
        },
    );
    cx.run_until_parked();
    let _buffer_with_lsp = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/the-root/src/main.rs"), cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();

    let approvals = cx.update(ProjectLaunchApprovals::global);
    let key = approvals
        .read_with(cx, |approvals, _| {
            approvals
                .pending_launches()
                .first()
                .map(|pending| pending.key.clone())
        })
        .expect("the binary waits for approval");
    approvals
        .update(cx, |approvals, cx| approvals.approve_pending(&key, cx))
        .await
        .expect("the approval is stored");
    fake_servers
        .next()
        .await
        .expect("the language server starts once approved");
    cx.run_until_parked();
    let running = |cx: &mut TestAppContext| {
        project.read_with(cx, |project, cx| {
            project
                .lsp_store()
                .read(cx)
                .language_server_statuses()
                .any(|(_, status)| &*status.name.0 == "project_lsp")
        })
    };
    assert!(running(cx));

    let record = approvals
        .read_with(cx, |approvals, _| {
            approvals
                .decision_records()
                .expect("the decisions are readable")
                .into_iter()
                .find(|record| record.kind == LaunchKind::LanguageServerBinary)
        })
        .expect("the approval is listed for review");
    assert_eq!(record.name, "project_lsp");
    assert_eq!(record.decision, LaunchDecision::Approved);
    assert_eq!(record.approval_key(), key);
    assert!(
        record.is_for(
            LaunchKind::LanguageServerBinary,
            "project_lsp",
            &LaunchHost::ThisMachine,
            Path::new(path!("/the-root")),
        ),
        "the row has to be findable from the server it started: {record:?}"
    );

    // A revoked launch has to raise its notification again: the dialog that
    // approves a language server is only reachable from it, so without one the
    // binary would wait for an approval nothing can give until Zed restarts.
    let events = Rc::new(RefCell::new(Vec::new()));
    let _subscription = cx.update(|cx| {
        let events = events.clone();
        cx.subscribe(&approvals, move |_, event, _| {
            events.borrow_mut().push(event.clone())
        })
    });
    approvals
        .update(cx, |approvals, cx| {
            approvals.revoke(&record.approval_key(), cx)
        })
        .await
        .expect("the approval is revoked");
    cx.run_until_parked();
    assert!(!running(cx), "the server stops with its approval");
    assert_eq!(
        approvals.read_with(cx, |approvals, _| approvals
            .decision_records()
            .expect("the decisions are readable")
            .len()),
        0,
        "the revoked decision leaves the list"
    );

    project.update(cx, |project, cx| {
        project.lsp_store().update(cx, |lsp_store, cx| {
            lsp_store.restart_all_language_servers(cx)
        });
    });
    cx.run_until_parked();
    assert!(
        approvals.read_with(cx, |approvals, _| {
            approvals
                .pending_launches()
                .iter()
                .any(|pending| pending.name == "project_lsp")
        }),
        "the binary asks to be approved again"
    );
    assert!(
        events.borrow().iter().any(|event| matches!(
            event,
            ProjectLaunchApprovalsEvent::PendingLaunchAppeared(details)
                if details.name.as_ref() == "project_lsp"
        )),
        "and notifies about it: {:?}",
        events.borrow()
    );
    assert!(!running(cx), "and does not run in the meantime");
}

// A language server waiting for its binary to be approved asks for its own
// definition once, however often its wait is cut short, and stops asking once
// it is stopped.
#[gpui::test]
async fn test_language_server_waiting_for_approval_asks_once_until_stopped(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    cx.update(|cx| cx.set_global(db::AppDatabase::test_new()));

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/the-root"),
        json!({
            ".zed": {
                "settings.json": json!({
                    "languages": { "Rust": { "language_servers": ["project_lsp"] } },
                    "lsp": { "project_lsp": { "binary": { "path": "project-lsp-binary" } } },
                })
                .to_string(),
            },
            "src": { "main.rs": "fn main() {}" },
        }),
    )
    .await;
    let project = Project::test(fs, [path!("/the-root").as_ref()], cx).await;
    let language_registry = project.read_with(cx, |project, _| project.languages().clone());
    language_registry.add(rust_lang());
    let mut fake_servers = language_registry.register_fake_lsp(
        "Rust",
        FakeLspAdapter {
            name: "project_lsp",
            ..Default::default()
        },
    );
    cx.run_until_parked();
    let _buffer_with_lsp = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/the-root/src/main.rs"), cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();

    let approvals = cx.update(ProjectLaunchApprovals::global);
    let shown = approvals.read_with(cx, |approvals, _| approvals.pending_launches().to_vec());
    assert_eq!(shown.len(), 1);
    let mut other_definition = shown[0].clone();
    other_definition.key = LaunchApprovalKey::for_tests("another-definition");
    let other_window = cx.new(|_| ()).entity_id();
    let pending_keys = |cx: &mut TestAppContext| {
        approvals.read_with(cx, |approvals, _| {
            approvals
                .pending_launches()
                .iter()
                .map(|pending| pending.key.clone())
                .collect::<Vec<_>>()
        })
    };

    // A newer definition of the same launch cuts the wait short; the server
    // looks again and asks for the definition it would run, once.
    approvals.update(cx, |approvals, cx| {
        approvals.request_launch(other_definition.clone(), other_window, cx);
    });
    cx.run_until_parked();
    assert_eq!(pending_keys(cx), vec![shown[0].key.clone()]);

    // Stopped, it no longer asks.
    project.update(cx, |project, cx| {
        project
            .lsp_store()
            .update(cx, |lsp_store, cx| lsp_store.stop_all_language_servers(cx))
    });
    approvals.update(cx, |approvals, cx| {
        approvals.request_launch(other_definition.clone(), other_window, cx);
    });
    cx.run_until_parked();
    assert_eq!(
        pending_keys(cx),
        vec![other_definition.key],
        "a stopped server no longer asks for its definition"
    );
    assert!(fake_servers.try_recv().is_err());
}

#[gpui::test]
async fn test_diagnostic_batches_skip_paths_without_worktrees(cx: &mut TestAppContext) {
    init_test(cx);

    for skipped_index in 0..=2 {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/dir"), json!({ "a.rs": "one", "b.rs": "two" }))
            .await;
        let project = Project::test(fs, [Path::new(path!("/dir"))], cx).await;
        let lsp_store = project.read_with(cx, |project, _| project.lsp_store());
        let buffer_a = project
            .update(cx, |project, cx| {
                project.open_local_buffer(path!("/dir/a.rs"), cx)
            })
            .await
            .unwrap();
        let worktree_id =
            buffer_a.read_with(cx, |buffer, cx| buffer.file().unwrap().worktree_id(cx));
        let server_id = LanguageServerId(0);

        for message in [Some("error"), None] {
            cx.run_until_parked();
            project.read_with(cx, |project, cx| {
                assert_eq!(
                    project.get_open_buffer(&(worktree_id, rel_path("b.rs")).into(), cx),
                    None
                );
            });
            let mut events = cx.events(&project);
            let mut paths = vec![path!("/dir/a.rs"), path!("/dir/b.rs")];
            paths.insert(skipped_index, path!("/outside.rs"));
            let updates = paths
                .into_iter()
                .map(|path| DocumentDiagnosticsUpdate {
                    diagnostics: lsp::PublishDiagnosticsParams {
                        uri: Uri::from_file_path(path).unwrap(),
                        version: None,
                        diagnostics: message
                            .into_iter()
                            .map(|message| lsp::Diagnostic {
                                range: lsp::Range::new(
                                    lsp::Position::new(0, 0),
                                    lsp::Position::new(0, 3),
                                ),
                                severity: Some(lsp::DiagnosticSeverity::ERROR),
                                message: lsp::DiagnosticMessage::from(message),
                                ..lsp::Diagnostic::default()
                            })
                            .collect(),
                    },
                    result_id: None,
                    registration_id: None,
                    server_id,
                    disk_based_sources: Cow::Borrowed(&[]),
                })
                .collect();
            lsp_store.update(cx, |lsp_store, cx| {
                lsp_store
                    .merge_lsp_diagnostics(
                        DiagnosticSourceKind::Pushed,
                        updates,
                        |_, _, _| false,
                        cx,
                    )
                    .unwrap();
            });
            cx.run_until_parked();

            project.read_with(cx, |project, cx| {
                assert_eq!(
                    project.diagnostic_summary(false, cx),
                    DiagnosticSummary {
                        error_count: if message.is_some() { 2 } else { 0 },
                        warning_count: 0,
                    },
                    "skipped update at index {skipped_index}, message {message:?}"
                );
            });
            let diagnostic_events = std::iter::from_fn(|| events.next().now_or_never().flatten())
                .filter_map(|event| match event {
                    Event::DiagnosticsUpdated {
                        language_server_id,
                        paths,
                    } => Some((language_server_id, paths)),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(
                diagnostic_events,
                vec![(
                    server_id,
                    vec![
                        (worktree_id, rel_path("a.rs")).into(),
                        (worktree_id, rel_path("b.rs")).into(),
                    ],
                )],
                "skipped update at index {skipped_index}, message {message:?}"
            );

            let buffer_b = project
                .update(cx, |project, cx| {
                    project.open_local_buffer(path!("/dir/b.rs"), cx)
                })
                .await
                .unwrap();
            for buffer in [&buffer_a, &buffer_b] {
                buffer.read_with(cx, |buffer, _| {
                    assert_eq!(
                        buffer
                            .buffer_diagnostics(Some(server_id))
                            .iter()
                            .map(|entry| entry.diagnostic.message.to_string())
                            .collect::<Vec<_>>(),
                        message.into_iter().collect::<Vec<_>>()
                    );
                });
            }
        }
    }
}

#[gpui::test]
async fn test_removing_invisible_worktree_cleans_reused_lsp_bookkeeping(cx: &mut TestAppContext) {
    init_test(cx);
    cx.executor().allow_parking();

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/the-root"), json!({ "main.rs": "fn main() {}" }))
        .await;
    fs.insert_tree(
        path!("/the-registry"),
        json!({ "dep": { "src": { "dep.rs": "pub fn dep() {}" } } }),
    )
    .await;

    let project = Project::test(fs, [path!("/the-root").as_ref()], cx).await;
    let language_registry = project.read_with(cx, |project, _| project.languages().clone());
    language_registry.add(rust_lang());
    let mut fake_servers = language_registry.register_fake_lsp("Rust", FakeLspAdapter::default());

    let (_visible_buffer, _visible_handle) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/the-root/main.rs"), cx)
        })
        .await
        .unwrap();
    fake_servers.next().await.unwrap();
    cx.run_until_parked();

    let server_id = project.read_with(cx, |project, cx| {
        project
            .lsp_store()
            .read(cx)
            .language_server_statuses()
            .next()
            .unwrap()
            .0
    });
    let external_buffer = project
        .update(cx, |project, cx| {
            project.open_local_buffer_via_lsp(
                Uri::from_file_path(path!("/the-registry/dep/src/dep.rs")).unwrap(),
                server_id,
                cx,
            )
        })
        .await
        .unwrap();
    cx.run_until_parked();

    let invisible_worktree_id =
        external_buffer.read_with(cx, |buffer, cx| buffer.file().unwrap().worktree_id(cx));
    project.read_with(cx, |project, cx| {
        let worktree = project.worktree_for_id(invisible_worktree_id, cx).unwrap();
        assert!(!worktree.read(cx).is_visible());
        assert!(
            project
                .lsp_store()
                .read(cx)
                .has_language_server_seed_for_worktree(invisible_worktree_id)
        );
    });

    project.update(cx, |project, cx| {
        project.remove_worktree(invisible_worktree_id, cx);
    });
    cx.run_until_parked();

    project.read_with(cx, |project, cx| {
        let lsp_store = project.lsp_store();
        let lsp_store = lsp_store.read(cx);
        assert!(
            lsp_store
                .language_server_statuses()
                .any(|(status_server_id, _)| status_server_id == server_id)
        );
        assert!(!lsp_store.has_language_server_seed_for_worktree(invisible_worktree_id));
    });
}

#[gpui::test]
async fn test_open_buffer_via_lsp_case_variant_no_duplicate(cx: &mut TestAppContext) {
    init_test(cx);
    cx.executor().allow_parking();

    let fs = FakeFs::new(cx.executor());
    fs.set_case_sensitive(false);
    fs.insert_tree(
        path!("/root"),
        json!({ "src": { "main.rs": "fn main() {}" } }),
    )
    .await;

    let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
    let language_registry = project.read_with(cx, |project, _| project.languages().clone());
    language_registry.add(rust_lang());
    let mut fake_servers = language_registry.register_fake_lsp("Rust", FakeLspAdapter::default());

    project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/root/src/main.rs"), cx)
        })
        .await
        .unwrap();
    fake_servers.next().await.unwrap();
    cx.run_until_parked();

    let server_id = project.read_with(cx, |project, cx| {
        project
            .lsp_store()
            .read(cx)
            .language_server_statuses()
            .next()
            .unwrap()
            .0
    });

    project
        .update(cx, |project, cx| {
            project.open_local_buffer_via_lsp(
                Uri::from_file_path(path!("/root/SRC/main.rs")).unwrap(),
                server_id,
                cx,
            )
        })
        .await
        .unwrap();
    cx.run_until_parked();

    project.read_with(cx, |project, cx| {
        let worktree = project.worktrees(cx).next().unwrap();
        let entries: Vec<_> = worktree
            .read(cx)
            .snapshot()
            .entries(true, 0)
            .map(|entry| entry.path.as_unix_str().to_string())
            .collect();
        assert_eq!(entries, vec!["", "src", "src/main.rs"]);
    });
}

#[gpui::test]
async fn test_open_buffer_via_lsp_preserves_external_symlink_path(cx: &mut TestAppContext) {
    init_test(cx);
    cx.executor().allow_parking();

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/shared"),
        json!({ "pkg": { "def.rs": "pub fn def() {}" } }),
    )
    .await;
    fs.insert_tree(
        path!("/project"),
        json!({ "src": { "main.rs": "fn main() {}" } }),
    )
    .await;
    fs.create_symlink(
        path!("/project/pkg").as_ref(),
        PathBuf::from(path!("/shared/pkg")),
    )
    .await
    .unwrap();

    let (project, server_id) =
        project_with_rust_server(fs, path!("/project"), path!("/project/src/main.rs"), cx).await;

    let buffer = project
        .update(cx, |project, cx| {
            project.open_local_buffer_via_lsp(
                Uri::from_file_path(path!("/project/pkg/def.rs")).unwrap(),
                server_id,
                cx,
            )
        })
        .await
        .unwrap();
    cx.run_until_parked();

    assert_eq!(
        buffer_paths(&buffer, cx),
        (
            "pkg/def.rs".to_string(),
            PathBuf::from(path!("/project/pkg/def.rs"))
        )
    );
    assert_eq!(
        worktree_roots(&project, cx),
        vec![PathBuf::from(path!("/project"))]
    );
}

#[gpui::test]
async fn test_open_buffer_via_lsp_case_variant_in_unscanned_dir(cx: &mut TestAppContext) {
    init_test(cx);
    cx.executor().allow_parking();

    let fs = FakeFs::new(cx.executor());
    fs.set_case_sensitive(false);
    fs.insert_tree(
        path!("/root"),
        json!({
            ".gitignore": "ignored\n",
            "src": { "main.rs": "fn main() {}" },
            "ignored": { "lib.rs": "pub fn lib() {}" },
        }),
    )
    .await;

    let (project, server_id) =
        project_with_rust_server(fs, path!("/root"), path!("/root/src/main.rs"), cx).await;
    assert_eq!(
        worktree_entries(&project, cx),
        vec!["", ".gitignore", "ignored", "src", "src/main.rs"]
    );

    let buffer = project
        .update(cx, |project, cx| {
            project.open_local_buffer_via_lsp(
                Uri::from_file_path(path!("/root/IGNORED/LIB.rs")).unwrap(),
                server_id,
                cx,
            )
        })
        .await
        .unwrap();
    cx.run_until_parked();

    assert_eq!(
        buffer_paths(&buffer, cx),
        (
            "ignored/lib.rs".to_string(),
            PathBuf::from(path!("/root/ignored/lib.rs"))
        )
    );
    assert_eq!(
        worktree_roots(&project, cx),
        vec![PathBuf::from(path!("/root"))]
    );
    assert_eq!(
        worktree_entries(&project, cx),
        vec![
            "",
            ".gitignore",
            "ignored",
            "ignored/lib.rs",
            "src",
            "src/main.rs"
        ]
    );
}

#[test]
fn test_rpc_log_grouping_separates_timed_messages() {
    for (received, direction) in [(false, "Send"), (true, "Receive")] {
        let mut header_state = TestRpcLogHeaderState::new();

        assert_eq!(
            header_state.header_for_message(received, None),
            Some(format!("\n// {direction}:"))
        );
        assert_eq!(header_state.header_for_message(received, None), None);
        assert_eq!(
            header_state.header_for_message(received, Some(Duration::from_millis(53))),
            Some(format!("\n// {direction} (took 53.0ms):"))
        );
        assert_eq!(
            header_state.header_for_message(received, None),
            Some(format!("\n// {direction}:"))
        );
        assert_eq!(header_state.header_for_message(received, None), None);
    }
}

#[test]
fn test_rpc_request_tracker_distinguishes_request_directions() {
    let mut tracker = TestRpcRequestTracker::new();
    let started_at = Instant::now();

    assert_eq!(
        tracker.observe(
            false,
            r#"{"jsonrpc":"2.0","id":1,"method":"textDocument/hover"}"#,
            started_at,
        ),
        None
    );
    assert_eq!(
        tracker.observe(
            true,
            r#"{"jsonrpc":"2.0","id":1,"method":"workspace/configuration"}"#,
            started_at + Duration::from_millis(10),
        ),
        None
    );
    assert_eq!(
        tracker.observe(
            false,
            r#"{"jsonrpc":"2.0","id":1,"result":[]}"#,
            started_at + Duration::from_millis(30),
        ),
        Some(Duration::from_millis(20))
    );
    assert_eq!(
        tracker.observe(
            true,
            r#"{"jsonrpc":"2.0","id":1,"result":null}"#,
            started_at + Duration::from_millis(50),
        ),
        Some(Duration::from_millis(50))
    );
}

#[test]
fn test_rpc_request_tracker_decodes_ids_and_times_cancelled_requests() {
    let mut tracker = TestRpcRequestTracker::new();
    let started_at = Instant::now();

    tracker.observe(
        true,
        r#"{"jsonrpc":"2.0","id":"foo\u002fbar","method":"workspace/configuration"}"#,
        started_at,
    );
    assert_eq!(
        tracker.observe(
            false,
            r#"{"jsonrpc":"2.0","id":"foo/bar","result":[]}"#,
            started_at + Duration::from_millis(25),
        ),
        Some(Duration::from_millis(25))
    );

    tracker.observe(
        false,
        r#"{"jsonrpc":"2.0","id":7,"method":"textDocument/hover"}"#,
        started_at,
    );
    tracker.observe(
        false,
        r#"{"jsonrpc":"2.0","method":"$/cancelRequest","params":{"id":7}}"#,
        started_at + Duration::from_millis(1),
    );
    assert_eq!(tracker.pending_request_count(), 1);
    assert_eq!(
        tracker.observe(
            true,
            r#"{"jsonrpc":"2.0","id":7,"error":{"code":-32800,"message":"Request was cancelled"}}"#,
            started_at + Duration::from_millis(10),
        ),
        Some(Duration::from_millis(10))
    );
    assert_eq!(tracker.pending_request_count(), 0);
}

#[test]
fn test_rpc_request_tracker_bounds_unanswered_requests() {
    let mut tracker = TestRpcRequestTracker::new();
    let started_at = Instant::now();
    let max_pending_requests = TestRpcRequestTracker::max_pending_requests();

    for id in 0..=max_pending_requests {
        tracker.observe(
            false,
            &format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"textDocument/hover"}}"#),
            started_at + Duration::from_nanos(id as u64),
        );
    }

    assert_eq!(tracker.pending_request_count(), max_pending_requests);
    assert_eq!(
        tracker.observe(
            true,
            r#"{"jsonrpc":"2.0","id":0,"result":null}"#,
            started_at + Duration::from_secs(1),
        ),
        None
    );
    assert!(
        tracker
            .observe(
                true,
                r#"{"jsonrpc":"2.0","id":1,"result":null}"#,
                started_at + Duration::from_secs(1),
            )
            .is_some()
    );
}

#[test]
fn test_rpc_log_duration_proto_roundtrip() {
    let log_type = LanguageServerLogType::Rpc {
        received: true,
        elapsed: Some(Duration::from_micros(1234)),
    };

    assert_eq!(
        LanguageServerLogType::from_proto(log_type.to_proto()),
        log_type
    );
}

#[test]
fn test_glob_literal_prefix() {
    assert_eq!(glob_literal_prefix(Path::new("**/*.js")), Path::new(""));
    assert_eq!(
        glob_literal_prefix(Path::new("node_modules/**/*.js")),
        Path::new("node_modules")
    );
    assert_eq!(
        glob_literal_prefix(Path::new("foo/{bar,baz}.js")),
        Path::new("foo")
    );
    assert_eq!(
        glob_literal_prefix(Path::new("foo/bar/baz.js")),
        Path::new("foo/bar/baz.js")
    );

    #[cfg(target_os = "windows")]
    {
        assert_eq!(glob_literal_prefix(Path::new("**\\*.js")), Path::new(""));
        assert_eq!(
            glob_literal_prefix(Path::new("node_modules\\**/*.js")),
            Path::new("node_modules")
        );
        assert_eq!(
            glob_literal_prefix(Path::new("foo/{bar,baz}.js")),
            Path::new("foo")
        );
        assert_eq!(
            glob_literal_prefix(Path::new("foo\\bar\\baz.js")),
            Path::new("foo/bar/baz.js")
        );
    }
}

#[test]
fn test_multi_len_chars_normalization() {
    let mut label = CodeLabel::new(
        "myElˇ (parameter) myElˇ: {\n    foo: string;\n}".to_string(),
        0..6,
        vec![(0..6, HighlightId::new(1))],
    );
    ensure_uniform_list_compatible_label(&mut label);
    assert_eq!(
        label,
        CodeLabel::new(
            "myElˇ (parameter) myElˇ: { foo: string; }".to_string(),
            0..6,
            vec![(0..6, HighlightId::new(1))],
        )
    );
}

#[test]
fn test_completion_label_snippet_normalization() {
    for line_ending in ["\n", "\r\n", "\r"] {
        let text = "
            #[cfg(test)]
            mod tests {
                use super::*;

                #[test]
                fn test_name() {

                }
            }"
        .unindent()
        .replace('\n', line_ending);
        let name_start = text.find("test_name").expect("snippet has a test name");
        let text_len = text.len();
        let mut label = CodeLabel::new(
            text,
            0..text_len,
            vec![
                (0..12, HighlightId::new(1)),
                (name_start..name_start + 9, HighlightId::TABSTOP_REPLACE_ID),
            ],
        );

        ensure_uniform_list_compatible_label(&mut label);

        assert_eq!(
            label,
            CodeLabel::new(
                "#[cfg(test)] mod tests { use super::*; #[test] fn test_name() { } }".to_string(),
                0..67,
                vec![
                    (0..12, HighlightId::new(1)),
                    (50..59, HighlightId::TABSTOP_REPLACE_ID),
                ],
            ),
            "line ending: {line_ending:?}",
        );
    }
}

#[test]
fn test_completion_label_unicode_normalization() {
    for line_ending in ["\n", "\r\n", "\r"] {
        let text = "
            héllo {
                🦀value: 世界,
            }"
        .unindent()
        .replace('\n', line_ending);
        let value_start = text.find("🦀value").expect("label has a value");
        let type_start = text.find("世界").expect("label has a type");
        let text_len = text.len();
        let mut label = CodeLabel::new(
            text,
            value_start..value_start + 9,
            vec![
                (0..text_len, HighlightId::new(0)),
                (0..6, HighlightId::new(1)),
                (
                    value_start..value_start + 9,
                    HighlightId::TABSTOP_REPLACE_ID,
                ),
                (type_start..type_start + 6, HighlightId::new(2)),
            ],
        );

        ensure_uniform_list_compatible_label(&mut label);

        assert_eq!(
            label,
            CodeLabel::new(
                "héllo { 🦀value: 世界, }".to_string(),
                9..18,
                vec![
                    (0..29, HighlightId::new(0)),
                    (0..6, HighlightId::new(1)),
                    (9..18, HighlightId::TABSTOP_REPLACE_ID),
                    (20..26, HighlightId::new(2)),
                ],
            ),
            "line ending: {line_ending:?}",
        );
    }
}

#[test]
fn test_completion_label_whitespace_normalization() {
    for line_ending in ["\n", "\r\n", "\r", "\n\r", "\r\r\n"] {
        for before in ["", " ", " \t "] {
            for after in ["", " ", " \t "] {
                let mut label = CodeLabel::plain(format!("{before}{line_ending}{after}"), None);
                ensure_uniform_list_compatible_label(&mut label);
                assert_eq!(label, CodeLabel::plain(" ".to_string(), None));
            }
        }
    }

    for text in ["", " ", " \t ", "héllo  世界", "héllo\t世界"] {
        let mut label = CodeLabel::plain(text.to_string(), None);
        let expected = label.clone();
        ensure_uniform_list_compatible_label(&mut label);
        assert_eq!(label, expected);
    }
}

#[test]
fn test_trailing_newline_in_completion_documentation() {
    let doc =
        lsp::Documentation::String("Inappropriate argument value (of correct type).\n".to_string());
    let completion_doc: CompletionDocumentation = doc.into();
    assert!(
        matches!(completion_doc, CompletionDocumentation::SingleLine(s) if s == "Inappropriate argument value (of correct type).")
    );

    let doc = lsp::Documentation::String("  some value  \n".to_string());
    let completion_doc: CompletionDocumentation = doc.into();
    assert!(matches!(
        completion_doc,
        CompletionDocumentation::SingleLine(s) if s == "some value"
    ));
}

#[gpui::test]
async fn test_user_initialization_options_override_adapter_arrays(cx: &mut TestAppContext) {
    init_test(cx);

    let user_settings = serde_json::json!({
        "lsp": {
            "the-fake-language-server": {
                "initialization_options": {
                    "preview": {
                        "background": {
                            "enabled": true,
                            "args": ["--data-plane-host=127.0.0.1:23635", "--invert-colors=never"],
                        },
                    },
                    "plugins": ["user-plugin"],
                    "userOnly": ["user"],
                },
            },
        },
    });

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/the-root"),
        json!({
            ".zed": {
                "settings.json": user_settings.to_string(),
            },
            "main.rs": "fn main() {}",
        }),
    )
    .await;

    let project = Project::test(fs, [path!("/the-root").as_ref()], cx).await;
    let language_registry = project.read_with(cx, |project, _| project.languages().clone());
    language_registry.add(rust_lang());

    let sent_initialization_options = Arc::new(Mutex::new(None));
    let mut fake_servers = language_registry.register_fake_lsp(
        "Rust",
        FakeLspAdapter {
            name: "the-fake-language-server",
            initialization_options: Some(json!({
                "preview": {
                    "background": {
                        "args": ["--data-plane-host=127.0.0.1:23635", "--invert-colors=never"],
                        "partialRendering": true,
                    },
                },
                "plugins": ["default-plugin", "user-plugin"],
                "adapterOnly": [1, 2],
            })),
            initializer: Some(Box::new({
                let sent_initialization_options = sent_initialization_options.clone();
                move |fake_server| {
                    let sent_initialization_options = sent_initialization_options.clone();
                    fake_server.set_request_handler::<lsp::request::Initialize, _, _>(
                        move |params, _| {
                            *sent_initialization_options.lock() = params.initialization_options;
                            async move { Ok(lsp::InitializeResult::default()) }
                        },
                    );
                }
            })),
            ..FakeLspAdapter::default()
        },
    );
    cx.run_until_parked();

    project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/the-root/main.rs"), cx)
        })
        .await
        .unwrap();
    fake_servers.next().await.unwrap();
    cx.run_until_parked();

    assert_eq!(
        sent_initialization_options.lock().take(),
        Some(json!({
            "preview": {
                "background": {
                    "enabled": true,
                    "args": ["--data-plane-host=127.0.0.1:23635", "--invert-colors=never"],
                    "partialRendering": true,
                },
            },
            "plugins": ["user-plugin"],
            "adapterOnly": [1, 2],
            "userOnly": ["user"],
        })),
    );
}

#[gpui::test]
async fn test_other_adapters_lsp_configuration_contributions_are_unioned(cx: &mut TestAppContext) {
    init_test(cx);

    let user_settings = serde_json::json!({
        "lsp": {
            "the-fake-language-server": {
                "initialization_options": {
                    "languages": ["user-lang"],
                    "userOnly": true,
                },
            },
        },
    });

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/the-root"),
        json!({
            ".zed": {
                "settings.json": user_settings.to_string(),
            },
            "main.rs": "fn main() {}",
        }),
    )
    .await;

    let project = Project::test(fs, [path!("/the-root").as_ref()], cx).await;
    let language_registry = project.read_with(cx, |project, _| project.languages().clone());
    language_registry.add(rust_lang());

    let main_server_name = LanguageServerName("the-fake-language-server".into());
    for (language, server_name, plugin, lang, memory) in [
        ("Vue", "vue-language-server", "vue-plugin", "vue", 4096),
        (
            "Astro",
            "astro-language-server",
            "astro-plugin",
            "astro",
            2048,
        ),
    ] {
        let contribution = json!({
            "tsserver": {
                "globalPlugins": ["shared-plugin", plugin],
                "maxMemory": memory,
            },
            "languages": [lang],
        });
        language_registry.register_fake_lsp_adapter(
            language,
            FakeLspAdapter {
                name: server_name,
                additional_initialization_options: HashMap::from_iter([(
                    main_server_name.clone(),
                    contribution.clone(),
                )]),
                additional_workspace_configuration: HashMap::from_iter([(
                    main_server_name.clone(),
                    contribution,
                )]),
                ..FakeLspAdapter::default()
            },
        );
    }

    let sent_initialization_options = Arc::new(Mutex::new(None));
    let mut fake_servers = language_registry.register_fake_lsp(
        "Rust",
        FakeLspAdapter {
            name: "the-fake-language-server",
            initialization_options: Some(json!({
                "tsserver": {
                    "globalPlugins": ["default-plugin"],
                },
                "languages": ["default-lang"],
            })),
            initializer: Some(Box::new({
                let sent_initialization_options = sent_initialization_options.clone();
                move |fake_server| {
                    let sent_initialization_options = sent_initialization_options.clone();
                    fake_server.set_request_handler::<lsp::request::Initialize, _, _>(
                        move |params, _| {
                            *sent_initialization_options.lock() = params.initialization_options;
                            async move { Ok(lsp::InitializeResult::default()) }
                        },
                    );
                }
            })),
            ..FakeLspAdapter::default()
        },
    );
    cx.run_until_parked();

    project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/the-root/main.rs"), cx)
        })
        .await
        .unwrap();
    let mut fake_server = fake_servers.next().await.unwrap();
    let workspace_configuration = fake_server
        .receive_notification::<lsp::notification::DidChangeConfiguration>()
        .await
        .settings;
    cx.run_until_parked();

    assert_eq!(
        sent_initialization_options.lock().take(),
        Some(json!({
            "tsserver": {
                "globalPlugins": ["default-plugin", "shared-plugin", "astro-plugin", "vue-plugin"],
                "maxMemory": 4096,
            },
            "languages": ["user-lang"],
            "userOnly": true,
        })),
    );
    assert_eq!(
        workspace_configuration,
        json!({
            "tsserver": {
                "globalPlugins": ["shared-plugin", "astro-plugin", "vue-plugin"],
                "maxMemory": 4096,
            },
            "languages": ["astro", "vue"],
        }),
    );
}

#[gpui::test]
async fn test_initialization_options_contributions_without_own_options(cx: &mut TestAppContext) {
    init_test(cx);

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/the-root"), json!({ "main.rs": "fn main() {}" }))
        .await;

    let project = Project::test(fs, [path!("/the-root").as_ref()], cx).await;
    let language_registry = project.read_with(cx, |project, _| project.languages().clone());
    language_registry.add(rust_lang());

    let contribution = json!({
        "tsserver": {
            "globalPlugins": ["vue-plugin"],
        },
    });
    language_registry.register_fake_lsp_adapter(
        "Vue",
        FakeLspAdapter {
            name: "vue-language-server",
            additional_initialization_options: HashMap::from_iter([(
                LanguageServerName("the-fake-language-server".into()),
                contribution.clone(),
            )]),
            ..FakeLspAdapter::default()
        },
    );

    let sent_initialization_options = Arc::new(Mutex::new(None));
    let mut fake_servers = language_registry.register_fake_lsp(
        "Rust",
        FakeLspAdapter {
            name: "the-fake-language-server",
            initialization_options: None,
            initializer: Some(Box::new({
                let sent_initialization_options = sent_initialization_options.clone();
                move |fake_server| {
                    let sent_initialization_options = sent_initialization_options.clone();
                    fake_server.set_request_handler::<lsp::request::Initialize, _, _>(
                        move |params, _| {
                            *sent_initialization_options.lock() =
                                Some(params.initialization_options);
                            async move { Ok(lsp::InitializeResult::default()) }
                        },
                    );
                }
            })),
            ..FakeLspAdapter::default()
        },
    );
    cx.run_until_parked();

    project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/the-root/main.rs"), cx)
        })
        .await
        .unwrap();
    fake_servers.next().await.unwrap();
    cx.run_until_parked();

    assert_eq!(
        sent_initialization_options.lock().take(),
        Some(Some(contribution)),
    );
}

async fn project_with_rust_server(
    fs: Arc<FakeFs>,
    root: &str,
    first_file: &str,
    cx: &mut TestAppContext,
) -> (Entity<Project>, LanguageServerId) {
    let project = Project::test(fs, [root.as_ref()], cx).await;
    let language_registry = project.read_with(cx, |project, _| project.languages().clone());
    language_registry.add(rust_lang());
    let mut fake_servers = language_registry.register_fake_lsp("Rust", FakeLspAdapter::default());

    project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(first_file, cx)
        })
        .await
        .unwrap();
    fake_servers.next().await.unwrap();
    cx.run_until_parked();

    let server_id = project.read_with(cx, |project, cx| {
        project
            .lsp_store()
            .read(cx)
            .language_server_statuses()
            .next()
            .unwrap()
            .0
    });
    (project, server_id)
}

fn buffer_paths(buffer: &Entity<Buffer>, cx: &TestAppContext) -> (String, PathBuf) {
    buffer.read_with(cx, |buffer, cx| {
        let file = File::from_dyn(buffer.file()).unwrap();
        (file.path.as_unix_str().to_string(), file.abs_path(cx))
    })
}

fn worktree_roots(project: &Entity<Project>, cx: &TestAppContext) -> Vec<PathBuf> {
    project.read_with(cx, |project, cx| {
        project
            .worktrees(cx)
            .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
            .collect()
    })
}

fn worktree_entries(project: &Entity<Project>, cx: &TestAppContext) -> Vec<String> {
    project.read_with(cx, |project, cx| {
        let worktree = project.worktrees(cx).next().unwrap();
        worktree
            .read(cx)
            .snapshot()
            .entries(true, 0)
            .map(|entry| entry.path.as_unix_str().to_string())
            .collect()
    })
}

fn document_uri(abs_path: &str) -> Uri {
    Uri::from_file_path(abs_path).expect("an absolute path")
}

/// The documents `server` was sent didOpen for since the last call, with their
/// text as sent.
fn opened_documents(server: &mut lsp::FakeLanguageServer) -> Vec<(Uri, String)> {
    let mut opened = Vec::new();
    while let Some(Some(params)) = server
        .try_receive_notification::<lsp::notification::DidOpenTextDocument>()
        .now_or_never()
    {
        // Windows sends the buffer's line endings.
        opened.push((
            params.text_document.uri,
            params.text_document.text.replace("\r\n", "\n"),
        ));
    }
    opened
}

// A restart reopens, in the new server, only the buffers something still
// holds: a buffer opened with no one holding it has no refcount to close it
// with, so it would stay open in the new server for good.
#[gpui::test]
async fn test_restart_reopens_only_the_buffers_still_held(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/dir"),
        json!({
            "held.rs": "fn held() {}",
            "scrolled_away.rs": "fn scrolled_away() {}",
            "never_shown.rs": "fn never_shown() {}",
        }),
    )
    .await;
    let project = Project::test(fs, [path!("/dir").as_ref()], cx).await;
    let language_registry = project.read_with(cx, |project, _| project.languages().clone());
    language_registry.add(rust_lang());
    let mut fake_servers = language_registry.register_fake_lsp("Rust", FakeLspAdapter::default());
    let lsp_store = project.read_with(cx, |project, _| project.lsp_store());

    let (held, held_handle) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/dir/held.rs"), cx)
        })
        .await
        .expect("opening held.rs");
    let (scrolled_away, scrolled_away_handle) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/dir/scrolled_away.rs"), cx)
        })
        .await
        .expect("opening scrolled_away.rs");
    // Like a project search result no editor has shown.
    let never_shown = project
        .update(cx, |project, cx| {
            project.open_local_buffer(path!("/dir/never_shown.rs"), cx)
        })
        .await
        .expect("opening never_shown.rs");
    let mut first = fake_servers.next().await.expect("the server starts");
    cx.run_until_parked();
    assert_eq!(
        opened_documents(&mut first)
            .into_iter()
            .map(|(uri, _)| uri)
            .collect::<HashSet<_>>(),
        HashSet::from_iter([
            document_uri(path!("/dir/held.rs")),
            document_uri(path!("/dir/scrolled_away.rs")),
        ])
    );

    held.update(cx, |buffer, cx| {
        buffer.edit([(0..0, "// unsaved\n")], None, cx)
    });
    // Like an editor with scrolled_away.rs in an excerpt it no longer shows:
    // it lets go of the buffer when its language server goes away.
    let scrolled_away_handle = Rc::new(RefCell::new(Some(scrolled_away_handle)));
    let _subscription = cx.update(|cx| {
        let scrolled_away_handle = scrolled_away_handle.clone();
        cx.subscribe(&lsp_store, move |_, event, _| {
            if let LspStoreEvent::LanguageServerRemoved(_) = event {
                scrolled_away_handle.borrow_mut().take();
            }
        })
    });
    // Clones: the buffers stay open in the project, as a search result does,
    // rather than being dropped (and so closed) by the restart itself.
    project.update(cx, |project, cx| {
        project.restart_language_servers_for_buffers(
            vec![never_shown.clone(), scrolled_away.clone(), held.clone()],
            HashSet::default(),
            true,
            cx,
        );
    });
    cx.run_until_parked();
    let mut second = fake_servers.next().await.expect("the server restarts");
    cx.run_until_parked();
    assert!(
        scrolled_away_handle.borrow().is_none(),
        "the old server was removed"
    );
    assert_eq!(
        opened_documents(&mut second),
        [(
            document_uri(path!("/dir/held.rs")),
            "// unsaved\nfn held() {}".to_string()
        )],
        "the new server gets the unsaved text of the buffer still held, and nothing no one holds"
    );

    // The buffer reopened is still counted: letting go of it closes it.
    cx.update(|_| drop(held_handle));
    cx.run_until_parked();
    assert_eq!(
        second
            .receive_notification::<lsp::notification::DidCloseTextDocument>()
            .await
            .text_document
            .uri,
        document_uri(path!("/dir/held.rs"))
    );
}

// A file outside the project uses the server of the project's worktree. A
// restart that registered it first would start a server of its own for it.
#[gpui::test]
async fn test_restart_reopens_project_files_before_files_outside_it(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/dir"), json!({ "main.rs": "fn main() {}" }))
        .await;
    fs.insert_tree(path!("/outside"), json!({ "header.rs": "fn outside() {}" }))
        .await;
    let project = Project::test(fs, [path!("/dir").as_ref()], cx).await;
    let language_registry = project.read_with(cx, |project, _| project.languages().clone());
    language_registry.add(rust_lang());
    let mut fake_servers = language_registry.register_fake_lsp("Rust", FakeLspAdapter::default());

    let (main, _main_handle) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/dir/main.rs"), cx)
        })
        .await
        .expect("opening main.rs");
    let outside = project
        .update(cx, |project, cx| {
            project.open_local_buffer(path!("/outside/header.rs"), cx)
        })
        .await
        .expect("opening header.rs");
    let _outside_handle = project.update(cx, |project, cx| {
        project.register_buffer_with_language_servers(&outside, cx)
    });
    let mut first = fake_servers.next().await.expect("the server starts");
    cx.run_until_parked();
    assert_eq!(opened_documents(&mut first).len(), 2);
    assert!(
        fake_servers.next().now_or_never().is_none(),
        "the file outside the project uses the project's server"
    );

    project.update(cx, |project, cx| {
        project.restart_language_servers_for_buffers(
            vec![outside.clone(), main.clone()],
            HashSet::default(),
            true,
            cx,
        );
    });
    cx.run_until_parked();
    let mut second = fake_servers.next().await.expect("the server restarts");
    cx.run_until_parked();
    assert!(
        fake_servers.next().now_or_never().is_none(),
        "the restart starts one server, not one more for the file outside the project"
    );
    assert_eq!(
        opened_documents(&mut second)
            .into_iter()
            .map(|(uri, _)| uri)
            .collect::<HashSet<_>>(),
        HashSet::from_iter([
            document_uri(path!("/dir/main.rs")),
            document_uri(path!("/outside/header.rs")),
        ])
    );
}

// What a restart does to the diagnostics of a file no one holds: the stopped
// server's go with it, and a new server that is not sent the file describes
// it only if it does so on its own (clangd and rust-analyzer do, from their
// index or check; a server that diagnoses only opened documents does not).
#[gpui::test]
async fn test_restart_leaves_a_file_no_one_holds_to_the_new_server(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/dir"),
        json!({ "held.rs": "fn held() {}", "unheld.rs": "fn unheld() {}" }),
    )
    .await;
    let project = Project::test(fs, [path!("/dir").as_ref()], cx).await;
    let language_registry = project.read_with(cx, |project, _| project.languages().clone());
    language_registry.add(rust_lang());
    let mut fake_servers = language_registry.register_fake_lsp("Rust", FakeLspAdapter::default());

    let (held, _held_handle) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/dir/held.rs"), cx)
        })
        .await
        .expect("opening held.rs");
    let unheld = project
        .update(cx, |project, cx| {
            project.open_local_buffer(path!("/dir/unheld.rs"), cx)
        })
        .await
        .expect("opening unheld.rs");
    let first = fake_servers.next().await.expect("the server starts");
    cx.run_until_parked();
    let publish = |server: &lsp::FakeLanguageServer, abs_path: &str| {
        server.notify::<lsp::notification::PublishDiagnostics>(lsp::PublishDiagnosticsParams {
            uri: document_uri(abs_path),
            version: None,
            diagnostics: vec![lsp::Diagnostic {
                range: lsp::Range::new(lsp::Position::new(0, 0), lsp::Position::new(0, 2)),
                severity: Some(lsp::DiagnosticSeverity::ERROR),
                message: lsp::DiagnosticMessage::String("an error".to_string()),
                ..Default::default()
            }],
        });
    };
    let errors_in = |abs_path: &str, cx: &mut TestAppContext| {
        project.read_with(cx, |project, cx| {
            project
                .diagnostic_summaries(false, cx)
                .filter(|(project_path, _, _)| {
                    project
                        .absolute_path(project_path, cx)
                        .is_some_and(|path| path == Path::new(abs_path))
                })
                .map(|(_, _, summary)| summary.error_count)
                .sum::<usize>()
        })
    };
    publish(&first, path!("/dir/held.rs"));
    publish(&first, path!("/dir/unheld.rs"));
    cx.run_until_parked();
    assert_eq!(errors_in(path!("/dir/held.rs"), cx), 1);
    assert_eq!(errors_in(path!("/dir/unheld.rs"), cx), 1);

    project.update(cx, |project, cx| {
        project.restart_language_servers_for_buffers(
            vec![unheld.clone(), held.clone()],
            HashSet::default(),
            true,
            cx,
        );
    });
    cx.run_until_parked();
    let mut second = fake_servers.next().await.expect("the server restarts");
    cx.run_until_parked();
    assert_eq!(
        opened_documents(&mut second)
            .into_iter()
            .map(|(uri, _)| uri)
            .collect::<Vec<_>>(),
        [document_uri(path!("/dir/held.rs"))]
    );
    assert_eq!(
        errors_in(path!("/dir/unheld.rs"), cx),
        0,
        "the stopped server's diagnostics go with it"
    );

    publish(&second, path!("/dir/unheld.rs"));
    cx.run_until_parked();
    assert_eq!(
        errors_in(path!("/dir/unheld.rs"), cx),
        1,
        "a server that describes files it was not sent still shows them"
    );
}

// Restarting one server by name reopens, in it, only the buffers still held,
// and opens nothing in the servers left running.
#[gpui::test]
async fn test_restarting_one_server_reopens_only_the_buffers_still_held(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/dir"),
        json!({
            "held.rs": "fn held() {}",
            "scrolled_away.rs": "fn scrolled_away() {}",
            "never_shown.rs": "fn never_shown() {}",
        }),
    )
    .await;
    let project = Project::test(fs, [path!("/dir").as_ref()], cx).await;
    let language_registry = project.read_with(cx, |project, _| project.languages().clone());
    language_registry.add(rust_lang());
    let mut restarted_servers = language_registry.register_fake_lsp(
        "Rust",
        FakeLspAdapter {
            name: "restarted-server",
            ..FakeLspAdapter::default()
        },
    );
    let mut other_servers = language_registry.register_fake_lsp(
        "Rust",
        FakeLspAdapter {
            name: "other-server",
            ..FakeLspAdapter::default()
        },
    );
    let lsp_store = project.read_with(cx, |project, _| project.lsp_store());

    let (held, _held_handle) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/dir/held.rs"), cx)
        })
        .await
        .expect("opening held.rs");
    let (scrolled_away, scrolled_away_handle) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/dir/scrolled_away.rs"), cx)
        })
        .await
        .expect("opening scrolled_away.rs");
    let never_shown = project
        .update(cx, |project, cx| {
            project.open_local_buffer(path!("/dir/never_shown.rs"), cx)
        })
        .await
        .expect("opening never_shown.rs");
    let mut first = restarted_servers.next().await.expect("the server starts");
    let mut other = other_servers.next().await.expect("the other server starts");
    cx.run_until_parked();
    assert_eq!(opened_documents(&mut first).len(), 2);
    assert_eq!(opened_documents(&mut other).len(), 2);

    let scrolled_away_handle = Rc::new(RefCell::new(Some(scrolled_away_handle)));
    let _subscription = cx.update(|cx| {
        let scrolled_away_handle = scrolled_away_handle.clone();
        cx.subscribe(&lsp_store, move |_, event, _| {
            if let LspStoreEvent::LanguageServerRemoved(_) = event {
                scrolled_away_handle.borrow_mut().take();
            }
        })
    });
    project.update(cx, |project, cx| {
        project.restart_language_servers_for_buffers(
            vec![never_shown.clone(), scrolled_away.clone(), held.clone()],
            HashSet::from_iter([lsp::LanguageServerSelector::Name(LanguageServerName(
                "restarted-server".into(),
            ))]),
            true,
            cx,
        );
    });
    cx.run_until_parked();
    let mut second = restarted_servers.next().await.expect("the server restarts");
    cx.run_until_parked();
    assert!(
        scrolled_away_handle.borrow().is_none(),
        "the old server was removed"
    );
    assert_eq!(
        opened_documents(&mut second)
            .into_iter()
            .map(|(uri, _)| uri)
            .collect::<Vec<_>>(),
        [document_uri(path!("/dir/held.rs"))]
    );
    assert!(
        other_servers.next().now_or_never().is_none(),
        "the other server keeps running"
    );
    assert_eq!(
        opened_documents(&mut other),
        Vec::new(),
        "nothing is reopened in the server left running"
    );
}

// A restart starts servers for the files open in them. With none open there
// is nothing to start them for, so they stay stopped until a file is opened,
// and the user is told so rather than left to wonder.
#[gpui::test]
async fn test_restart_with_no_file_open_says_the_servers_start_with_a_file(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/dir"), json!({ "main.rs": "fn main() {}" }))
        .await;
    let project = Project::test(fs, [path!("/dir").as_ref()], cx).await;
    let language_registry = project.read_with(cx, |project, _| project.languages().clone());
    language_registry.add(rust_lang());
    let mut fake_servers = language_registry.register_fake_lsp("Rust", FakeLspAdapter::default());
    let lsp_store = project.read_with(cx, |project, _| project.lsp_store());
    let notifications = Rc::new(RefCell::new(Vec::new()));
    let _subscription = cx.update(|cx| {
        let notifications = notifications.clone();
        cx.subscribe(&lsp_store, move |_, event, _| {
            if let LspStoreEvent::Notification(message) = event {
                notifications.borrow_mut().push(message.clone());
            }
        })
    });

    let (_main, main_handle) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/dir/main.rs"), cx)
        })
        .await
        .expect("opening main.rs");
    let _first = fake_servers.next().await.expect("the server starts");
    cx.run_until_parked();

    lsp_store.update(cx, |lsp_store, cx| {
        lsp_store.restart_all_language_servers(cx)
    });
    cx.run_until_parked();
    let _second = fake_servers
        .next()
        .now_or_never()
        .flatten()
        .expect("the server restarts for the file open in it");
    assert_eq!(*notifications.borrow(), Vec::<String>::new());

    cx.update(|_| drop(main_handle));
    cx.run_until_parked();
    // As a change of the semantic tokens setting restarts them.
    let buffers = lsp_store.read_with(cx, |lsp_store, cx| {
        lsp_store
            .buffer_store()
            .read(cx)
            .buffers()
            .collect::<Vec<_>>()
    });
    lsp_store.update(cx, |lsp_store, cx| {
        lsp_store.restart_language_servers_for_buffers(buffers, HashSet::default(), false, cx)
    });
    cx.run_until_parked();
    assert_eq!(
        *notifications.borrow(),
        Vec::<String>::new(),
        "a restart the user did not ask for says nothing"
    );
    let (_main, main_handle) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/dir/main.rs"), cx)
        })
        .await
        .expect("opening main.rs");
    cx.run_until_parked();
    assert!(
        fake_servers.next().now_or_never().flatten().is_some(),
        "the server starts again"
    );
    cx.update(|_| drop(main_handle));
    cx.run_until_parked();

    lsp_store.update(cx, |lsp_store, cx| {
        lsp_store.restart_all_language_servers(cx)
    });
    cx.run_until_parked();
    assert!(
        fake_servers.next().now_or_never().is_none(),
        "no server starts with no file open"
    );
    assert_eq!(
        *notifications.borrow(),
        [
            "No open file uses the restarted language servers, so they will start when you open a file."
        ]
    );

    let (_main, _main_handle) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/dir/main.rs"), cx)
        })
        .await
        .expect("opening main.rs again");
    cx.run_until_parked();
    assert!(
        fake_servers.next().now_or_never().flatten().is_some(),
        "opening a file starts the server"
    );
    lsp_store.update(cx, |lsp_store, cx| {
        lsp_store.restart_all_language_servers(cx)
    });
    cx.run_until_parked();
    assert_eq!(
        notifications.borrow().len(),
        1,
        "nothing to say when a file is open"
    );
}
