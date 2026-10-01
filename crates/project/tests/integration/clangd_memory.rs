use std::{cell::RefCell, rc::Rc, sync::Arc};

use fs::FakeFs;
use futures::{FutureExt as _, StreamExt as _};
use gpui::{Entity, TestAppContext, UpdateGlobal as _};
use language::{FakeLspAdapter, Language, LanguageConfig, LanguageMatcher};
use lsp::{FakeLanguageServer, LanguageServerId};
use project::{
    Project,
    lsp_store::{
        LspStore, LspStoreEvent,
        clangd_memory::{
            ClangdMemoryBreakdown, ClangdMemoryEvent, ClangdMemoryEvents, ClangdMemoryReport,
            MEMORY_CHECK_INTERVAL, MEMORY_REPORT_TIMEOUT, MIN_RESTART_INTERVAL, MemoryUsage,
        },
    },
};
use serde_json::json;
use settings::{ClangdIndexMode, ClangdSettingsContent, SettingsStore};
use util::path;

use crate::init_test;

const MEGABYTE: u64 = 1024 * 1024;

fn c_lang() -> Arc<Language> {
    Arc::new(Language::new(
        LanguageConfig {
            name: "C".into(),
            matcher: LanguageMatcher {
                path_suffixes: vec!["c".to_string()],
                ..Default::default()
            }
            .into(),
            ..Default::default()
        },
        None,
    ))
}

fn set_clangd_settings(settings: ClangdSettingsContent, cx: &mut TestAppContext) {
    cx.update(|cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |content| {
                content.project.clangd = Some(settings);
            });
        });
    });
}

struct OpenedC {
    project: Entity<Project>,
    servers: futures::channel::mpsc::UnboundedReceiver<FakeLanguageServer>,
    server: FakeLanguageServer,
    /// The file names each clangd was told were opened, in order.
    did_open: Arc<parking_lot::Mutex<Vec<(LanguageServerId, String)>>>,
    /// The text each clangd was given with each file, in order.
    did_open_texts: Arc<parking_lot::Mutex<Vec<(LanguageServerId, String, String)>>>,
    buffer: Entity<language::Buffer>,
    _handle: project::lsp_store::OpenLspBufferHandle,
}

/// Opens `a.c` with a fake clangd.
async fn open_c_file(cx: &mut TestAppContext) -> OpenedC {
    open_c_file_in(
        json!({ "a.c": "int main(void) { return 0; }", "b.c": "void b(void) {}" }),
        cx,
    )
    .await
}

/// Opens `a.c` of `tree` with a fake clangd.
async fn open_c_file_in(tree: serde_json::Value, cx: &mut TestAppContext) -> OpenedC {
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/root"), tree).await;
    let project = Project::test(fs, [path!("/root").as_ref()], cx).await;
    let language_registry = project.read_with(cx, |project, _| project.languages().clone());
    language_registry.add(c_lang());
    let did_open = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let did_open_texts = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let mut fake_servers = language_registry.register_fake_lsp(
        "C",
        FakeLspAdapter {
            name: "clangd",
            initializer: Some(Box::new({
                let did_open = did_open.clone();
                let did_open_texts = did_open_texts.clone();
                move |server| {
                    let did_open = did_open.clone();
                    let did_open_texts = did_open_texts.clone();
                    let server_id = server.server.server_id();
                    server.handle_notification::<lsp::notification::DidOpenTextDocument, _>(
                        move |params, _| {
                            let uri = params.text_document.uri.to_string();
                            let name = uri.rsplit('/').next().unwrap_or_default().to_string();
                            did_open.lock().push((server_id, name.clone()));
                            did_open_texts.lock().push((
                                server_id,
                                name,
                                params.text_document.text,
                            ));
                        },
                    );
                }
            })),
            ..Default::default()
        },
    );
    let (buffer, handle) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/root/a.c"), cx)
        })
        .await
        .expect("opening a.c");
    let server = fake_servers.next().await.expect("clangd starts");
    cx.run_until_parked();
    OpenedC {
        project,
        servers: fake_servers,
        server,
        did_open,
        did_open_texts,
        buffer,
        _handle: handle,
    }
}

fn lsp_store(project: &Entity<Project>, cx: &mut TestAppContext) -> Entity<LspStore> {
    project.read_with(cx, |project, _| project.lsp_store())
}

fn arguments(server: &FakeLanguageServer) -> Vec<String> {
    server
        .binary
        .arguments
        .iter()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect()
}

#[gpui::test]
async fn test_clangd_starts_with_upstream_arguments_by_default(cx: &mut TestAppContext) {
    init_test(cx);
    let OpenedC { server, .. } = open_c_file(cx).await;
    assert_eq!(arguments(&server), Vec::<String>::new());
}

#[gpui::test]
async fn test_clangd_light_mode_and_workers_reach_the_command_line(cx: &mut TestAppContext) {
    init_test(cx);
    set_clangd_settings(
        ClangdSettingsContent {
            index_mode: Some(ClangdIndexMode::Light),
            max_workers: Some(2),
            memory_limit_mb: None,
        },
        cx,
    );
    let OpenedC { server, .. } = open_c_file(cx).await;
    assert_eq!(arguments(&server), ["--background-index=false", "-j=2"]);
}

#[gpui::test]
async fn test_clangd_arguments_of_the_user_win(cx: &mut TestAppContext) {
    init_test(cx);
    set_clangd_settings(
        ClangdSettingsContent {
            index_mode: Some(ClangdIndexMode::Light),
            max_workers: Some(2),
            memory_limit_mb: None,
        },
        cx,
    );
    cx.update(|cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |content| {
                content.project.lsp.0.insert(
                    "clangd".into(),
                    serde_json::from_value(json!({
                        "binary": {
                            "path": path!("/usr/bin/clangd"),
                            "arguments": ["--background-index", "-j=8", "--log=error"]
                        }
                    }))
                    .expect("valid language server settings"),
                );
            });
        });
    });
    let OpenedC { server, .. } = open_c_file(cx).await;
    assert_eq!(
        arguments(&server),
        ["--background-index", "-j=8", "--log=error"]
    );
}

#[gpui::test]
async fn test_clangd_restarts_when_its_command_line_settings_change(cx: &mut TestAppContext) {
    init_test(cx);
    let OpenedC {
        project: _project,
        mut servers,
        server: first,
        _handle,
        ..
    } = open_c_file(cx).await;

    // The limit is not on the command line.
    set_clangd_settings(
        ClangdSettingsContent {
            memory_limit_mb: Some(4096),
            ..Default::default()
        },
        cx,
    );
    cx.run_until_parked();
    assert!(
        servers.next().now_or_never().is_none(),
        "a new limit alone must not restart clangd"
    );

    set_clangd_settings(
        ClangdSettingsContent {
            index_mode: Some(ClangdIndexMode::Light),
            memory_limit_mb: Some(4096),
            ..Default::default()
        },
        cx,
    );
    cx.run_until_parked();
    let second = servers
        .next()
        .now_or_never()
        .flatten()
        .expect("clangd restarts in light mode");
    assert_ne!(second.server.server_id(), first.server.server_id());
    assert_eq!(arguments(&second), ["--background-index=false"]);
    cx.run_until_parked();
    assert!(
        servers.next().now_or_never().is_none(),
        "one change restarts clangd once"
    );
}

fn record_events(cx: &mut TestAppContext) -> Rc<RefCell<Vec<ClangdMemoryEvent>>> {
    let events = Rc::new(RefCell::new(Vec::new()));
    cx.update(|cx| {
        let recorded = events.clone();
        let emitter = ClangdMemoryEvents::global(cx);
        cx.subscribe(&emitter, move |_, event: &ClangdMemoryEvent, _| {
            recorded.borrow_mut().push(event.clone());
        })
        .detach();
    });
    events
}

fn restarted_flags(events: &Rc<RefCell<Vec<ClangdMemoryEvent>>>) -> Vec<(LanguageServerId, bool)> {
    events
        .borrow()
        .iter()
        .map(|event| match event {
            ClangdMemoryEvent::LimitExceeded {
                server_id,
                restarted,
                ..
            } => (*server_id, *restarted),
        })
        .collect()
}

#[gpui::test]
async fn test_clangd_over_its_memory_limit_restarts_at_most_every_ten_minutes(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    set_clangd_settings(
        ClangdSettingsContent {
            memory_limit_mb: Some(100),
            ..Default::default()
        },
        cx,
    );
    let OpenedC {
        project,
        mut servers,
        server: first,
        did_open,
        _handle,
        ..
    } = open_c_file(cx).await;
    let lsp_store = lsp_store(&project, cx);
    // Loaded, but open in no editor.
    let _unopened = project
        .update(cx, |project, cx| {
            project.open_local_buffer(path!("/root/b.c"), cx)
        })
        .await
        .expect("loading b.c");
    let resident = Arc::new(parking_lot::Mutex::new(50 * MEGABYTE));
    lsp_store.update(cx, |lsp_store, _| {
        let resident = resident.clone();
        lsp_store.set_clangd_resident_memory_for_tests(Arc::new(move |_| Some(*resident.lock())));
    });
    let events = record_events(cx);

    cx.executor().advance_clock(MEMORY_CHECK_INTERVAL);
    cx.run_until_parked();
    assert!(events.borrow().is_empty(), "under the limit");
    assert!(servers.next().now_or_never().is_none());

    *resident.lock() = 200 * MEGABYTE;
    cx.executor().advance_clock(MEMORY_CHECK_INTERVAL);
    cx.run_until_parked();
    let second = servers
        .next()
        .now_or_never()
        .flatten()
        .expect("clangd is restarted over its limit");
    let first_id = first.server.server_id();
    let second_id = second.server.server_id();
    assert_ne!(second_id, first_id);
    assert_eq!(restarted_flags(&events), [(first_id, true)]);
    assert_eq!(
        *did_open.lock(),
        [
            (first_id, "a.c".to_string()),
            (second_id, "a.c".to_string())
        ],
        "the new clangd gets the open file, and not the one open in no editor"
    );
    assert_eq!(
        events.borrow().first(),
        Some(&ClangdMemoryEvent::LimitExceeded {
            lsp_store: lsp_store.entity_id(),
            server_id: first_id,
            resident_bytes: 200 * MEGABYTE,
            limit_bytes: 100 * MEGABYTE,
            restarted: true,
        })
    );

    // Back over the limit right away: told once, not restarted.
    cx.executor().advance_clock(MEMORY_CHECK_INTERVAL);
    cx.run_until_parked();
    cx.executor().advance_clock(MEMORY_CHECK_INTERVAL);
    cx.run_until_parked();
    assert!(servers.next().now_or_never().is_none());
    assert_eq!(
        restarted_flags(&events),
        [(first_id, true), (second_id, false)]
    );

    // The clock moves through every check on the way; the restart was two
    // checks ago, so this stops one check short of ten minutes after it.
    cx.executor()
        .advance_clock(MIN_RESTART_INTERVAL - MEMORY_CHECK_INTERVAL * 3);
    cx.run_until_parked();
    assert!(servers.next().now_or_never().is_none());
    assert_eq!(events.borrow().len(), 2);

    cx.executor().advance_clock(MEMORY_CHECK_INTERVAL);
    cx.run_until_parked();
    let third = servers
        .next()
        .now_or_never()
        .flatten()
        .expect("restarted again once ten minutes have passed");
    assert_ne!(third.server.server_id(), second_id);
    assert_eq!(
        restarted_flags(&events),
        [(first_id, true), (second_id, false), (second_id, true)]
    );
}

#[gpui::test]
async fn test_clangd_memory_report_reads_memory_usage(cx: &mut TestAppContext) {
    init_test(cx);
    let OpenedC {
        project, server, ..
    } = open_c_file(cx).await;
    let lsp_store = lsp_store(&project, cx);
    lsp_store.update(cx, |lsp_store, _| {
        lsp_store.set_clangd_resident_memory_for_tests(Arc::new(|_| Some(900 * MEGABYTE)));
    });
    server.set_request_handler::<MemoryUsage, _, _>(|_, _| async move {
        Ok(json!({
            "_self": 0,
            "_total": 700,
            "clangd_server": {
                "_self": 0,
                "_total": 700,
                "background_index": { "_self": 500, "_total": 500 },
                "tuscheduler": { "_self": 200, "_total": 200 }
            }
        }))
    });
    let server_id = server.server.server_id();
    let report = lsp_store
        .update(cx, |lsp_store, cx| {
            lsp_store.clangd_memory_report(server_id, cx)
        })
        .await
        .expect("a report");
    assert_eq!(
        report,
        ClangdMemoryReport {
            resident_bytes: Some(900 * MEGABYTE),
            breakdown: Some(ClangdMemoryBreakdown {
                total_bytes: 700,
                background_index_bytes: 500,
                dynamic_index_bytes: 0,
                open_files_bytes: 200,
            }),
        }
    );
}

#[gpui::test]
async fn test_clangd_worker_count_from_settings_is_capped(cx: &mut TestAppContext) {
    init_test(cx);
    set_clangd_settings(
        ClangdSettingsContent {
            max_workers: Some(u32::MAX),
            ..Default::default()
        },
        cx,
    );
    let OpenedC { server, .. } = open_c_file(cx).await;
    assert_eq!(arguments(&server), ["-j=256"]);
}

#[gpui::test]
async fn test_clangd_restart_reopens_unsaved_text_of_buffers_still_open(cx: &mut TestAppContext) {
    init_test(cx);
    set_clangd_settings(
        ClangdSettingsContent {
            memory_limit_mb: Some(100),
            ..Default::default()
        },
        cx,
    );
    let OpenedC {
        project,
        mut servers,
        server: first,
        did_open_texts,
        buffer,
        _handle,
        ..
    } = open_c_file(cx).await;
    let lsp_store = lsp_store(&project, cx);
    let (_other_buffer, other_handle) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/root/b.c"), cx)
        })
        .await
        .expect("opening b.c");
    cx.run_until_parked();
    let first_id = first.server.server_id();
    assert_eq!(
        did_open_texts
            .lock()
            .iter()
            .map(|(server_id, name, _)| (*server_id, name.clone()))
            .collect::<Vec<_>>(),
        [(first_id, "a.c".to_string()), (first_id, "b.c".to_string())]
    );

    buffer.update(cx, |buffer, cx| {
        buffer.edit([(0..0, "// unsaved\n")], None, cx)
    });
    // Like an editor with b.c in an excerpt it no longer shows: it lets go of
    // the buffer when a language server goes away.
    let other_handle = Rc::new(RefCell::new(Some(other_handle)));
    cx.update(|cx| {
        let other_handle = other_handle.clone();
        cx.subscribe(&lsp_store, move |_, event, _| {
            if let LspStoreEvent::LanguageServerRemoved(_) = event {
                other_handle.borrow_mut().take();
            }
        })
        .detach();
    });
    lsp_store.update(cx, |lsp_store, _| {
        lsp_store.set_clangd_resident_memory_for_tests(Arc::new(|_| Some(200 * MEGABYTE)));
    });
    cx.executor().advance_clock(MEMORY_CHECK_INTERVAL);
    cx.run_until_parked();
    let second = servers
        .next()
        .now_or_never()
        .flatten()
        .expect("clangd is restarted over its limit");
    let second_id = second.server.server_id();
    assert!(
        other_handle.borrow().is_none(),
        "the old clangd was removed"
    );
    let reopened = did_open_texts
        .lock()
        .iter()
        .filter(|(server_id, _, _)| *server_id == second_id)
        // Windows sends the buffer's line endings.
        .map(|(_, name, text)| (name.clone(), text.replace("\r\n", "\n")))
        .collect::<Vec<_>>();
    assert_eq!(
        reopened,
        [(
            "a.c".to_string(),
            "// unsaved\nint main(void) { return 0; }".to_string()
        )],
        "the new clangd gets the buffer's text, not the file's, and not b.c that no editor holds any more"
    );
}

#[gpui::test]
async fn test_clangd_memory_report_does_not_wait_long_for_a_silent_clangd(cx: &mut TestAppContext) {
    init_test(cx);
    let OpenedC {
        project, server, ..
    } = open_c_file(cx).await;
    let lsp_store = lsp_store(&project, cx);
    lsp_store.update(cx, |lsp_store, _| {
        lsp_store.set_clangd_resident_memory_for_tests(Arc::new(|_| Some(900 * MEGABYTE)));
    });
    server.set_request_handler::<MemoryUsage, _, _>(|_, _| async move {
        futures::future::pending::<()>().await;
        Ok(json!(null))
    });
    let server_id = server.server.server_id();
    let report = lsp_store.update(cx, |lsp_store, cx| {
        lsp_store.clangd_memory_report(server_id, cx)
    });
    cx.run_until_parked();
    cx.executor().advance_clock(MEMORY_REPORT_TIMEOUT);
    cx.run_until_parked();
    let report = report
        .now_or_never()
        .expect("answered once the report timeout has passed")
        .expect("a report");
    assert_eq!(
        report,
        ClangdMemoryReport {
            resident_bytes: Some(900 * MEGABYTE),
            breakdown: None,
        }
    );
}

#[gpui::test]
async fn test_clangd_without_memory_usage_is_not_asked_again(cx: &mut TestAppContext) {
    init_test(cx);
    let OpenedC {
        project, server, ..
    } = open_c_file(cx).await;
    let lsp_store = lsp_store(&project, cx);
    let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    server.set_request_handler::<MemoryUsage, _, _>({
        let asked = asked.clone();
        move |_, _| {
            asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async move { Err(anyhow::anyhow!("method not found")) }
        }
    });
    let server_id = server.server.server_id();
    for _ in 0..2 {
        let report = lsp_store
            .update(cx, |lsp_store, cx| {
                lsp_store.clangd_memory_report(server_id, cx)
            })
            .await
            .expect("a report without a breakdown");
        assert_eq!(report.breakdown, None);
    }
    assert_eq!(asked.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[gpui::test]
async fn test_clangd_memory_watch_resumes_when_a_limit_is_set_again(cx: &mut TestAppContext) {
    init_test(cx);
    set_clangd_settings(
        ClangdSettingsContent {
            memory_limit_mb: Some(100),
            ..Default::default()
        },
        cx,
    );
    let OpenedC {
        project,
        mut servers,
        _handle,
        ..
    } = open_c_file(cx).await;
    let lsp_store = lsp_store(&project, cx);
    lsp_store.update(cx, |lsp_store, _| {
        lsp_store.set_clangd_resident_memory_for_tests(Arc::new(|_| Some(200 * MEGABYTE)));
    });
    set_clangd_settings(ClangdSettingsContent::default(), cx);
    cx.executor().advance_clock(MEMORY_CHECK_INTERVAL * 2);
    cx.run_until_parked();
    assert!(
        servers.next().now_or_never().is_none(),
        "no limit, no restart"
    );

    set_clangd_settings(
        ClangdSettingsContent {
            memory_limit_mb: Some(100),
            ..Default::default()
        },
        cx,
    );
    cx.executor().advance_clock(MEMORY_CHECK_INTERVAL);
    cx.run_until_parked();
    assert!(
        servers.next().now_or_never().flatten().is_some(),
        "the limit is watched again"
    );
}

#[gpui::test]
async fn test_clangd_limits_come_from_the_user_settings_only(cx: &mut TestAppContext) {
    init_test(cx);
    let OpenedC {
        project,
        mut servers,
        server: first,
        _handle,
        ..
    } = open_c_file_in(
        json!({
            ".zed": {
                "settings.json": r#"{ "clangd": { "index_mode": "light", "memory_limit_mb": 1, "max_workers": 9999 } }"#
            },
            "a.c": "int main(void) { return 0; }",
        }),
        cx,
    )
    .await;
    assert_eq!(
        arguments(&first),
        ["--background-index=false"],
        "the project's index mode applies, its worker count does not"
    );
    let lsp_store = lsp_store(&project, cx);
    lsp_store.update(cx, |lsp_store, _| {
        lsp_store.set_clangd_resident_memory_for_tests(Arc::new(|_| Some(900 * MEGABYTE)));
    });
    let events = record_events(cx);
    cx.executor().advance_clock(MEMORY_CHECK_INTERVAL * 2);
    cx.run_until_parked();
    assert!(
        events.borrow().is_empty(),
        "the project's memory limit does not apply"
    );
    assert!(servers.next().now_or_never().is_none());

    set_clangd_settings(
        ClangdSettingsContent {
            index_mode: None,
            max_workers: Some(3),
            memory_limit_mb: Some(100),
        },
        cx,
    );
    cx.run_until_parked();
    let second = servers
        .next()
        .now_or_never()
        .flatten()
        .expect("the user's worker count restarts clangd");
    assert_eq!(arguments(&second), ["--background-index=false", "-j=3"]);
    cx.executor().advance_clock(MEMORY_CHECK_INTERVAL);
    cx.run_until_parked();
    assert_eq!(
        events.borrow().first(),
        Some(&ClangdMemoryEvent::LimitExceeded {
            lsp_store: lsp_store.entity_id(),
            server_id: second.server.server_id(),
            resident_bytes: 900 * MEGABYTE,
            limit_bytes: 100 * MEGABYTE,
            restarted: true,
        }),
        "the user's memory limit applies"
    );
}
