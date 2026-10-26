//! The name index joining the language server in [`call_hierarchy`] (C and C++ files).

use super::*;
use crate::LanguageServerNotRunning;
use futures::{FutureExt as _, StreamExt as _};
use gpui::{TestAppContext, UpdateGlobal as _};
use language::{FakeLspAdapter, Language, LanguageConfig, LanguageMatcher};
use project::{
    FakeFs, WorktreeId, buffer_store::BufferStoreEvent, lsp_store::OpenLspBufferHandle,
    symbol_index_store::build_index_for_tests,
};
use serde_json::json;
use settings::{SettingsStore, SplicingVec};
use std::sync::{Arc, Mutex};
use util::{path, rel_path::rel_path};

const MAIN_C: &str = r#"#define LOG(text) log_line(text)
static int helper(int value) {
    return value + 1;
}
int middle(void) {
    return helper(1);
}
int top(struct ops *ops) {
    middle();
    ops->open(1);
    LOG("x");
    printf("done");
    return dup();
}
void recurse(void) {
    recurse();
}
"#;
const OTHER_C: &str = r#"static int helper(int value) {
    return value;
}
int caller_of_middle(void) {
    return middle() + helper(2);
}
int dup(void) { return 1; }
int entry(void) {
    return caller_of_middle();
}
"#;
const THIRD_C: &str = r#"int dup(void) { return 2; }
static int real_open(int flags) { return flags; }
struct ops table = {
    .open = real_open,
};
void log_line(const char *text) {}
"#;
const SECRET_C: &str = "int hidden(void) { return middle(); }\n";
const LIB_RS: &str = "fn middle() {}\n";

struct Fixture {
    project: Entity<Project>,
    worktree_id: WorktreeId,
    _index_directory: tempfile::TempDir,
}

fn init_test(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |settings| {
                settings.project.worktree.file_scan_exclusions =
                    Some(SplicingVec::from(vec!["**/excluded".to_string()]));
                settings.project.worktree.private_files =
                    Some(vec!["**/secret.c".to_string()].into());
            });
        });
    });
}

fn c_language() -> Arc<Language> {
    Arc::new(Language::new(
        LanguageConfig {
            name: "C".into(),
            matcher: LanguageMatcher {
                path_suffixes: vec!["c".into(), "h".into()],
                ..LanguageMatcher::default()
            }
            .into(),
            ..LanguageConfig::default()
        },
        None,
    ))
}

async fn project_without_index(cx: &mut TestAppContext) -> (Entity<Project>, WorktreeId) {
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/root"),
        json!({
            "src": {
                "main.c": MAIN_C,
                "other.c": OTHER_C,
                "third.c": THIRD_C,
                "lib.rs": LIB_RS,
            },
            "secret.c": SECRET_C,
            "excluded": { "gen.c": "int generated(void) { return 0; }\n" },
        }),
    )
    .await;
    fs.insert_tree(
        path!("/outside"),
        json!({ "linked.c": "int linked(void) { return 0; }\n" }),
    )
    .await;
    fs.insert_symlink(path!("/root/link"), path!("/outside").into())
        .await;
    let project = Project::test(fs, [path!("/root").as_ref()], cx).await;
    project.read_with(cx, |project, _| project.languages().add(c_language()));
    cx.executor().run_until_parked();
    let worktree_id = project.read_with(cx, |project, cx| {
        project
            .visible_worktrees(cx)
            .next()
            .expect("the project has a worktree")
            .read(cx)
            .id()
    });
    (project, worktree_id)
}

/// The index holds `secret.c` too, as one built before the file became private would: the
/// store leaves it out of every answer.
async fn fixture(cx: &mut TestAppContext) -> Fixture {
    let (project, worktree_id) = project_without_index(cx).await;
    let index_directory = tempfile::tempdir().expect("a temporary directory");
    let index = build_index_for_tests(
        index_directory.path(),
        &[
            ("src/main.c", MAIN_C),
            ("src/other.c", OTHER_C),
            ("src/third.c", THIRD_C),
            ("secret.c", SECRET_C),
        ],
    )
    .expect("building the index");
    project.update(cx, |project, cx| {
        project.symbol_index_store().update(cx, |store, cx| {
            store.insert_index_for_tests(worktree_id, index, cx)
        })
    });
    cx.executor().run_until_parked();
    Fixture {
        project,
        worktree_id,
        _index_directory: index_directory,
    }
}

fn project_path(fixture: &Fixture, path: &str) -> ProjectPath {
    ProjectPath {
        worktree_id: fixture.worktree_id,
        path: rel_path(path).into(),
    }
}

fn at(fixture: &Fixture, path: &str, line: u32, symbol: &str) -> CallHierarchyTarget {
    CallHierarchyTarget::Position {
        path: project_path(fixture, path),
        position: LinePosition {
            line,
            symbol: (!symbol.is_empty()).then(|| symbol.to_string()),
            column: None,
        },
    }
}

fn options(direction: CallDirection, depth: usize) -> CallHierarchyOptions {
    CallHierarchyOptions {
        direction,
        depth,
        max_results: 0,
    }
}

fn ask(
    project: &Entity<Project>,
    target: CallHierarchyTarget,
    options: CallHierarchyOptions,
    cx: &mut TestAppContext,
) -> Task<Result<CallHierarchyAnswer>> {
    cx.update(|cx| call_hierarchy(project, target, options, cx))
}

fn found(answer: CallHierarchyAnswer) -> CallHierarchy {
    match answer {
        CallHierarchyAnswer::Found(hierarchy) => hierarchy,
        other => panic!("expected a tree, got {other:?}"),
    }
}

/// One line per node: name (`-` when withheld), place, call lines, preview, how far it was
/// followed, and what marks it carries.
fn outline(node: &CallNode) -> Vec<String> {
    fn visit(node: &CallNode, indent: usize, lines: &mut Vec<String>) {
        let place = if node.in_project {
            node.path.path.as_unix_str().to_string()
        } else {
            "<outside>".to_string()
        };
        let mut line = format!(
            "{}{} {place}:{}-{} {:?} {:?} {:?}",
            "  ".repeat(indent),
            if node.name.is_empty() {
                "-"
            } else {
                &node.name
            },
            node.lines.start_line,
            node.lines.end_line,
            node.call_lines,
            node.preview,
            node.expansion,
        );
        match node.source {
            CallSource::LanguageServer => {}
            CallSource::NameIndex => line.push_str(" [name]"),
            CallSource::Candidate => line.push_str(" [candidate]"),
            CallSource::Macro => line.push_str(" [macro]"),
        }
        if let Some(counts) = &node.ambiguous {
            line.push_str(&format!(" ambiguous:{}", counts.shown));
            if counts.withheld > 0 {
                line.push_str(&format!("+{}withheld", counts.withheld));
            }
        }
        match &node.resolution {
            CallResolution::Definition => {}
            CallResolution::NoDefinition => line.push_str(" no-definition"),
            CallResolution::Indirect { member } => line.push_str(&format!(" via .{member}")),
        }
        if !node.contents_allowed {
            line.push_str(" withheld");
        }
        lines.push(line);
        for child in &node.children {
            visit(child, indent + 1, lines);
        }
    }
    let mut lines = Vec::new();
    visit(node, 0, &mut lines);
    lines
}

fn function(name: &str) -> lsp::CallHierarchyItem {
    let (abs_path, row, end_row, column) = match name {
        "helper" => (path!("/root/src/main.c"), 1, 3, 11),
        "middle" => (path!("/root/src/main.c"), 4, 6, 4),
        "top" => (path!("/root/src/main.c"), 7, 13, 4),
        "dup" => (path!("/root/src/other.c"), 6, 6, 4),
        "caller_of_middle" => (path!("/root/src/other.c"), 3, 5, 4),
        other => panic!("not a function of the test: {other}"),
    };
    lsp::CallHierarchyItem {
        name: name.to_string(),
        kind: lsp::SymbolKind::FUNCTION,
        tags: None,
        detail: None,
        uri: lsp::Uri::from_file_path(abs_path).expect("an absolute path"),
        range: lsp::Range::new(lsp::Position::new(row, 0), lsp::Position::new(end_row, 1)),
        selection_range: lsp::Range::new(
            lsp::Position::new(row, column),
            lsp::Position::new(row, column + name.len() as u32),
        ),
        data: None,
    }
}

fn call_ranges(rows: &[u32]) -> Vec<lsp::Range> {
    rows.iter()
        .map(|row| lsp::Range::new(lsp::Position::new(*row, 4), lsp::Position::new(*row, 8)))
        .collect()
}

async fn start_c_server(
    project: &Entity<Project>,
    supports_call_hierarchy: bool,
    cx: &mut TestAppContext,
) -> (lsp::FakeLanguageServer, OpenLspBufferHandle) {
    let language_registry = project.read_with(cx, |project, _| project.languages().clone());
    let mut fake_servers = language_registry.register_fake_lsp(
        "C",
        FakeLspAdapter {
            capabilities: lsp::ServerCapabilities {
                call_hierarchy_provider: supports_call_hierarchy
                    .then_some(lsp::CallHierarchyServerCapability::Simple(true)),
                ..lsp::ServerCapabilities::default()
            },
            ..FakeLspAdapter::default()
        },
    );
    let (_buffer, registration) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/root/src/main.c"), cx)
        })
        .await
        .expect("the file opens");
    let server = fake_servers.next().await.expect("the server starts");
    cx.executor().run_until_parked();
    (server, registration)
}

/// A language server that knows part of the graph: `top` calls `middle`, on a line the index
/// doesn't know of (while the index knows another); `top` calls `middle` and the `dup` of
/// `other.c`. It knows no caller in `other.c`. Records what each incoming or outgoing request
/// asks about.
fn serve_part_of_the_graph(
    server: &lsp::FakeLanguageServer,
    stalled: Option<&'static str>,
) -> Arc<Mutex<Vec<String>>> {
    let asked = Arc::new(Mutex::new(Vec::new()));
    server.set_request_handler::<lsp::request::CallHierarchyPrepare, _, _>(
        move |params, _| async move {
            if stalled == Some("prepare") {
                futures::future::pending::<()>().await;
            }
            let position = params.text_document_position_params.position;
            Ok(match position.line {
                1 => Some(vec![function("helper")]),
                4 => Some(vec![function("middle")]),
                7 => Some(vec![function("top")]),
                _ => None,
            })
        },
    );
    server.set_request_handler::<lsp::request::CallHierarchyIncomingCalls, _, _>({
        let asked = asked.clone();
        move |params, _| {
            let asked = asked.clone();
            async move {
                let name = params.item.name;
                asked.lock().expect("not poisoned").push(name.clone());
                if stalled == Some(name.as_str()) {
                    futures::future::pending::<()>().await;
                }
                Ok(Some(match name.as_str() {
                    "middle" => vec![lsp::CallHierarchyIncomingCall {
                        from: function("top"),
                        from_ranges: call_ranges(&[12]),
                    }],
                    _ => Vec::new(),
                }))
            }
        }
    });
    server.set_request_handler::<lsp::request::CallHierarchyOutgoingCalls, _, _>({
        let asked = asked.clone();
        move |params, _| {
            let asked = asked.clone();
            async move {
                let name = params.item.name;
                asked.lock().expect("not poisoned").push(name.clone());
                Ok(Some(match name.as_str() {
                    "top" => vec![
                        lsp::CallHierarchyOutgoingCall {
                            to: function("middle"),
                            from_ranges: call_ranges(&[8]),
                        },
                        lsp::CallHierarchyOutgoingCall {
                            to: function("dup"),
                            from_ranges: call_ranges(&[12]),
                        },
                    ],
                    _ => Vec::new(),
                }))
            }
        }
    });
    asked
}

fn asked(asked: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
    asked.lock().expect("not poisoned").clone()
}

/// No language server: the name index answers alone, two levels deep, and says so. `hidden`
/// (in a private file the index still holds) calls `middle` too, and is not listed.
#[gpui::test]
async fn test_the_index_answers_without_a_language_server(cx: &mut TestAppContext) {
    init_test(cx);
    let fixture = fixture(cx).await;

    let hierarchy = found(
        ask(
            &fixture.project,
            at(&fixture, "src/main.c", 5, "middle"),
            options(CallDirection::Incoming, 2),
            cx,
        )
        .await
        .expect("the index answers"),
    );
    assert_eq!(
        outline(&hierarchy.root),
        [
            "middle src/main.c:5-7 [] \"int middle(void) {\" Expanded [name]",
            "  top src/main.c:8-14 [9] \"middle();\" Expanded [name]",
            "  caller_of_middle src/other.c:4-6 [5] \"return middle() + helper(2);\" Expanded [name]",
            "    entry src/other.c:8-10 [9] \"return caller_of_middle();\" DepthLimit [name]",
        ]
    );
    assert!(hierarchy.used_name_index);
    assert!(hierarchy.without_language_server);
    assert_eq!(hierarchy.index_building, None);
    assert_eq!(hierarchy.stopped, None);
    assert!(!format!("{hierarchy:?}").contains("hidden"));
}

/// While the index is still being built it answers with what it has, and says how far it has
/// got, so that callers can tell results may be missing.
#[gpui::test]
async fn test_an_index_still_being_built_says_so(cx: &mut TestAppContext) {
    init_test(cx);
    let fixture = fixture(cx).await;
    let set_status = |status: SymbolIndexStatus, cx: &mut TestAppContext| {
        fixture.project.update(cx, |project, cx| {
            project.symbol_index_store().update(cx, |store, cx| {
                store.set_status_for_tests(fixture.worktree_id, status, cx)
            })
        });
        cx.executor().run_until_parked();
    };
    set_status(SymbolIndexStatus::Indexing { done: 2, total: 4 }, cx);

    let hierarchy = found(
        ask(
            &fixture.project,
            at(&fixture, "src/main.c", 5, "middle"),
            options(CallDirection::Incoming, 1),
            cx,
        )
        .await
        .expect("the index answers while it is built"),
    );
    assert!(hierarchy.used_name_index);
    assert_eq!(
        hierarchy.index_building,
        Some(IndexBuilding { done: 2, total: 4 })
    );
    assert!(
        outline(&hierarchy.root)
            .iter()
            .any(|line| line.contains("caller_of_middle")),
        "{:?}",
        outline(&hierarchy.root)
    );

    set_status(
        SymbolIndexStatus::Ready {
            stats: Default::default(),
        },
        cx,
    );
    let hierarchy = found(
        ask(
            &fixture.project,
            at(&fixture, "src/main.c", 5, "middle"),
            options(CallDirection::Incoming, 1),
            cx,
        )
        .await
        .expect("the index answers"),
    );
    assert_eq!(hierarchy.index_building, None);
}

/// Outgoing calls through the index: a function defined once, a macro, a library function, a
/// name defined twice and a call through a member with the function stored into it.
#[gpui::test]
async fn test_outgoing_calls_from_the_index_carry_their_marks(cx: &mut TestAppContext) {
    init_test(cx);
    let fixture = fixture(cx).await;

    let hierarchy = found(
        ask(
            &fixture.project,
            at(&fixture, "src/main.c", 8, "top"),
            options(CallDirection::Outgoing, 2),
            cx,
        )
        .await
        .expect("the index answers"),
    );
    assert_eq!(
        outline(&hierarchy.root),
        [
            "top src/main.c:8-14 [] \"int top(struct ops *ops) {\" Expanded [name]",
            "  LOG src/main.c:1-1 [11] \"#define LOG(text) log_line(text)\" Expanded [macro]",
            "    log_line src/third.c:6-6 [1] \"void log_line(const char *text) {}\" DepthLimit [name]",
            "  middle src/main.c:5-7 [9] \"int middle(void) {\" Expanded [name]",
            "    helper src/main.c:2-4 [6] \"static int helper(int value) {\" DepthLimit [name]",
            "  open src/main.c:10-10 [10] \"ops->open(1);\" DepthLimit [name] via .open",
            "    real_open src/third.c:4-4 [] \".open = real_open,\" DepthLimit [candidate]",
            "  printf src/main.c:12-12 [12] \"printf(\\\"done\\\");\" DepthLimit [name] no-definition",
            "  dup src/other.c:7-7 [13] \"int dup(void) { return 1; }\" Expanded [name] ambiguous:2",
            "  dup src/third.c:1-1 [13] \"int dup(void) { return 2; }\" Expanded [name] ambiguous:2",
        ]
    );
    assert_eq!(hierarchy.listed_calls(), 9);
}

/// A `static` function can only be called from its own file: `helper` of `main.c` is not
/// called by `other.c`, which has a `helper` of its own.
#[gpui::test]
async fn test_callers_of_a_static_function_stay_in_its_file(cx: &mut TestAppContext) {
    init_test(cx);
    let fixture = fixture(cx).await;

    let hierarchy = found(
        ask(
            &fixture.project,
            at(&fixture, "src/main.c", 2, "helper"),
            options(CallDirection::Incoming, 1),
            cx,
        )
        .await
        .expect("the index answers"),
    );
    assert_eq!(
        outline(&hierarchy.root),
        [
            "helper src/main.c:2-4 [] \"static int helper(int value) {\" Expanded [name]",
            "  middle src/main.c:5-7 [6] \"return helper(1);\" DepthLimit [name]",
        ]
    );

    let hierarchy = found(
        ask(
            &fixture.project,
            at(&fixture, "src/other.c", 1, "helper"),
            options(CallDirection::Incoming, 1),
            cx,
        )
        .await
        .expect("the index answers"),
    );
    assert_eq!(
        outline(&hierarchy.root),
        [
            "helper src/other.c:1-3 [] \"static int helper(int value) {\" Expanded [name]",
            "  caller_of_middle src/other.c:4-6 [5] \"return middle() + helper(2);\" DepthLimit [name]",
        ]
    );
}

const SHADOW_A_C: &str = "int f(void) { return 0; }\nint g(void) { return f(); }\n";
const SHADOW_B_C: &str = "static int f(void) { return 1; }\nint h(void) { return f(); }\n";
const SHADOW_C_C: &str = "int k(void) { return f(); }\n";

/// `a.c` and `b.c` each define an `f`, `b.c`'s `static`; `c.c` calls `f` without one of its own.
async fn shadowing_fixture(cx: &mut TestAppContext) -> Fixture {
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/root"),
        json!({ "src": { "a.c": SHADOW_A_C, "b.c": SHADOW_B_C, "c.c": SHADOW_C_C } }),
    )
    .await;
    let project = Project::test(fs, [path!("/root").as_ref()], cx).await;
    project.read_with(cx, |project, _| project.languages().add(c_language()));
    cx.executor().run_until_parked();
    let worktree_id = project.read_with(cx, |project, cx| {
        project
            .visible_worktrees(cx)
            .next()
            .expect("the project has a worktree")
            .read(cx)
            .id()
    });
    let index_directory = tempfile::tempdir().expect("a temporary directory");
    let index = build_index_for_tests(
        index_directory.path(),
        &[
            ("src/a.c", SHADOW_A_C),
            ("src/b.c", SHADOW_B_C),
            ("src/c.c", SHADOW_C_C),
        ],
    )
    .expect("building the index");
    project.update(cx, |project, cx| {
        project.symbol_index_store().update(cx, |store, cx| {
            store.insert_index_for_tests(worktree_id, index, cx)
        })
    });
    cx.executor().run_until_parked();
    Fixture {
        project,
        worktree_id,
        _index_directory: index_directory,
    }
}

/// A call in a file with its own `static` function of the name goes to that function: it is
/// not a caller of another file's function of the name, `static` or not.
#[gpui::test]
async fn test_a_files_own_static_function_takes_its_calls(cx: &mut TestAppContext) {
    init_test(cx);
    let fixture = shadowing_fixture(cx).await;

    let hierarchy = found(
        ask(
            &fixture.project,
            at(&fixture, "src/a.c", 1, "f"),
            options(CallDirection::Incoming, 1),
            cx,
        )
        .await
        .expect("the index answers"),
    );
    assert_eq!(
        outline(&hierarchy.root),
        [
            "f src/a.c:1-1 [] \"int f(void) { return 0; }\" Expanded [name]",
            "  g src/a.c:2-2 [2] \"int g(void) { return f(); }\" DepthLimit [name]",
            "  k src/c.c:1-1 [1] \"int k(void) { return f(); }\" DepthLimit [name]",
        ]
    );

    let hierarchy = found(
        ask(
            &fixture.project,
            at(&fixture, "src/b.c", 1, "f"),
            options(CallDirection::Incoming, 1),
            cx,
        )
        .await
        .expect("the index answers"),
    );
    assert_eq!(
        outline(&hierarchy.root),
        [
            "f src/b.c:1-1 [] \"static int f(void) { return 1; }\" Expanded [name]",
            "  h src/b.c:2-2 [2] \"int h(void) { return f(); }\" DepthLimit [name]",
        ]
    );
}

/// Calls found in the body of a macro are the macro's; recursion stops the walk.
#[gpui::test]
async fn test_macros_and_recursion_through_the_index(cx: &mut TestAppContext) {
    init_test(cx);
    let fixture = fixture(cx).await;

    let hierarchy = found(
        ask(
            &fixture.project,
            at(&fixture, "src/third.c", 6, "log_line"),
            options(CallDirection::Incoming, 3),
            cx,
        )
        .await
        .expect("the index answers"),
    );
    assert_eq!(
        outline(&hierarchy.root),
        [
            "log_line src/third.c:6-6 [] \"void log_line(const char *text) {}\" Expanded [name]",
            "  LOG src/main.c:1-1 [1] \"#define LOG(text) log_line(text)\" Expanded [macro]",
            "    top src/main.c:8-14 [11] \"LOG(\\\"x\\\");\" Expanded [name]",
        ]
    );

    let hierarchy = found(
        ask(
            &fixture.project,
            at(&fixture, "src/main.c", 15, "recurse"),
            options(CallDirection::Incoming, 3),
            cx,
        )
        .await
        .expect("the index answers"),
    );
    assert_eq!(
        outline(&hierarchy.root),
        [
            "recurse src/main.c:15-17 [] \"void recurse(void) {\" Expanded [name]",
            "  recurse src/main.c:15-17 [16] \"recurse();\" Recursive [name]",
        ]
    );
}

/// A position that names no function falls back to the function whose body holds it.
#[gpui::test]
async fn test_a_position_in_a_body_means_its_function(cx: &mut TestAppContext) {
    init_test(cx);
    let fixture = fixture(cx).await;

    let hierarchy = found(
        ask(
            &fixture.project,
            at(&fixture, "src/main.c", 3, "value"),
            options(CallDirection::Incoming, 1),
            cx,
        )
        .await
        .expect("the index answers"),
    );
    assert_eq!(hierarchy.root.name, "helper");
    assert_eq!(hierarchy.root.lines.start_line, 2);

    let answer = ask(
        &fixture.project,
        at(&fixture, "src/third.c", 4, ""),
        options(CallDirection::Incoming, 1),
        cx,
    )
    .await
    .expect("the index answers");
    assert_eq!(
        answer,
        CallHierarchyAnswer::NoFunction {
            used_name_index: true,
            index_building: None,
        }
    );
}

/// The language server and the index both answer: what both found is the language server's,
/// with the call lines of both; what only the index found comes after, marked, and is followed
/// through the index alone. Where the language server resolved a called name, the index's other
/// functions of that name (the `dup` of `third.c`) are left out.
#[gpui::test]
async fn test_the_index_joins_the_language_server(cx: &mut TestAppContext) {
    init_test(cx);
    let fixture = fixture(cx).await;
    let (server, _registration) = start_c_server(&fixture.project, true, cx).await;
    let requests = serve_part_of_the_graph(&server, None);

    let hierarchy = found(
        ask(
            &fixture.project,
            at(&fixture, "src/main.c", 5, "middle"),
            options(CallDirection::Incoming, 3),
            cx,
        )
        .await
        .expect("both answer"),
    );
    assert_eq!(
        outline(&hierarchy.root),
        [
            "middle src/main.c:5-7 [] \"int middle(void) {\" Expanded",
            "  top src/main.c:8-14 [9, 13] \"middle();\" Expanded",
            "  caller_of_middle src/other.c:4-6 [5] \"return middle() + helper(2);\" Expanded [name]",
            "    entry src/other.c:8-10 [9] \"return caller_of_middle();\" Expanded [name]",
        ]
    );
    assert!(hierarchy.used_name_index);
    assert!(!hierarchy.without_language_server);
    assert_eq!(
        asked(&requests),
        ["middle", "top"],
        "what only the index found is not sent to the language server"
    );

    let hierarchy = found(
        ask(
            &fixture.project,
            at(&fixture, "src/main.c", 8, "top"),
            options(CallDirection::Outgoing, 1),
            cx,
        )
        .await
        .expect("both answer"),
    );
    assert_eq!(
        outline(&hierarchy.root),
        [
            "top src/main.c:8-14 [] \"int top(struct ops *ops) {\" Expanded",
            "  middle src/main.c:5-7 [9] \"int middle(void) {\" DepthLimit",
            "  dup src/other.c:7-7 [13] \"int dup(void) { return 1; }\" DepthLimit",
            "  LOG src/main.c:1-1 [11] \"#define LOG(text) log_line(text)\" DepthLimit [macro]",
            "  open src/main.c:10-10 [10] \"ops->open(1);\" DepthLimit [name] via .open",
            "    real_open src/third.c:4-4 [] \".open = real_open,\" DepthLimit [candidate]",
            "  printf src/main.c:12-12 [12] \"printf(\\\"done\\\");\" DepthLimit [name] no-definition",
        ]
    );
}

/// The language server doesn't say whether a function is `static`; the index does, so the
/// language server's root is matched to the index's definition before the index is asked for
/// its callers.
#[gpui::test]
async fn test_a_static_root_from_the_language_server_keeps_its_callers_in_its_file(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    let fixture = fixture(cx).await;
    let (server, _registration) = start_c_server(&fixture.project, true, cx).await;
    serve_part_of_the_graph(&server, None);

    let hierarchy = found(
        ask(
            &fixture.project,
            at(&fixture, "src/main.c", 2, "helper"),
            options(CallDirection::Incoming, 1),
            cx,
        )
        .await
        .expect("both answer"),
    );
    assert_eq!(
        outline(&hierarchy.root),
        [
            "helper src/main.c:2-4 [] \"static int helper(int value) {\" Expanded",
            "  middle src/main.c:5-7 [6] \"return helper(1);\" DepthLimit [name]",
        ]
    );
}

/// A server without call hierarchy leaves the index to answer alone.
#[gpui::test]
async fn test_the_index_answers_when_the_language_server_cannot(cx: &mut TestAppContext) {
    init_test(cx);
    let fixture = fixture(cx).await;
    let (_server, _registration) = start_c_server(&fixture.project, false, cx).await;

    let hierarchy = found(
        ask(
            &fixture.project,
            at(&fixture, "src/main.c", 5, "middle"),
            options(CallDirection::Incoming, 1),
            cx,
        )
        .await
        .expect("the index answers"),
    );
    assert!(hierarchy.without_language_server);
    assert_eq!(hierarchy.root.source, CallSource::NameIndex);
    assert_eq!(hierarchy.root.children.len(), 2);
}

/// A server that doesn't answer where the function is leaves the index to answer alone, once
/// the server has had its time.
#[gpui::test]
async fn test_a_language_server_that_does_not_answer_leaves_the_index(cx: &mut TestAppContext) {
    init_test(cx);
    let fixture = fixture(cx).await;
    let (server, _registration) = start_c_server(&fixture.project, true, cx).await;
    serve_part_of_the_graph(&server, Some("prepare"));

    let mut task = ask(
        &fixture.project,
        at(&fixture, "src/main.c", 5, "middle"),
        options(CallDirection::Incoming, 1),
        cx,
    );
    cx.executor().run_until_parked();
    assert!((&mut task).now_or_never().is_none(), "waits for the server");
    cx.executor()
        .advance_clock(LANGUAGE_SERVER_TIMEOUT + Duration::from_secs(1));
    cx.executor().run_until_parked();
    let hierarchy = found(
        task.now_or_never()
            .expect("the index answers once the server has timed out")
            .expect("the index answers"),
    );
    assert!(hierarchy.without_language_server);
    assert_eq!(hierarchy.stopped, None);
    assert_eq!(
        outline(&hierarchy.root)[1..],
        [
            "  top src/main.c:8-14 [9] \"middle();\" DepthLimit [name]",
            "  caller_of_middle src/other.c:4-6 [5] \"return middle() + helper(2);\" DepthLimit [name]",
        ]
    );
}

/// The server answers the root, then stops answering: the index goes on alone for the rest of
/// the walk instead of waiting out the server at every function.
#[gpui::test]
async fn test_a_server_that_stops_answering_leaves_the_rest_to_the_index(cx: &mut TestAppContext) {
    init_test(cx);
    let fixture = fixture(cx).await;
    let (server, _registration) = start_c_server(&fixture.project, true, cx).await;
    let requests = serve_part_of_the_graph(&server, Some("middle"));

    let mut task = ask(
        &fixture.project,
        at(&fixture, "src/main.c", 5, "middle"),
        options(CallDirection::Incoming, 2),
        cx,
    );
    cx.executor().run_until_parked();
    assert!((&mut task).now_or_never().is_none(), "waits for the server");
    cx.executor()
        .advance_clock(LANGUAGE_SERVER_TIMEOUT + Duration::from_secs(1));
    cx.executor().run_until_parked();
    let hierarchy = found(
        task.now_or_never()
            .expect("the walk goes on once the server has timed out")
            .expect("the index answers"),
    );
    assert_eq!(
        outline(&hierarchy.root),
        [
            "middle src/main.c:5-7 [] \"int middle(void) {\" Expanded",
            "  top src/main.c:8-14 [9] \"middle();\" Expanded [name]",
            "  caller_of_middle src/other.c:4-6 [5] \"return middle() + helper(2);\" Expanded [name]",
            "    entry src/other.c:8-10 [9] \"return caller_of_middle();\" DepthLimit [name]",
        ]
    );
    assert_eq!(asked(&requests), ["middle"]);
    assert!(!hierarchy.without_language_server);
}

/// Two functions the language server found are followed; it stops answering for the first, and
/// the second is then asked of the index alone rather than waiting out the server again.
#[gpui::test]
async fn test_once_the_language_server_times_out_it_is_not_asked_again(cx: &mut TestAppContext) {
    init_test(cx);
    let fixture = fixture(cx).await;
    let (server, _registration) = start_c_server(&fixture.project, true, cx).await;
    serve_part_of_the_graph(&server, None);
    let requests = Arc::new(Mutex::new(Vec::new()));
    server.set_request_handler::<lsp::request::CallHierarchyIncomingCalls, _, _>({
        let requests = requests.clone();
        move |params, _| {
            let requests = requests.clone();
            async move {
                let name = params.item.name;
                requests.lock().expect("not poisoned").push(name.clone());
                if name == "top" {
                    futures::future::pending::<()>().await;
                }
                Ok(Some(match name.as_str() {
                    "middle" => vec![
                        lsp::CallHierarchyIncomingCall {
                            from: function("top"),
                            from_ranges: call_ranges(&[8]),
                        },
                        lsp::CallHierarchyIncomingCall {
                            from: function("caller_of_middle"),
                            from_ranges: call_ranges(&[4]),
                        },
                    ],
                    _ => Vec::new(),
                }))
            }
        }
    });

    let task = ask(
        &fixture.project,
        at(&fixture, "src/main.c", 5, "middle"),
        options(CallDirection::Incoming, 2),
        cx,
    );
    cx.executor().run_until_parked();
    cx.executor()
        .advance_clock(LANGUAGE_SERVER_TIMEOUT + Duration::from_secs(1));
    cx.executor().run_until_parked();
    let hierarchy = found(
        task.now_or_never()
            .expect("the walk ends without waiting for the server a second time")
            .expect("the index answers"),
    );
    assert_eq!(asked(&requests), ["middle", "top"]);
    assert_eq!(
        outline(&hierarchy.root),
        [
            "middle src/main.c:5-7 [] \"int middle(void) {\" Expanded",
            "  top src/main.c:8-14 [9] \"middle();\" Expanded",
            "  caller_of_middle src/other.c:4-6 [5] \"return middle() + helper(2);\" Expanded",
            "    entry src/other.c:8-10 [9] \"return caller_of_middle();\" DepthLimit [name]",
        ]
    );
}

/// Only C and C++ files are indexed; elsewhere the name index is never asked, even for a name
/// it knows.
#[gpui::test]
async fn test_other_languages_do_not_use_the_index(cx: &mut TestAppContext) {
    init_test(cx);
    let fixture = fixture(cx).await;
    fixture.project.read_with(cx, |project, _| {
        project.languages().add(language::rust_lang())
    });

    let error = ask(
        &fixture.project,
        at(&fixture, "src/lib.rs", 1, "middle"),
        options(CallDirection::Incoming, 1),
        cx,
    )
    .await
    .expect_err("no language server runs for Rust");
    assert!(
        error.downcast_ref::<LanguageServerNotRunning>().is_some(),
        "{error}"
    );
}

/// A name alone: one function is walked like its position, several are listed to choose from.
#[gpui::test]
async fn test_a_name_alone(cx: &mut TestAppContext) {
    init_test(cx);
    let fixture = fixture(cx).await;
    let name = |name: &str, near: Option<&str>| CallHierarchyTarget::Name {
        name: name.to_string(),
        near: near.map(|near| project_path(&fixture, near)),
    };

    let hierarchy = found(
        ask(
            &fixture.project,
            name("middle", None),
            options(CallDirection::Incoming, 1),
            cx,
        )
        .await
        .expect("one function has the name"),
    );
    assert_eq!(
        outline(&hierarchy.root),
        [
            "middle src/main.c:5-7 [] \"int middle(void) {\" Expanded [name]",
            "  top src/main.c:8-14 [9] \"middle();\" DepthLimit [name]",
            "  caller_of_middle src/other.c:4-6 [5] \"return middle() + helper(2);\" DepthLimit [name]",
        ]
    );

    let answer = ask(
        &fixture.project,
        name("dup", None),
        options(CallDirection::Incoming, 1),
        cx,
    )
    .await
    .expect("two functions have the name");
    let CallHierarchyAnswer::Candidates(candidates) = answer else {
        panic!("expected candidates, got {answer:?}");
    };
    assert_eq!(candidates.name, "dup");
    assert_eq!(candidates.counts.shown, 2);
    assert_eq!(candidates.counts.withheld, 0);
    assert_eq!(
        candidates
            .candidates
            .iter()
            .flat_map(outline)
            .collect::<Vec<_>>(),
        [
            "dup src/other.c:7-7 [] \"int dup(void) { return 1; }\" DepthLimit [name]",
            "dup src/third.c:1-1 [] \"int dup(void) { return 2; }\" DepthLimit [name]",
        ]
    );

    let answer = ask(
        &fixture.project,
        name("helper", None),
        options(CallDirection::Incoming, 1),
        cx,
    )
    .await
    .expect("two static functions have the name");
    assert!(
        matches!(&answer, CallHierarchyAnswer::Candidates(candidates) if candidates.counts.shown == 2),
        "{answer:?}"
    );
    let hierarchy = found(
        ask(
            &fixture.project,
            name("helper", Some("src/other.c")),
            options(CallDirection::Incoming, 1),
            cx,
        )
        .await
        .expect("near a file, the other file's static function is left out"),
    );
    assert_eq!(hierarchy.root.path, project_path(&fixture, "src/other.c"));

    let answer = ask(
        &fixture.project,
        name("no_such_function", None),
        options(CallDirection::Incoming, 1),
        cx,
    )
    .await
    .expect("the index answers");
    assert_eq!(
        answer,
        CallHierarchyAnswer::NoFunction {
            used_name_index: true,
            index_building: None,
        }
    );

    let answer = ask(
        &fixture.project,
        name("hidden", None),
        options(CallDirection::Incoming, 1),
        cx,
    )
    .await
    .expect("the index answers");
    assert_eq!(
        answer,
        CallHierarchyAnswer::NoFunction {
            used_name_index: true,
            index_building: None,
        },
        "a function of a private file is not found by name"
    );
}

#[gpui::test]
async fn test_a_name_alone_needs_the_index(cx: &mut TestAppContext) {
    init_test(cx);
    let (project, _) = project_without_index(cx).await;
    let error = ask(
        &project,
        CallHierarchyTarget::Name {
            name: "middle".to_string(),
            near: None,
        },
        options(CallDirection::Incoming, 1),
        cx,
    )
    .await
    .expect_err("there is no index");
    assert!(
        error.downcast_ref::<NameIndexUnavailable>().is_some(),
        "{error}"
    );
}

#[test]
fn test_the_index_reports_how_far_it_has_got() {
    let worktree = WorktreeId::from_usize;
    assert_eq!(index_building(&[]), None);
    assert_eq!(
        index_building(&[
            (
                worktree(1),
                SymbolIndexStatus::Ready {
                    stats: Default::default()
                }
            ),
            (worktree(2), SymbolIndexStatus::Scanning),
        ]),
        None
    );
    assert_eq!(
        index_building(&[
            (
                worktree(1),
                SymbolIndexStatus::Indexing {
                    done: 12_000,
                    total: 50_000,
                }
            ),
            (worktree(2), SymbolIndexStatus::Disabled),
            (
                worktree(3),
                SymbolIndexStatus::Indexing {
                    done: 345,
                    total: 10_000,
                }
            ),
        ]),
        Some(IndexBuilding {
            done: 12_345,
            total: 60_000,
        })
    );
}

fn index_place(fixture: &Fixture, path: &str, row: u32) -> FoundCall {
    FoundCall {
        name: format!("function_in_{}", path.replace(['/', '.'], "_")),
        kind: "function".to_string(),
        place: FoundPlace::Index {
            path: project_path(fixture, path),
            name_row: row,
            end_row: row,
            call_rows: vec![row],
            preview_row: row,
        },
        source: CallSource::NameIndex,
        handle: CallHandle {
            language_server: None,
            index: Some(IndexHandle {
                name: "function".to_string(),
                path: project_path(fixture, path),
                position: Point::new(row, 0),
                is_static: false,
            }),
        },
        ambiguous: None,
        resolution: CallResolution::Definition,
        candidates: Vec::new(),
    }
}

fn is_open(fixture: &Fixture, path: &str, cx: &TestAppContext) -> bool {
    let path = project_path(fixture, path);
    fixture.project.read_with(cx, |project, cx| {
        project.get_open_buffer(&path, cx).is_some()
    })
}

/// Every file opened from now on, even one closed again right after.
fn record_opened_files(
    project: &Entity<Project>,
    cx: &mut TestAppContext,
) -> (Arc<Mutex<Vec<String>>>, gpui::Subscription) {
    let opened = Arc::new(Mutex::new(Vec::new()));
    let buffer_store = project.read_with(cx, |project, _| project.buffer_store().clone());
    let subscription = cx.update(|cx| {
        cx.subscribe(&buffer_store, {
            let opened = opened.clone();
            move |_, event, cx| {
                if let BufferStoreEvent::BufferAdded(buffer) = event
                    && let Some(file) = buffer.read(cx).file()
                {
                    opened
                        .lock()
                        .expect("not poisoned")
                        .push(file.path().as_unix_str().to_string());
                }
            }
        })
    });
    (opened, subscription)
}

/// Whatever the name index hands back goes through the same checks as the language server's
/// results. A function in a private file, an excluded file, or a file behind a link out of the
/// project is placed without its preview (and, for the first two, its name), and its file is
/// never opened to quote it: the check comes before the file is opened. The allowed one in the
/// same batch is quoted.
///
/// The store already leaves such files out, so the index's results can only reach this through
/// a mistake in the store; the check here is what an agent relies on.
#[gpui::test]
async fn test_withheld_index_results_are_not_named_quoted_or_opened(cx: &mut TestAppContext) {
    init_test(cx);
    let fixture = fixture(cx).await;
    let places = [
        ("src/other.c", 6),
        ("secret.c", 0),
        ("excluded/gen.c", 0),
        ("link/linked.c", 0),
    ];
    for (path, _) in places {
        assert!(!is_open(&fixture, path, cx), "{path} starts closed");
    }

    let (opened, _subscription) = record_opened_files(&fixture.project, cx);
    let mut async_cx = cx.to_async();
    let deadline = cx.executor().now() + CALL_HIERARCHY_TIME_BUDGET;
    let mut walk = Walk::new(
        fixture.project.clone(),
        options(CallDirection::Outgoing, 1),
        deadline,
        None,
        &async_cx,
    );
    let found = places
        .iter()
        .map(|(path, row)| index_place(&fixture, path, *row))
        .collect::<Vec<_>>();
    let mut built = fixture.project.read_with(cx, |project, cx| {
        found
            .iter()
            .map(|found| {
                let built = call_node(project, found, CallDirection::Outgoing, false, cx)
                    .expect("every place is a file");
                (built.node, built.preview_row)
            })
            .collect::<Vec<_>>()
    });
    assert_eq!(
        built.iter().map(|(_, row)| *row).collect::<Vec<_>>(),
        [Some(6), None, None, None],
        "only the allowed file waits to be quoted"
    );
    // The filling checks again, so a withheld node that asks for a quote still gets none.
    let pending = built
        .iter_mut()
        .map(|(node, _)| (node, 0))
        .collect::<Vec<_>>();
    walk.fill_previews(pending, &mut async_cx).await;

    assert_eq!(
        built
            .iter()
            .flat_map(|(node, _)| outline(node))
            .collect::<Vec<_>>(),
        [
            "function_in_src_other_c src/other.c:7-7 [7] \"static int helper(int value) {\" DepthLimit [name]",
            "- secret.c:1-1 [1] \"\" DepthLimit [name] withheld",
            "- excluded/gen.c:1-1 [1] \"\" DepthLimit [name] withheld",
            "function_in_link_linked_c <outside>:1-1 [1] \"\" DepthLimit [name] withheld",
        ]
    );
    assert_eq!(
        *opened.lock().expect("not poisoned"),
        ["src/other.c"],
        "only the allowed file is opened"
    );
}

/// A withheld root found by the index is placed and nothing more: no question is asked about it.
#[gpui::test]
async fn test_a_withheld_index_root_is_not_followed(cx: &mut TestAppContext) {
    init_test(cx);
    let fixture = fixture(cx).await;
    let mut async_cx = cx.to_async();
    let deadline = cx.executor().now() + CALL_HIERARCHY_TIME_BUDGET;
    let index = IndexAccess::new(&fixture.project, &async_cx).expect("the index can answer");
    let mut walk = Walk::new(
        fixture.project.clone(),
        options(CallDirection::Incoming, 3),
        deadline,
        Some(index),
        &async_cx,
    );
    let (opened, _subscription) = record_opened_files(&fixture.project, cx);
    let hierarchy = walk
        .run(index_place(&fixture, "secret.c", 0), &mut async_cx)
        .await
        .expect("the walk ends");
    assert_eq!(
        outline(&hierarchy.root),
        ["- secret.c:1-1 [1] \"\" Withheld [name] withheld"]
    );
    assert_eq!(
        walk.requests, 0,
        "nothing is asked about a withheld function"
    );
    assert!(opened.lock().expect("not poisoned").is_empty());
}

/// Quoting what the index found opens at most [`MAX_PREVIEW_FILES`] files; the rest are placed
/// without a preview, and files already open cost nothing.
#[gpui::test]
async fn test_quoting_opens_a_bounded_number_of_files(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    let files = (0..MAX_PREVIEW_FILES + 3)
        .map(|index| {
            (
                format!("file_{index:02}.c"),
                json!(format!("int f{index}(void);\n")),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    fs.insert_tree(path!("/root"), serde_json::Value::Object(files))
        .await;
    let project = Project::test(fs, [path!("/root").as_ref()], cx).await;
    cx.executor().run_until_parked();
    let worktree_id = project.read_with(cx, |project, cx| {
        project
            .visible_worktrees(cx)
            .next()
            .expect("a worktree")
            .read(cx)
            .id()
    });
    let fixture = Fixture {
        project: project.clone(),
        worktree_id,
        _index_directory: tempfile::tempdir().expect("a temporary directory"),
    };
    let last = format!("file_{:02}.c", MAX_PREVIEW_FILES + 2);
    let _already_open = project
        .update(cx, |project, cx| {
            project.open_buffer(project_path(&fixture, &last), cx)
        })
        .await
        .expect("an already open file");

    let mut async_cx = cx.to_async();
    let deadline = cx.executor().now() + CALL_HIERARCHY_TIME_BUDGET;
    let mut walk = Walk::new(
        project.clone(),
        options(CallDirection::Outgoing, 1),
        deadline,
        None,
        &async_cx,
    );
    let mut nodes = project.read_with(cx, |project, cx| {
        (0..MAX_PREVIEW_FILES + 3)
            .map(|index| {
                let found = index_place(&fixture, &format!("file_{index:02}.c"), 0);
                let built =
                    call_node(project, &found, CallDirection::Outgoing, false, cx).expect("a file");
                (built.node, built.preview_row)
            })
            .collect::<Vec<_>>()
    });
    walk.fill_previews(
        nodes
            .iter_mut()
            .filter_map(|(node, row)| Some((node, (*row)?)))
            .collect(),
        &mut async_cx,
    )
    .await;
    let quoted = nodes
        .iter()
        .map(|(node, _)| !node.preview.is_empty())
        .collect::<Vec<_>>();
    let mut expected = vec![true; MAX_PREVIEW_FILES];
    expected.extend([false, false, true]);
    assert_eq!(quoted, expected);
    assert_eq!(walk.files_opened, MAX_PREVIEW_FILES);
}

/// The questions to the name index count against the same limit as the language server's.
#[gpui::test]
async fn test_questions_to_the_index_count_as_requests(cx: &mut TestAppContext) {
    init_test(cx);
    let fixture = fixture(cx).await;
    let mut async_cx = cx.to_async();
    let deadline = cx.executor().now() + CALL_HIERARCHY_TIME_BUDGET;
    let index = IndexAccess::new(&fixture.project, &async_cx).expect("the index can answer");
    let mut walk = Walk::new(
        fixture.project.clone(),
        options(CallDirection::Outgoing, 1),
        deadline,
        Some(index),
        &async_cx,
    );
    walk.requests = MAX_CALL_HIERARCHY_REQUESTS - 2;
    let root = FoundCall {
        name: "top".to_string(),
        handle: CallHandle {
            language_server: None,
            index: Some(IndexHandle {
                name: "top".to_string(),
                path: project_path(&fixture, "src/main.c"),
                position: Point::new(7, 4),
                is_static: false,
            }),
        },
        ..index_place(&fixture, "src/main.c", 7)
    };
    let hierarchy = walk.run(root, &mut async_cx).await.expect("the walk ends");
    assert_eq!(walk.requests, MAX_CALL_HIERARCHY_REQUESTS);
    assert_eq!(hierarchy.stopped, Some(CallWalkStop::RequestLimit));
    // `callees` and the lookup of `middle` fit; `open`, `LOG` and `dup` did not.
    assert_eq!(
        outline(&hierarchy.root)[1..],
        [
            "  middle src/main.c:5-7 [9] \"int middle(void) {\" DepthLimit [name]",
            "  printf src/main.c:12-12 [12] \"printf(\\\"done\\\");\" DepthLimit [name] no-definition",
        ]
    );
    assert_eq!(hierarchy.omitted, 3);
}

/// The static rule reads the header extensions the store does.
#[test]
fn test_static_functions_of_headers_are_everyones() {
    let worktree_id = WorktreeId::from_usize(1);
    let path = |path: &str| ProjectPath {
        worktree_id,
        path: rel_path(path).into(),
    };
    let handle = |at: &str, is_static| IndexHandle {
        name: "f".to_string(),
        path: path(at),
        position: Point::zero(),
        is_static,
    };
    assert!(may_call_from(&handle("a.c", false), &path("b.c")));
    assert!(may_call_from(&handle("a.c", true), &path("a.c")));
    assert!(!may_call_from(&handle("a.c", true), &path("b.c")));
    assert!(may_call_from(&handle("a.h", true), &path("b.c")));
    assert!(may_call_from(&handle("a.hpp", true), &path("b.c")));
}

/// A name alone is a name: no spaces, no punctuation but the `::` of a C++ name (looked up
/// without its qualifier), and not too long. Anything else is refused before the index is asked.
#[gpui::test]
async fn test_a_name_alone_is_checked(cx: &mut TestAppContext) {
    init_test(cx);
    let fixture = fixture(cx).await;
    let name = |name: &str| CallHierarchyTarget::Name {
        name: name.to_string(),
        near: None,
    };

    let long_name = "a".repeat(MAX_CALL_NAME_CHARS + 1);
    for refused in [
        "",
        "  ",
        "struct ops",
        "middle()",
        "mid\ndle",
        "A::",
        "src/main.c",
        long_name.as_str(),
    ] {
        let error = ask(
            &fixture.project,
            name(refused),
            options(CallDirection::Incoming, 1),
            cx,
        )
        .await
        .expect_err("not a function's name");
        let message = error.to_string();
        assert!(message.contains("is not a function's name"), "{message}");
        assert!(!message.contains('\n'), "{message:?}");
        assert!(
            message.chars().count() < MAX_CALL_NAME_CHARS + 120,
            "the refusal quotes a bounded part of the input: {message}"
        );
    }

    for accepted in ["  middle ", "Ns::middle", "::middle", "A::B::middle"] {
        let hierarchy = found(
            ask(
                &fixture.project,
                name(accepted),
                options(CallDirection::Incoming, 1),
                cx,
            )
            .await
            .expect("the index answers"),
        );
        assert_eq!(hierarchy.root.name, "middle", "{accepted:?}");
        assert_eq!(hierarchy.root.children.len(), 2, "{accepted:?}");
    }
}

/// A language server can name a C++ function with its qualifier where the index uses its own
/// name; the walk still sees one function, so a function calling itself stops at once.
#[gpui::test]
async fn test_recursion_is_seen_across_the_two_sources(cx: &mut TestAppContext) {
    init_test(cx);
    let fixture = fixture(cx).await;
    let (server, _registration) = start_c_server(&fixture.project, true, cx).await;
    let qualified = lsp::CallHierarchyItem {
        name: "x::recurse".to_string(),
        kind: lsp::SymbolKind::FUNCTION,
        tags: None,
        detail: None,
        uri: lsp::Uri::from_file_path(path!("/root/src/main.c")).expect("an absolute path"),
        range: lsp::Range::new(lsp::Position::new(14, 0), lsp::Position::new(16, 1)),
        selection_range: lsp::Range::new(lsp::Position::new(14, 5), lsp::Position::new(14, 12)),
        data: None,
    };
    server.set_request_handler::<lsp::request::CallHierarchyPrepare, _, _>(move |_, _| {
        let qualified = qualified.clone();
        async move { Ok(Some(vec![qualified])) }
    });
    server.set_request_handler::<lsp::request::CallHierarchyIncomingCalls, _, _>(
        |_, _| async move { Ok(Some(Vec::new())) },
    );

    let hierarchy = found(
        ask(
            &fixture.project,
            at(&fixture, "src/main.c", 15, "recurse"),
            options(CallDirection::Incoming, 3),
            cx,
        )
        .await
        .expect("both answer"),
    );
    assert_eq!(
        outline(&hierarchy.root),
        [
            "x::recurse src/main.c:15-17 [] \"void recurse(void) {\" Expanded",
            "  recurse src/main.c:15-17 [16] \"recurse();\" Recursive [name]",
        ]
    );
}

/// The store answers for a worktree that is not part of the visible project (a library opened
/// alongside it); the agent's checks place its caller without a preview and don't follow it,
/// while the allowed caller next to it is followed. Its file is never opened.
#[gpui::test]
async fn test_an_index_caller_outside_the_project_is_not_followed(cx: &mut TestAppContext) {
    const VENDOR_C: &str = "int vendor_caller(void) {\n    return middle(); // hunter2\n}\nint vendor_top(void) {\n    return vendor_caller();\n}\n";
    init_test(cx);
    let fixture = fixture(cx).await;
    let fs = fixture
        .project
        .read_with(cx, |project, _| project.fs().as_fake());
    fs.insert_tree(path!("/vendor"), json!({ "vendor.c": VENDOR_C }))
        .await;
    let (worktree, _) = fixture
        .project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path!("/vendor"), false, cx)
        })
        .await
        .expect("an invisible worktree");
    cx.executor().run_until_parked();
    let vendor_id = worktree.read_with(cx, |worktree, _| worktree.id());
    let vendor_directory = tempfile::tempdir().expect("a temporary directory");
    let index = build_index_for_tests(vendor_directory.path(), &[("vendor.c", VENDOR_C)])
        .expect("building the index");
    fixture.project.update(cx, |project, cx| {
        project.symbol_index_store().update(cx, |store, cx| {
            store.insert_index_for_tests(vendor_id, index, cx)
        })
    });
    cx.executor().run_until_parked();

    let (opened, _subscription) = record_opened_files(&fixture.project, cx);
    let hierarchy = found(
        ask(
            &fixture.project,
            at(&fixture, "src/main.c", 5, "middle"),
            options(CallDirection::Incoming, 3),
            cx,
        )
        .await
        .expect("the index answers"),
    );
    assert_eq!(
        outline(&hierarchy.root),
        [
            "middle src/main.c:5-7 [] \"int middle(void) {\" Expanded [name]",
            "  top src/main.c:8-14 [9] \"middle();\" Expanded [name]",
            "  caller_of_middle src/other.c:4-6 [5] \"return middle() + helper(2);\" Expanded [name]",
            "    entry src/other.c:8-10 [9] \"return caller_of_middle();\" Expanded [name]",
            "  vendor_caller <outside>:1-3 [2] \"\" Withheld [name] withheld",
        ]
    );
    let text = format!("{hierarchy:?}");
    assert!(!text.contains("hunter2"), "{text}");
    assert!(!text.contains("vendor_top"), "{text}");
    assert!(
        !opened
            .lock()
            .expect("not poisoned")
            .iter()
            .any(|path| path == "vendor.c"),
        "the file outside the project is not opened to quote it"
    );
}

/// More call sites of a name than one answer of the store carries: the tree says it may miss
/// callers.
#[gpui::test]
async fn test_a_truncated_answer_of_the_index_is_reported(cx: &mut TestAppContext) {
    init_test(cx);
    let (project, worktree_id) = project_without_index(cx).await;
    let mut busy = String::from("int busy(void) {\n");
    for _ in 0..=MAX_CALLERS_LIMIT {
        busy.push_str("    middle();\n");
    }
    busy.push_str("}\n");
    let fs = project.read_with(cx, |project, _| project.fs().as_fake());
    fs.insert_tree(path!("/root/src"), json!({ "busy.c": busy.clone() }))
        .await;
    cx.executor().run_until_parked();
    let index_directory = tempfile::tempdir().expect("a temporary directory");
    let index = build_index_for_tests(
        index_directory.path(),
        &[("src/main.c", MAIN_C), ("src/busy.c", &busy)],
    )
    .expect("building the index");
    project.update(cx, |project, cx| {
        project.symbol_index_store().update(cx, |store, cx| {
            store.insert_index_for_tests(worktree_id, index, cx)
        })
    });
    cx.executor().run_until_parked();
    let fixture = Fixture {
        project: project.clone(),
        worktree_id,
        _index_directory: index_directory,
    };

    let hierarchy = found(
        ask(
            &project,
            at(&fixture, "src/main.c", 5, "middle"),
            options(CallDirection::Incoming, 1),
            cx,
        )
        .await
        .expect("the index answers"),
    );
    assert!(hierarchy.index_truncated);
    let busy_node = hierarchy
        .root
        .children
        .iter()
        .find(|child| child.name == "busy")
        .expect("busy is listed");
    assert!(busy_node.call_lines.len() <= MAX_CALLERS_LIMIT);

    let hierarchy = found(
        ask(
            &project,
            at(&fixture, "src/main.c", 2, "helper"),
            options(CallDirection::Incoming, 1),
            cx,
        )
        .await
        .expect("the index answers"),
    );
    assert!(!hierarchy.index_truncated, "a short answer is not marked");
}

/// A project whose index also holds `extra` (each written to the project too).
async fn fixture_with(cx: &mut TestAppContext, extra: &[(&str, &str)]) -> Fixture {
    let (project, worktree_id) = project_without_index(cx).await;
    let fs = project.read_with(cx, |project, _| project.fs().as_fake());
    for (path, text) in extra {
        fs.insert_file(
            std::path::Path::new(path!("/root")).join(path),
            text.as_bytes().to_vec(),
        )
        .await;
    }
    cx.executor().run_until_parked();
    let index_directory = tempfile::tempdir().expect("a temporary directory");
    let mut files = vec![
        ("src/main.c", MAIN_C),
        ("src/other.c", OTHER_C),
        ("src/third.c", THIRD_C),
    ];
    files.extend(extra.iter().copied());
    let index = build_index_for_tests(index_directory.path(), &files).expect("building the index");
    project.update(cx, |project, cx| {
        project.symbol_index_store().update(cx, |store, cx| {
            store.insert_index_for_tests(worktree_id, index, cx)
        })
    });
    cx.executor().run_until_parked();
    Fixture {
        project,
        worktree_id,
        _index_directory: index_directory,
    }
}

const MANY_DEFINITIONS: usize = MAX_CALL_CANDIDATES + 5;
const MANY_STORES: usize = MAX_INDIRECT_CANDIDATES + 2;

/// `many` is defined [`MANY_DEFINITIONS`] times; `slot` has [`MANY_STORES`] functions stored
/// into it. `both` calls two functions that each call `many` and go through `slot`.
fn wide_file() -> (String, u32) {
    let mut text = String::new();
    for _ in 0..MANY_DEFINITIONS {
        text.push_str("int many(void) { return 0; }\n");
    }
    for index in 0..MANY_STORES {
        text.push_str(&format!("int stored_{index}(int v) {{ return v; }}\n"));
        text.push_str(&format!(
            "struct ops table_{index} = {{ .slot = stored_{index} }};\n"
        ));
    }
    text.push_str("void use_many(struct ops *ops) {\n    many();\n    ops->slot(1);\n}\n");
    text.push_str("void use_again(struct ops *ops) {\n    many();\n    ops->slot(2);\n}\n");
    let both_row = text.lines().count() as u32;
    text.push_str("void both(struct ops *ops) {\n    use_many(ops);\n    use_again(ops);\n}\n");
    (text, both_row)
}

/// A name with more functions than are listed: [`MAX_CALL_CANDIDATES`] candidates, and the
/// total.
#[gpui::test]
async fn test_candidates_are_capped(cx: &mut TestAppContext) {
    init_test(cx);
    let (wide, _) = wide_file();
    let fixture = fixture_with(cx, &[("src/wide.c", &wide)]).await;
    let answer = ask(
        &fixture.project,
        CallHierarchyTarget::Name {
            name: "many".to_string(),
            near: None,
        },
        options(CallDirection::Incoming, 1),
        cx,
    )
    .await
    .expect("the index answers");
    let CallHierarchyAnswer::Candidates(candidates) = answer else {
        panic!("expected candidates, got {answer:?}");
    };
    assert_eq!(candidates.counts.shown, MANY_DEFINITIONS);
    assert_eq!(candidates.candidates.len(), MAX_CALL_CANDIDATES);
}

/// Each called name lists at most [`MAX_AMBIGUOUS_DEFINITIONS`] of its definitions and each
/// member at most [`MAX_INDIRECT_CANDIDATES`] of the functions stored into it; a name or member
/// looked up once is not asked about again in the same walk.
#[gpui::test]
async fn test_outgoing_lists_are_capped_and_lookups_are_kept(cx: &mut TestAppContext) {
    init_test(cx);
    let (wide, both_row) = wide_file();
    let fixture = fixture_with(cx, &[("src/wide.c", &wide)]).await;
    let mut async_cx = cx.to_async();
    let deadline = cx.executor().now() + CALL_HIERARCHY_TIME_BUDGET;
    let index = IndexAccess::new(&fixture.project, &async_cx).expect("the index can answer");
    let mut walk = Walk::new(
        fixture.project.clone(),
        options(CallDirection::Outgoing, 2),
        deadline,
        Some(index),
        &async_cx,
    );
    let root = FoundCall {
        name: "both".to_string(),
        handle: CallHandle {
            language_server: None,
            index: Some(IndexHandle {
                name: "both".to_string(),
                path: project_path(&fixture, "src/wide.c"),
                position: Point::new(both_row, 5),
                is_static: false,
            }),
        },
        ..index_place(&fixture, "src/wide.c", both_row)
    };
    let hierarchy = walk.run(root, &mut async_cx).await.expect("the walk ends");
    assert_eq!(hierarchy.stopped, None);
    let names = hierarchy
        .root
        .children
        .iter()
        .map(|child| child.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(names, ["use_many", "use_again"]);
    for child in &hierarchy.root.children {
        let many = child
            .children
            .iter()
            .filter(|call| call.name == "many")
            .collect::<Vec<_>>();
        assert_eq!(many.len(), MAX_AMBIGUOUS_DEFINITIONS, "{}", child.name);
        assert!(
            many.iter()
                .all(|call| call.ambiguous.as_ref().map(|counts| counts.shown)
                    == Some(MANY_DEFINITIONS)),
            "{}",
            child.name
        );
        let slot = child
            .children
            .iter()
            .find(|call| matches!(&call.resolution, CallResolution::Indirect { member } if member == "slot"))
            .expect("the call through the member");
        assert_eq!(
            slot.children.len(),
            MAX_INDIRECT_CANDIDATES,
            "{}",
            child.name
        );
    }
    // The callees of `both`, `use_many` and `use_again`; the definitions of `use_many`,
    // `use_again` and `many`; the stores into `slot`. `many` and `slot` come up again under
    // `use_again` and are not asked about twice.
    assert_eq!(walk.requests, 7);
}

/// The language server knows one of two functions of the same name on neighbouring lines; the
/// index knows both. Only the one on the same line is the language server's: the other stays a
/// function of its own.
#[gpui::test]
async fn test_only_the_same_line_merges_with_the_language_server(cx: &mut TestAppContext) {
    const PAIR_C: &str =
        "int target(void) { return 0; }\nvoid g(void) { target(); }\nvoid g(void) { target(); }\n";
    init_test(cx);
    let fixture = fixture_with(cx, &[("src/pair.c", PAIR_C)]).await;
    let (server, _registration) = start_c_server(&fixture.project, true, cx).await;
    let item = |name: &str, row: u32, column: u32, end_column: u32| lsp::CallHierarchyItem {
        name: name.to_string(),
        kind: lsp::SymbolKind::FUNCTION,
        tags: None,
        detail: None,
        uri: lsp::Uri::from_file_path(path!("/root/src/pair.c")).expect("an absolute path"),
        range: lsp::Range::new(
            lsp::Position::new(row, 0),
            lsp::Position::new(row, end_column),
        ),
        selection_range: lsp::Range::new(
            lsp::Position::new(row, column),
            lsp::Position::new(row, column + name.len() as u32),
        ),
        data: None,
    };
    let target = item("target", 0, 4, 30);
    let first_g = item("g", 1, 5, 26);
    server.set_request_handler::<lsp::request::CallHierarchyPrepare, _, _>(move |_, _| {
        let target = target.clone();
        async move { Ok(Some(vec![target])) }
    });
    server.set_request_handler::<lsp::request::CallHierarchyIncomingCalls, _, _>(move |_, _| {
        let first_g = first_g.clone();
        async move {
            Ok(Some(vec![lsp::CallHierarchyIncomingCall {
                from: first_g,
                from_ranges: vec![lsp::Range::new(
                    lsp::Position::new(1, 15),
                    lsp::Position::new(1, 21),
                )],
            }]))
        }
    });

    let hierarchy = found(
        ask(
            &fixture.project,
            at(&fixture, "src/pair.c", 1, "target"),
            options(CallDirection::Incoming, 1),
            cx,
        )
        .await
        .expect("both answer"),
    );
    assert_eq!(
        outline(&hierarchy.root),
        [
            "target src/pair.c:1-1 [] \"int target(void) { return 0; }\" Expanded",
            "  g src/pair.c:2-2 [2] \"void g(void) { target(); }\" DepthLimit",
            "  g src/pair.c:3-3 [3] \"void g(void) { target(); }\" DepthLimit [name]",
        ]
    );
}

/// Definitions of a name in files withheld from the agent are counted apart from the ones that
/// may be shown, so `ambiguous` and the candidates' total say how many the agent can look at.
#[gpui::test]
async fn test_definitions_in_withheld_files_are_counted_apart(cx: &mut TestAppContext) {
    init_test(cx);
    let fixture = fixture(cx).await;
    let definition = |path: &str, row: u32| IndexedDefinition {
        name: "dup".to_string(),
        kind: DefinitionKind::Function,
        is_static: false,
        is_declaration: false,
        language: symbol_index::SourceLanguage::C,
        path: project_path(&fixture, path),
        name_range: Point::new(row, 4)..Point::new(row, 7),
        item_range: Point::new(row, 0)..Point::new(row, 27),
    };
    let definitions = [
        definition("src/other.c", 6),
        definition("src/third.c", 0),
        definition("src/third.c", 3),
        definition("secret.c", 0),
        definition("excluded/gen.c", 0),
        definition("link/linked.c", 0),
    ];
    let counts = fixture.project.read_with(cx, |project, cx| {
        DefinitionCounts::of(project, &definitions, cx)
    });
    assert_eq!(
        counts,
        DefinitionCounts {
            shown: 3,
            withheld: 3,
            shown_files: vec![
                (project_path(&fixture, "src/other.c"), 1),
                (project_path(&fixture, "src/third.c"), 2),
            ],
        }
    );
    assert_eq!(counts.total(), 6);
}
