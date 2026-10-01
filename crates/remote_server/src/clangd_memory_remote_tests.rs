//! clangd's memory on a remote project: measured and restarted on the host,
//! reported to the client.

use std::{
    cell::RefCell,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use fs::FakeFs;
use gpui::{AppContext as _, TestAppContext, UpdateGlobal as _};
use language::{FakeLspAdapter, LanguageConfig, LanguageMatcher};
use lsp::LanguageServerName;
use project::{
    lsp_store::clangd_memory::{
        ClangdMemoryBreakdown, ClangdMemoryEvent, ClangdMemoryEvents, ClangdMemoryReport,
        MEMORY_CHECK_INTERVAL, MemoryUsage,
    },
    project_launch_approvals::ProjectLaunchApprovals,
};
use serde_json::json;
use settings::SettingsStore;
use smol::stream::StreamExt as _;
use util::{path, rel_path::rel_path};

use crate::remote_editing_tests::init_test;

const MEGABYTE: u64 = 1024 * 1024;

#[gpui::test]
async fn test_remote_clangd_memory_is_measured_on_the_host(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    cx.update(ProjectLaunchApprovals::approve_everything_for_tests);
    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        path!("/code"),
        json!({
            "project1": {
                ".zed": {
                    // A repository cannot set the limit (or restart clangd every 30 seconds).
                    "settings.json": r#"{ "clangd": { "memory_limit_mb": 1 } }"#
                },
                "a.c": "int main(void) { return 0; }"
            },
        }),
    )
    .await;
    let (project, headless) = init_test(&fs, cx, server_cx).await;
    // The client's user settings reach the host.
    cx.update(|cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |settings| {
                settings
                    .project
                    .clangd
                    .get_or_insert_default()
                    .memory_limit_mb = Some(100);
            });
        });
    });
    cx.run_until_parked();
    server_cx.run_until_parked();

    cx.update_entity(&project, |project, _| {
        project.languages().register_test_language(LanguageConfig {
            name: "C".into(),
            matcher: (LanguageMatcher {
                path_suffixes: vec!["c".into()],
                ..Default::default()
            })
            .into(),
            ..Default::default()
        });
        project.languages().register_fake_lsp_adapter(
            "C",
            FakeLspAdapter {
                name: "clangd",
                ..FakeLspAdapter::default()
            },
        );
    });
    let mut fake_clangd = server_cx.update(|cx| {
        headless.read(cx).languages.register_fake_lsp_server(
            LanguageServerName("clangd".into()),
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
        .expect("adding the worktree")
        .0
        .read_with(cx, |worktree, _| worktree.id());
    cx.run_until_parked();

    let (_buffer, _handle) = project
        .update(cx, |project, cx| {
            project.open_buffer_with_lsp((worktree_id, rel_path("a.c")), cx)
        })
        .await
        .expect("opening a.c");
    cx.run_until_parked();
    let first = fake_clangd.next().await.expect("clangd starts on the host");
    let first_id = first.server.server_id();
    first.set_request_handler::<MemoryUsage, _, _>(|_, _| async move {
        Ok(json!({
            "_self": 0,
            "_total": 300,
            "clangd_server": {
                "_self": 0,
                "_total": 300,
                "background_index": { "_self": 200, "_total": 200 },
                "tuscheduler": { "_self": 100, "_total": 100 }
            }
        }))
    });
    let resident = Arc::new(AtomicU64::new(50 * MEGABYTE));
    server_cx.update(|cx| {
        let resident = resident.clone();
        let lsp_store = headless.read(cx).lsp_store.clone();
        lsp_store.update(cx, |lsp_store, _| {
            lsp_store.set_clangd_resident_memory_for_tests(Arc::new(move |_| {
                Some(resident.load(Ordering::SeqCst))
            }));
        });
    });

    let client_lsp_store = project.read_with(cx, |project, _| project.lsp_store());
    let report = client_lsp_store
        .update(cx, |lsp_store, cx| {
            lsp_store.clangd_memory_report(first_id, cx)
        })
        .await
        .expect("the host reports clangd's memory");
    assert_eq!(
        report,
        ClangdMemoryReport {
            resident_bytes: Some(50 * MEGABYTE),
            breakdown: Some(ClangdMemoryBreakdown {
                total_bytes: 300,
                background_index_bytes: 200,
                dynamic_index_bytes: 0,
                open_files_bytes: 100,
            }),
        }
    );

    let events = Rc::new(RefCell::new(Vec::new()));
    cx.update(|cx| {
        let recorded = events.clone();
        let emitter = ClangdMemoryEvents::global(cx);
        cx.subscribe(&emitter, move |_, event: &ClangdMemoryEvent, _| {
            recorded.borrow_mut().push(event.clone());
        })
        .detach();
    });

    resident.store(200 * MEGABYTE, Ordering::SeqCst);
    server_cx.executor().advance_clock(MEMORY_CHECK_INTERVAL);
    server_cx.run_until_parked();
    cx.run_until_parked();
    let second = fake_clangd
        .next()
        .await
        .expect("the host restarts clangd over its limit");
    assert_ne!(second.server.server_id(), first_id);
    assert_eq!(
        *events.borrow(),
        [ClangdMemoryEvent::LimitExceeded {
            lsp_store: client_lsp_store.entity_id(),
            server_id: first_id,
            resident_bytes: 200 * MEGABYTE,
            limit_bytes: 100 * MEGABYTE,
            restarted: true,
        }],
        "the client hears of it as its own store's"
    );
}
