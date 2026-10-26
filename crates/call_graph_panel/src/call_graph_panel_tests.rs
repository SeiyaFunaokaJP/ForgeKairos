use super::*;
use futures::{StreamExt as _, channel::oneshot};
use gpui::{TestAppContext, VisualTestContext};
use language::{FakeLspAdapter, Language, LanguageConfig, LanguageMatcher, LanguageName, Point};
use project::FakeFs;
use serde_json::json;
use settings::SettingsStore;
use std::{
    collections::HashMap as StdHashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use util::{path, rel_path::rel_path};
use workspace::{AppState, MultiWorkspace};

/// A call as the fake language server reports it.
#[derive(Clone)]
struct FakeCall {
    name: &'static str,
    file: &'static str,
    line: u32,
    /// Call sites as (line, start column, end column).
    sites: Vec<(u32, u32, u32)>,
}

fn call(name: &'static str, file: &'static str, line: u32) -> FakeCall {
    FakeCall {
        name,
        file,
        line,
        sites: Vec::new(),
    }
}

/// Answers of the fake server, keyed by the name of the function asked about.
#[derive(Default)]
struct FakeGraph {
    incoming: StdHashMap<&'static str, Vec<FakeCall>>,
    outgoing: StdHashMap<&'static str, Vec<FakeCall>>,
    /// Names the server was asked about, in order, with the direction.
    requests: Vec<(CallDirection, String)>,
    /// How many incoming requests still fail before the server answers.
    failures_left: usize,
    /// Whether prepare requests find nothing, as while a server is indexing.
    prepare_finds_nothing: bool,
    /// How many prepare requests the server got.
    prepares: usize,
    /// Prepare requests wait for this before answering.
    held_prepare: Option<oneshot::Receiver<()>>,
    /// Incoming requests about these names wait before answering.
    held_incoming: StdHashMap<&'static str, oneshot::Receiver<()>>,
    /// The functions prepare requests find, as (file, line, name). When
    /// empty, `main.rs` has the functions of `MAIN_SOURCE`.
    prepare_items: Vec<(&'static str, u32, &'static str)>,
}

struct Harness {
    workspace: Entity<Workspace>,
    project: Entity<Project>,
    panel: Entity<CallGraphPanel>,
    editor: Entity<Editor>,
    graph: Arc<Mutex<FakeGraph>>,
    requests_answered: Arc<AtomicUsize>,
    worktree_id: project::WorktreeId,
}

const MAIN_SOURCE: &str = "fn main() { helper(); helper(); }\nfn helper() { leaf(); }\nfn leaf() {}\nfn a() { b(); }\nfn b() { a(); }\nfn c1() {}\nfn c2() {}\nfn c3() {}\nfn c4() {}\nfn c5() {}\n";
const OTHER_SOURCE: &str = "fn other_caller() { helper(); }\nfn other_second() {}\n";
const THIRD_SOURCE: &str = "fn third_caller() { other_caller(); }\n";

fn uri_for(file: &str) -> lsp::Uri {
    lsp::Uri::from_file_path(std::path::Path::new(path!("/test/src")).join(file))
        .expect("a valid test path")
}

fn lsp_item(name: &str, file: &str, line: u32) -> lsp::CallHierarchyItem {
    lsp::CallHierarchyItem {
        name: name.to_string(),
        kind: lsp::SymbolKind::FUNCTION,
        tags: None,
        detail: Some(format!("fn {name}()")),
        uri: uri_for(file),
        range: lsp::Range {
            start: lsp::Position { line, character: 0 },
            end: lsp::Position {
                line,
                character: 10,
            },
        },
        selection_range: lsp::Range {
            start: lsp::Position { line, character: 3 },
            end: lsp::Position {
                line,
                character: 3 + name.len() as u32,
            },
        },
        data: None,
    }
}

fn lsp_ranges(sites: &[(u32, u32, u32)]) -> Vec<lsp::Range> {
    sites
        .iter()
        .map(|(line, start, end)| lsp::Range {
            start: lsp::Position {
                line: *line,
                character: *start,
            },
            end: lsp::Position {
                line: *line,
                character: *end,
            },
        })
        .collect()
}

/// The function each line of `MAIN_SOURCE` defines, for prepare requests.
fn main_function_at(line: u32) -> Option<&'static str> {
    [
        "main", "helper", "leaf", "a", "b", "c1", "c2", "c3", "c4", "c5",
    ]
    .get(line as usize)
    .copied()
}

fn init_test(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let _state = AppState::test(cx);
        editor::init(cx);
        crate::init(cx);
    });
}

fn rust_lang() -> Arc<Language> {
    Arc::new(Language::new(
        LanguageConfig {
            name: LanguageName::new("Rust"),
            matcher: Arc::new(LanguageMatcher {
                path_suffixes: vec!["rs".into()],
                ..LanguageMatcher::default()
            }),
            ..LanguageConfig::default()
        },
        Some(tree_sitter_rust::LANGUAGE.into()),
    ))
}

async fn setup(graph: FakeGraph, cx: &mut TestAppContext) -> (Harness, &mut VisualTestContext) {
    setup_project(
        json!({
            "main.rs": MAIN_SOURCE,
            "other.rs": OTHER_SOURCE,
            "third.rs": THIRD_SOURCE,
        }),
        "src/main.rs",
        rust_lang(),
        true,
        graph,
        cx,
    )
    .await
}

/// A project with `files` in `/test/src`, `open` open in an editor, and, with
/// `with_server`, a fake language server for `language` that answers from `graph`.
async fn setup_project<'a>(
    files: serde_json::Value,
    open: &str,
    language: Arc<Language>,
    with_server: bool,
    graph: FakeGraph,
    cx: &'a mut TestAppContext,
) -> (Harness, &'a mut VisualTestContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/test"), json!({ "src": files }))
        .await;
    let project = Project::test(fs, [path!("/test").as_ref()], cx).await;
    let language_name = language.name();
    project.read_with(cx, |project, _| project.languages().add(language));
    let mut fake_servers = with_server.then(|| {
        project.read_with(cx, |project, _| {
            project.languages().register_fake_lsp(
                language_name.clone(),
                FakeLspAdapter {
                    capabilities: lsp::ServerCapabilities {
                        call_hierarchy_provider: Some(lsp::CallHierarchyServerCapability::Simple(
                            true,
                        )),
                        ..lsp::ServerCapabilities::default()
                    },
                    ..FakeLspAdapter::default()
                },
            )
        })
    });

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace =
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
    let worktree_id = project.read_with(cx, |project, cx| {
        project
            .worktrees(cx)
            .next()
            .expect("the test worktree")
            .read(cx)
            .id()
    });
    let editor = workspace
        .update_in(cx, |workspace, window, cx| {
            workspace.open_path((worktree_id, rel_path(open)), None, true, window, cx)
        })
        .await
        .expect("the file opens")
        .downcast::<Editor>()
        .expect("the file opens in an editor");
    let panel = workspace.update_in(cx, |workspace, window, cx| {
        let panel = CallGraphPanel::new(workspace, window, cx);
        workspace.add_panel(panel.clone(), window, cx);
        panel
    });

    let graph = Arc::new(Mutex::new(graph));
    let requests_answered = Arc::new(AtomicUsize::new(0));
    let harness = Harness {
        workspace,
        project,
        panel,
        editor,
        graph: graph.clone(),
        requests_answered: requests_answered.clone(),
        worktree_id,
    };
    let Some(fake_servers) = fake_servers.as_mut() else {
        cx.executor().run_until_parked();
        return (harness, cx);
    };
    let fake_server = fake_servers.next().await.expect("the fake server starts");
    cx.executor().run_until_parked();
    fake_server.set_request_handler::<lsp::request::CallHierarchyPrepare, _, _>({
        let graph = graph.clone();
        move |params, _| {
            let graph = graph.clone();
            async move {
                let held = {
                    let mut graph = graph.lock().expect("graph lock");
                    graph.prepares += 1;
                    graph.held_prepare.take()
                };
                if let Some(held) = held {
                    held.await.ok();
                }
                let graph = graph.lock().expect("graph lock");
                if graph.prepare_finds_nothing {
                    return Ok(None);
                }
                let line = params.text_document_position_params.position.line;
                let file = params
                    .text_document_position_params
                    .text_document
                    .uri
                    .to_file_path()
                    .ok()
                    .and_then(|path| Some(path.file_name()?.to_string_lossy().into_owned()))
                    .unwrap_or_default();
                if graph.prepare_items.is_empty() {
                    return Ok(
                        main_function_at(line).map(|name| vec![lsp_item(name, "main.rs", line)])
                    );
                }
                Ok(graph
                    .prepare_items
                    .iter()
                    .find(|(item_file, item_line, _)| *item_file == file && *item_line == line)
                    .map(|(item_file, item_line, name)| {
                        vec![lsp_item(name, item_file, *item_line)]
                    }))
            }
        }
    });
    fake_server.set_request_handler::<lsp::request::CallHierarchyIncomingCalls, _, _>({
        let graph = graph.clone();
        let requests_answered = requests_answered.clone();
        move |params, _| {
            let graph = graph.clone();
            let requests_answered = requests_answered.clone();
            async move {
                let held = graph
                    .lock()
                    .expect("graph lock")
                    .held_incoming
                    .remove(params.item.name.as_str());
                if let Some(held) = held {
                    held.await.ok();
                }
                let mut graph = graph.lock().expect("graph lock");
                graph
                    .requests
                    .push((CallDirection::Incoming, params.item.name.clone()));
                requests_answered.fetch_add(1, Ordering::SeqCst);
                if graph.failures_left > 0 {
                    graph.failures_left -= 1;
                    return Err(anyhow::anyhow!("the server is busy"));
                }
                let calls = graph
                    .incoming
                    .get(params.item.name.as_str())
                    .cloned()
                    .unwrap_or_default();
                Ok(Some(
                    calls
                        .into_iter()
                        .map(|call| lsp::CallHierarchyIncomingCall {
                            from: lsp_item(call.name, call.file, call.line),
                            from_ranges: lsp_ranges(&call.sites),
                        })
                        .collect(),
                ))
            }
        }
    });
    fake_server.set_request_handler::<lsp::request::CallHierarchyOutgoingCalls, _, _>({
        let graph = graph.clone();
        let requests_answered = requests_answered.clone();
        move |params, _| {
            let graph = graph.clone();
            let requests_answered = requests_answered.clone();
            async move {
                let mut graph = graph.lock().expect("graph lock");
                graph
                    .requests
                    .push((CallDirection::Outgoing, params.item.name.clone()));
                requests_answered.fetch_add(1, Ordering::SeqCst);
                let calls = graph
                    .outgoing
                    .get(params.item.name.as_str())
                    .cloned()
                    .unwrap_or_default();
                Ok(Some(
                    calls
                        .into_iter()
                        .map(|call| lsp::CallHierarchyOutgoingCall {
                            to: lsp_item(call.name, call.file, call.line),
                            from_ranges: lsp_ranges(&call.sites),
                        })
                        .collect(),
                ))
            }
        }
    });

    (harness, cx)
}

impl Harness {
    fn move_cursor(&self, line: u32, cx: &mut VisualTestContext) {
        self.editor.update_in(cx, |editor, window, cx| {
            editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
                selections.select_ranges([Point::new(line, 4)..Point::new(line, 4)])
            });
            window.focus(&editor.focus_handle(cx), cx);
        });
    }

    fn show_callers_at(&self, line: u32, cx: &mut VisualTestContext) {
        self.move_cursor(line, cx);
        cx.dispatch_action(ShowCallers);
        cx.executor().run_until_parked();
    }

    fn requests(&self) -> Vec<(CallDirection, String)> {
        self.graph.lock().expect("graph lock").requests.clone()
    }

    fn requests_answered(&self) -> usize {
        self.requests_answered.load(Ordering::SeqCst)
    }

    fn rows(&self, cx: &mut VisualTestContext) -> Vec<String> {
        self.panel.read_with(cx, |panel, _| describe_rows(panel))
    }

    fn select(&self, name: &str, cx: &mut VisualTestContext) {
        self.panel.update(cx, |panel, cx| {
            let index = panel.rows.iter().position(|row| match row {
                Row::Node { id, .. } => panel
                    .tree
                    .node(*id)
                    .is_some_and(|node| node.symbol.name == name),
                _ => false,
            });
            let index = index.unwrap_or_else(|| panic!("no row named {name}"));
            panel.select_index(index, cx);
        });
    }

    fn select_row(&self, index: usize, cx: &mut VisualTestContext) {
        self.panel
            .update(cx, |panel, cx| panel.select_index(index, cx));
    }

    fn selected_name(&self, cx: &mut VisualTestContext) -> Option<SharedString> {
        self.panel.read_with(cx, |panel, _| {
            panel
                .selected
                .and_then(|row| panel.tree.node(row.node()))
                .map(|node| node.symbol.name.clone())
        })
    }

    fn dispatch(&self, action: impl Action, cx: &mut VisualTestContext) {
        self.panel.update_in(cx, |panel, window, cx| {
            window.focus(&panel.focus_handle, cx);
        });
        cx.dispatch_action(action);
        cx.executor().run_until_parked();
    }
}

/// The rows as text: two spaces per level, the name, then markers in brackets.
fn describe_rows(panel: &CallGraphPanel) -> Vec<String> {
    panel
        .rows
        .iter()
        .map(|row| {
            let indent = "  ".repeat(row.depth());
            match row {
                Row::Node { id, .. } => {
                    let Some(node) = panel.tree.node(*id) else {
                        return format!("{indent}<missing>");
                    };
                    let symbol = &node.symbol;
                    let mut text = match symbol.role() {
                        SymbolRole::Function => format!("{indent}{}", symbol.name),
                        SymbolRole::SameNameDefinitions(count) => {
                            format!("{indent}{} [{count} definitions]", symbol.name)
                        }
                        SymbolRole::IndirectCall => format!("{indent}indirect .{}", symbol.name),
                        SymbolRole::Unresolved => {
                            format!("{indent}{} [no definition]", symbol.name)
                        }
                        SymbolRole::FileScope => format!("{indent}(file scope)"),
                    };
                    if symbol.role() == SymbolRole::Function {
                        match symbol.origin {
                            CallOrigin::LanguageServer => {}
                            CallOrigin::NameMatch => text.push_str(" [name match]"),
                            CallOrigin::FunctionPointerCandidate => {
                                text.push_str(" [pointer candidate]")
                            }
                            CallOrigin::Macro => text.push_str(" [macro]"),
                        }
                    }
                    if node.recursive {
                        text.push_str(" [recursive]");
                    }
                    if node.is_loading() {
                        text.push_str(" [loading]");
                    }
                    if node.symbol.site_count() > 1 {
                        text.push_str(&format!(" [x{}]", node.symbol.site_count()));
                    }
                    text
                }
                Row::Empty { .. } => format!("{indent}[empty]"),
                Row::Error { .. } => format!("{indent}[error]"),
                Row::ShowMore { remaining, .. } => format!("{indent}[{remaining} more]"),
                Row::Excluded { count, .. } => format!("{indent}[{count} excluded]"),
                Row::Truncated { .. } => format!("{indent}[truncated]"),
            }
        })
        .collect()
}

fn graph_with_incoming(incoming: Vec<(&'static str, Vec<FakeCall>)>) -> FakeGraph {
    FakeGraph {
        incoming: incoming.into_iter().collect(),
        ..FakeGraph::default()
    }
}

#[gpui::test]
async fn test_expanding_asks_once_and_collapsing_forgets(cx: &mut TestAppContext) {
    let (harness, cx) = setup(
        graph_with_incoming(vec![
            ("helper", vec![call("main", "main.rs", 0)]),
            (
                "main",
                vec![call("c1", "main.rs", 5), call("c2", "main.rs", 6)],
            ),
        ]),
        cx,
    )
    .await;

    harness.show_callers_at(1, cx);
    assert_eq!(harness.rows(cx), vec!["helper", "  main"]);
    assert_eq!(
        harness.requests(),
        vec![(CallDirection::Incoming, "helper".to_string())],
        "setting the root asks for its callers, and for nothing else"
    );

    harness.select("main", cx);
    harness.dispatch(ExpandSelected, cx);
    assert_eq!(
        harness.rows(cx),
        vec!["helper", "  main", "    c1", "    c2"]
    );
    assert_eq!(harness.requests_answered(), 2);

    // Expanding what is already expanded moves into it instead of asking again.
    harness.select("main", cx);
    harness.dispatch(ExpandSelected, cx);
    assert_eq!(harness.requests_answered(), 2);
    assert_eq!(harness.selected_name(cx).as_deref(), Some("c1"));

    harness.select("main", cx);
    harness.dispatch(CollapseSelected, cx);
    assert_eq!(harness.rows(cx), vec!["helper", "  main"]);
    assert_eq!(
        harness.panel.read_with(cx, |panel, _| panel.tree.len()),
        2,
        "collapsing drops the children"
    );

    // Collapsing a collapsed row moves to its parent.
    harness.dispatch(CollapseSelected, cx);
    assert_eq!(harness.selected_name(cx).as_deref(), Some("helper"));

    harness.select("main", cx);
    harness.dispatch(ExpandSelected, cx);
    assert_eq!(
        harness.rows(cx),
        vec!["helper", "  main", "    c1", "    c2"]
    );
    assert_eq!(harness.requests_answered(), 3, "expanding again asks again");

    harness.dispatch(CollapseAll, cx);
    assert_eq!(harness.rows(cx), vec!["helper", "  main"]);
}

#[gpui::test]
async fn test_recursion_is_marked_and_not_expanded(cx: &mut TestAppContext) {
    let (harness, cx) = setup(
        graph_with_incoming(vec![
            ("a", vec![call("b", "main.rs", 4)]),
            ("b", vec![call("a", "main.rs", 3)]),
            ("leaf", vec![call("leaf", "main.rs", 2)]),
        ]),
        cx,
    )
    .await;

    harness.show_callers_at(3, cx);
    harness.select("b", cx);
    harness.dispatch(ExpandSelected, cx);
    assert_eq!(harness.rows(cx), vec!["a", "  b", "    a [recursive]"]);

    let before = harness.requests_answered();
    harness.select_row(2, cx);
    harness.dispatch(ExpandSelected, cx);
    assert_eq!(harness.rows(cx), vec!["a", "  b", "    a [recursive]"]);
    assert_eq!(
        harness.requests_answered(),
        before,
        "a recursive row is never expanded"
    );

    harness.show_callers_at(2, cx);
    assert_eq!(
        harness.rows(cx),
        vec!["leaf", "  leaf [recursive]"],
        "a function calling itself is marked right under the root"
    );
}

#[gpui::test]
async fn test_no_children_and_failure_with_retry(cx: &mut TestAppContext) {
    let mut graph = graph_with_incoming(vec![("helper", vec![call("main", "main.rs", 0)])]);
    graph.failures_left = 1;
    let (harness, cx) = setup(graph, cx).await;

    harness.show_callers_at(1, cx);
    assert_eq!(harness.rows(cx), vec!["helper", "  [error]"]);

    harness.select_row(1, cx);
    harness.dispatch(OpenSelected, cx);
    assert_eq!(harness.rows(cx), vec!["helper", "  main"]);

    harness.select("main", cx);
    harness.dispatch(ExpandSelected, cx);
    assert_eq!(harness.rows(cx), vec!["helper", "  main", "    [empty]"]);
}

#[gpui::test]
async fn test_show_more_past_the_limit(cx: &mut TestAppContext) {
    let (harness, cx) = setup(
        graph_with_incoming(vec![(
            "leaf",
            vec![
                call("c1", "main.rs", 5),
                call("c2", "main.rs", 6),
                call("c3", "main.rs", 7),
                call("c4", "main.rs", 8),
                call("c5", "main.rs", 9),
            ],
        )]),
        cx,
    )
    .await;
    cx.update(|_, cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |settings| {
                settings
                    .call_graph_panel
                    .get_or_insert_default()
                    .max_children = Some(2);
            });
        });
    });

    harness.show_callers_at(2, cx);
    assert_eq!(harness.rows(cx), vec!["leaf", "  c1", "  c2", "  [3 more]"]);

    harness.select_row(3, cx);
    harness.dispatch(OpenSelected, cx);
    assert_eq!(
        harness.rows(cx),
        vec!["leaf", "  c1", "  c2", "  c3", "  c4", "  [1 more]"]
    );
    harness.select_row(5, cx);
    harness.dispatch(OpenSelected, cx);
    assert_eq!(
        harness.rows(cx),
        vec!["leaf", "  c1", "  c2", "  c3", "  c4", "  c5"]
    );
    assert_eq!(harness.requests_answered(), 1, "showing more asks nothing");
}

#[gpui::test]
async fn test_filter_keeps_matches_and_their_ancestors(cx: &mut TestAppContext) {
    let (harness, cx) = setup(
        graph_with_incoming(vec![
            (
                "helper",
                vec![call("main", "main.rs", 0), call("c5", "main.rs", 9)],
            ),
            (
                "main",
                vec![call("c1", "main.rs", 5), call("c2", "main.rs", 6)],
            ),
        ]),
        cx,
    )
    .await;
    harness.show_callers_at(1, cx);
    harness.select("main", cx);
    harness.dispatch(ExpandSelected, cx);
    assert_eq!(
        harness.rows(cx),
        vec!["helper", "  main", "    c1", "    c2", "  c5"]
    );

    harness.panel.update_in(cx, |panel, window, cx| {
        panel
            .filter_editor
            .update(cx, |editor, cx| editor.set_text("C2", window, cx));
    });
    cx.executor().run_until_parked();
    assert_eq!(harness.rows(cx), vec!["helper", "  main", "    c2"]);
}

#[gpui::test]
async fn test_direction_toggle_keeps_root_and_history(cx: &mut TestAppContext) {
    let mut graph = graph_with_incoming(vec![("helper", vec![call("main", "main.rs", 0)])]);
    graph.outgoing = [("helper", vec![call("leaf", "main.rs", 2)])]
        .into_iter()
        .collect();
    let (harness, cx) = setup(graph, cx).await;

    harness.show_callers_at(1, cx);
    assert_eq!(harness.rows(cx), vec!["helper", "  main"]);

    harness.dispatch(ToggleDirection, cx);
    assert_eq!(harness.rows(cx), vec!["helper", "  leaf"]);
    assert_eq!(
        harness.panel.read_with(cx, |panel, _| panel.direction()),
        CallDirection::Outgoing
    );
    assert_eq!(
        harness.requests().last(),
        Some(&(CallDirection::Outgoing, "helper".to_string()))
    );

    harness.show_callers_at(2, cx);
    assert_eq!(
        harness.panel.read_with(cx, |panel, _| panel.direction()),
        CallDirection::Incoming,
        "ShowCallers switches back to callers"
    );
    assert_eq!(harness.rows(cx), vec!["leaf", "  [empty]"]);

    harness.dispatch(PreviousRoot, cx);
    assert_eq!(harness.rows(cx), vec!["helper", "  main"]);
    harness.dispatch(PreviousRoot, cx);
    assert_eq!(
        harness.rows(cx),
        vec!["helper", "  main"],
        "there is nothing before the first root"
    );
    harness.dispatch(NextRoot, cx);
    assert_eq!(harness.rows(cx), vec!["leaf", "  [empty]"]);

    // Asking for the same root again does not add a history entry.
    harness.show_callers_at(2, cx);
    harness.dispatch(PreviousRoot, cx);
    assert_eq!(harness.rows(cx), vec!["helper", "  main"]);
}

#[gpui::test]
async fn test_history_is_capped(cx: &mut TestAppContext) {
    let (harness, cx) = setup(FakeGraph::default(), cx).await;
    for index in 0..HISTORY_LIMIT + 5 {
        harness.show_callers_at((index % 2) as u32, cx);
        // Alternating roots are all distinct entries.
    }
    let history_len = harness.panel.read_with(cx, |panel, _| panel.history.len());
    assert_eq!(history_len, HISTORY_LIMIT);
}

#[gpui::test]
async fn test_opening_goes_to_each_call_site_in_turn(cx: &mut TestAppContext) {
    let (harness, cx) = setup(
        graph_with_incoming(vec![(
            "helper",
            vec![FakeCall {
                sites: vec![(0, 22, 28), (0, 12, 18)],
                ..call("main", "main.rs", 0)
            }],
        )]),
        cx,
    )
    .await;

    harness.show_callers_at(1, cx);
    assert_eq!(harness.rows(cx), vec!["helper", "  main [x2]"]);

    let cursor = |cx: &mut VisualTestContext| {
        let editor = harness
            .workspace
            .read_with(cx, |workspace, cx| workspace.active_item_as::<Editor>(cx))
            .expect("an editor is active");
        editor.update(cx, |editor, cx| {
            editor
                .selections
                .newest::<Point>(&editor.display_snapshot(cx))
                .head()
        })
    };

    harness.select("main", cx);
    harness.dispatch(OpenSelected, cx);
    assert_eq!(
        cursor(cx),
        Point::new(0, 12),
        "the earliest call site first"
    );
    harness.dispatch(OpenSelected, cx);
    assert_eq!(cursor(cx), Point::new(0, 22), "then the next one");
    harness.dispatch(OpenSelected, cx);
    assert_eq!(cursor(cx), Point::new(0, 12), "and around again");

    harness.move_cursor(2, cx);
    harness.dispatch(PreviewSelected, cx);
    assert_eq!(cursor(cx), Point::new(0, 22));
    let panel_focused = harness.panel.update_in(cx, |panel, window, cx| {
        panel.focus_handle.contains_focused(window, cx)
    });
    assert!(panel_focused, "previewing keeps the focus in the panel");
}

#[gpui::test]
async fn test_expanding_deeply_does_not_keep_buffers_open(cx: &mut TestAppContext) {
    let (harness, cx) = setup(
        graph_with_incoming(vec![
            ("helper", vec![call("other_caller", "other.rs", 0)]),
            ("other_caller", vec![call("third_caller", "third.rs", 0)]),
        ]),
        cx,
    )
    .await;
    let open_buffer_paths = |cx: &mut VisualTestContext| {
        let mut paths = harness.project.read_with(cx, |project, cx| {
            project
                .opened_buffers(cx)
                .into_iter()
                .filter_map(|buffer| {
                    buffer
                        .read(cx)
                        .file()
                        .map(|file| file.path().as_unix_str().to_string())
                })
                .collect::<Vec<_>>()
        });
        paths.sort();
        paths
    };
    assert_eq!(open_buffer_paths(cx), vec!["src/main.rs"]);

    harness.show_callers_at(1, cx);
    assert_eq!(
        open_buffer_paths(cx),
        vec!["src/main.rs"],
        "the tree does not keep the files it lists open"
    );
    harness.select("other_caller", cx);
    harness.dispatch(ExpandSelected, cx);
    assert_eq!(
        harness.rows(cx),
        vec!["helper", "  other_caller", "    third_caller"]
    );
    assert_eq!(
        harness.requests().last(),
        Some(&(CallDirection::Incoming, "other_caller".to_string())),
        "a caller in a file nobody has open can still be expanded"
    );
    assert_eq!(
        open_buffer_paths(cx),
        vec!["src/main.rs", "src/other.rs"],
        "the file opened to ask about a row stays open for a while, and the files of its answer do not"
    );
    advance(RECENT_BUFFER_LIFETIME, cx);
    assert_eq!(
        open_buffer_paths(cx),
        vec!["src/main.rs"],
        "and is released after a while"
    );

    harness.select("third_caller", cx);
    harness.dispatch(OpenSelected, cx);
    assert_eq!(
        open_buffer_paths(cx),
        vec!["src/main.rs", "src/third.rs"],
        "opening a row opens its file again"
    );
}

#[gpui::test]
async fn test_messages_are_translated(cx: &mut TestAppContext) {
    let (harness, cx) = setup(graph_with_incoming(vec![("leaf", Vec::new())]), cx).await;
    harness.show_callers_at(2, cx);
    assert_eq!(harness.rows(cx), vec!["leaf", "  [empty]"]);

    let _japanese = i18n::test_support::LocaleOverride::new(Some(
        i18n_catalog::test_support::bundled_catalog("ja"),
    ));
    assert_eq!(
        empty_children_label(CallDirection::Incoming),
        "呼び出し元はありません"
    );
    assert_eq!(
        empty_children_label(CallDirection::Outgoing),
        "呼び出し先はありません"
    );
    assert_eq!(direction_label(CallDirection::Incoming), "呼び出し元");
    assert_eq!(direction_label(CallDirection::Outgoing), "呼び出し先");
    assert_eq!(origin_label(CallOrigin::LanguageServer), "言語サーバー");
}

fn emit_work_end_without_waiting(project: &Entity<Project>, cx: &mut VisualTestContext) {
    let lsp_store = project.read_with(cx, |project, _| project.lsp_store());
    lsp_store.update(cx, |_, cx| {
        cx.emit(LspStoreEvent::LanguageServerUpdate {
            language_server_id: lsp::LanguageServerId(0),
            name: None,
            message: proto::update_language_server::Variant::WorkEnd(proto::LspWorkEnd {
                token: None,
            }),
        });
    });
    cx.executor().run_until_parked();
}

fn emit_work_end(project: &Entity<Project>, cx: &mut VisualTestContext) {
    emit_work_end_without_waiting(project, cx);
    cx.executor().advance_clock(INDEXING_RETRY_INTERVAL);
    cx.executor().run_until_parked();
}

#[gpui::test]
async fn test_asks_again_when_the_server_finishes_indexing(cx: &mut TestAppContext) {
    let mut graph = graph_with_incoming(vec![("helper", Vec::new())]);
    graph.prepare_finds_nothing = true;
    let (harness, cx) = setup(graph, cx).await;

    harness.show_callers_at(1, cx);
    assert_eq!(
        harness.panel.read_with(cx, |panel, _| panel.state.clone()),
        PanelState::NoSymbol
    );

    harness
        .graph
        .lock()
        .expect("graph lock")
        .prepare_finds_nothing = false;
    emit_work_end(&harness.project, cx);
    assert_eq!(harness.rows(cx), vec!["helper", "  [empty]"]);

    harness
        .graph
        .lock()
        .expect("graph lock")
        .incoming
        .insert("helper", vec![call("main", "main.rs", 0)]);
    emit_work_end(&harness.project, cx);
    assert_eq!(
        harness.rows(cx),
        vec!["helper", "  main"],
        "an empty answer is asked for again once indexing ends"
    );
}

fn advance(duration: Duration, cx: &mut VisualTestContext) {
    cx.executor().advance_clock(duration);
    cx.executor().run_until_parked();
}

#[gpui::test]
async fn test_indexing_retries_are_throttled_and_back_off(cx: &mut TestAppContext) {
    let (harness, cx) = setup(graph_with_incoming(vec![("helper", Vec::new())]), cx).await;
    harness.show_callers_at(1, cx);
    assert_eq!(harness.rows(cx), vec!["helper", "  [empty]"]);
    assert_eq!(harness.requests_answered(), 1);

    for _ in 0..20 {
        emit_work_end_without_waiting(&harness.project, cx);
    }
    assert_eq!(
        harness.requests_answered(),
        1,
        "a burst of progress reports asks nothing until the interval passes"
    );
    advance(INDEXING_RETRY_INTERVAL, cx);
    assert_eq!(
        harness.requests_answered(),
        2,
        "the whole burst asks again only once"
    );

    // A server that reports finished work twice a second for four seconds.
    // The answer waits two intervals after the first retry, then four.
    for _ in 0..8 {
        emit_work_end_without_waiting(&harness.project, cx);
        advance(INDEXING_RETRY_INTERVAL / 2, cx);
    }
    assert_eq!(
        harness.requests_answered(),
        3,
        "a server that never stops reporting is still asked again, but less and less often"
    );

    // The last report is still owed an answer, once the wait is over.
    advance(INDEXING_RETRY_INTERVAL * 60, cx);
    assert_eq!(harness.requests_answered(), 4);
    advance(INDEXING_RETRY_INTERVAL * 600, cx);
    assert_eq!(
        harness.requests_answered(),
        4,
        "without new reports nothing more is asked"
    );
    assert_eq!(harness.rows(cx), vec!["helper", "  [empty]"]);
}

#[gpui::test]
async fn test_indexing_retries_stop_when_rows_wait_at_different_times(cx: &mut TestAppContext) {
    let (harness, cx) = setup(
        graph_with_incoming(vec![(
            "helper",
            vec![call("main", "main.rs", 0), call("c5", "main.rs", 9)],
        )]),
        cx,
    )
    .await;
    harness.show_callers_at(1, cx);
    harness.select("main", cx);
    harness.dispatch(ExpandSelected, cx);
    harness.select("c5", cx);
    harness.dispatch(ExpandSelected, cx);
    assert_eq!(
        harness.rows(cx),
        vec!["helper", "  main", "    [empty]", "  c5", "    [empty]"]
    );

    emit_work_end(&harness.project, cx);
    // Folding and unfolding `c5` forgets its wait, so the two rows now wait
    // for different times.
    harness.select("c5", cx);
    harness.dispatch(CollapseSelected, cx);
    harness.select("c5", cx);
    harness.dispatch(ExpandSelected, cx);
    emit_work_end(&harness.project, cx);
    let after_reports = harness.requests_answered();

    advance(INDEXING_RETRY_INTERVAL * 600, cx);
    let settled = harness.requests_answered();
    assert!(
        settled <= after_reports + 1,
        "only the row that had to wait is asked again, once ({after_reports} then {settled})"
    );
    advance(INDEXING_RETRY_INTERVAL * 600, cx);
    assert_eq!(
        harness.requests_answered(),
        settled,
        "rows waiting at different times do not keep asking each other's turn"
    );
}

#[gpui::test]
async fn test_late_answer_after_collapsing_is_dropped(cx: &mut TestAppContext) {
    let mut graph = graph_with_incoming(vec![
        ("helper", vec![call("main", "main.rs", 0)]),
        ("main", vec![call("c1", "main.rs", 5)]),
    ]);
    let (release, held) = oneshot::channel();
    graph.held_incoming.insert("main", held);
    let (harness, cx) = setup(graph, cx).await;

    harness.show_callers_at(1, cx);
    harness.select("main", cx);
    harness.dispatch(ExpandSelected, cx);
    assert_eq!(harness.rows(cx), vec!["helper", "  main [loading]"]);

    harness.dispatch(CollapseSelected, cx);
    assert_eq!(harness.rows(cx), vec!["helper", "  main"]);
    release.send(()).ok();
    cx.executor().run_until_parked();
    assert_eq!(
        harness.rows(cx),
        vec!["helper", "  main"],
        "an answer for a collapsed node does not expand it"
    );
    assert_eq!(harness.panel.read_with(cx, |panel, _| panel.tree.len()), 2);
}

#[gpui::test]
async fn test_history_navigation_wins_over_a_slow_root(cx: &mut TestAppContext) {
    let (harness, cx) = setup(graph_with_incoming(vec![]), cx).await;
    harness.show_callers_at(1, cx);
    harness.show_callers_at(2, cx);
    assert_eq!(harness.rows(cx), vec!["leaf", "  [empty]"]);

    let (release, held) = oneshot::channel();
    harness.graph.lock().expect("graph lock").held_prepare = Some(held);
    harness.show_callers_at(3, cx);
    harness.dispatch(PreviousRoot, cx);
    assert_eq!(harness.rows(cx), vec!["helper", "  [empty]"]);

    release.send(()).ok();
    cx.executor().run_until_parked();
    assert_eq!(
        harness.rows(cx),
        vec!["helper", "  [empty]"],
        "the root looked for before going back does not replace the one gone back to"
    );
}

#[gpui::test]
async fn test_history_keeps_the_current_direction(cx: &mut TestAppContext) {
    let mut graph = graph_with_incoming(vec![("helper", vec![call("main", "main.rs", 0)])]);
    graph.outgoing = [
        ("helper", vec![call("leaf", "main.rs", 2)]),
        ("a", vec![call("b", "main.rs", 4)]),
    ]
    .into_iter()
    .collect();
    let (harness, cx) = setup(graph, cx).await;

    harness.show_callers_at(1, cx);
    harness.show_callers_at(3, cx);
    harness.dispatch(ToggleDirection, cx);
    assert_eq!(harness.rows(cx), vec!["a", "  b"]);

    harness.dispatch(PreviousRoot, cx);
    assert_eq!(
        harness.rows(cx),
        vec!["helper", "  leaf"],
        "going back shows the earlier root in the direction chosen now"
    );
    harness.dispatch(NextRoot, cx);
    assert_eq!(harness.rows(cx), vec!["a", "  b"]);
    assert_eq!(
        harness.panel.read_with(cx, |panel, _| panel.history.len()),
        2,
        "switching direction adds no history"
    );
}

#[gpui::test]
async fn test_selection_moves_to_the_nearest_shown_ancestor(cx: &mut TestAppContext) {
    let (harness, cx) = setup(
        graph_with_incoming(vec![
            (
                "helper",
                vec![call("main", "main.rs", 0), call("c5", "main.rs", 9)],
            ),
            (
                "main",
                vec![call("c1", "main.rs", 5), call("c2", "main.rs", 6)],
            ),
        ]),
        cx,
    )
    .await;
    harness.show_callers_at(1, cx);
    harness.select("main", cx);
    harness.dispatch(ExpandSelected, cx);
    harness.select("c1", cx);

    harness.panel.update_in(cx, |panel, window, cx| {
        panel
            .filter_editor
            .update(cx, |editor, cx| editor.set_text("c2", window, cx));
    });
    cx.executor().run_until_parked();
    assert_eq!(harness.rows(cx), vec!["helper", "  main", "    c2"]);
    assert_eq!(
        harness.selected_name(cx).as_deref(),
        Some("main"),
        "a row hidden by the filter hands the selection to its parent"
    );
}

#[gpui::test]
async fn test_cycling_through_call_sites_restarts_on_another_row(cx: &mut TestAppContext) {
    let (harness, cx) = setup(
        graph_with_incoming(vec![(
            "helper",
            vec![FakeCall {
                sites: vec![(0, 22, 28), (0, 12, 18)],
                ..call("main", "main.rs", 0)
            }],
        )]),
        cx,
    )
    .await;
    harness.show_callers_at(1, cx);
    let cursor = |cx: &mut VisualTestContext| {
        let editor = harness
            .workspace
            .read_with(cx, |workspace, cx| workspace.active_item_as::<Editor>(cx))
            .expect("an editor is active");
        editor.update(cx, |editor, cx| {
            editor
                .selections
                .newest::<Point>(&editor.display_snapshot(cx))
                .head()
        })
    };

    harness.select("main", cx);
    harness.dispatch(OpenSelected, cx);
    assert_eq!(cursor(cx), Point::new(0, 12));

    harness.select("helper", cx);
    harness.select("main", cx);
    harness.dispatch(OpenSelected, cx);
    assert_eq!(
        cursor(cx),
        Point::new(0, 12),
        "coming back to a row starts again from its first call site"
    );
}

#[gpui::test]
async fn test_show_more_boundary(cx: &mut TestAppContext) {
    fn many_callers(count: usize) -> Vec<FakeCall> {
        (0..count)
            .map(|index| {
                let name: &'static str = Box::leak(format!("caller{index}").into_boxed_str());
                call(name, "other.rs", 0)
            })
            .collect()
    }
    let (harness, cx) = setup(
        graph_with_incoming(vec![
            ("helper", many_callers(200)),
            ("leaf", many_callers(201)),
        ]),
        cx,
    )
    .await;

    harness.show_callers_at(1, cx);
    let rows = harness.rows(cx);
    assert_eq!(rows.len(), 201, "the root and exactly 200 callers");
    assert!(
        rows.iter().all(|row| !row.contains("more")),
        "200 callers fit without a Show more row"
    );

    harness.show_callers_at(2, cx);
    let rows = harness.rows(cx);
    assert_eq!(rows.len(), 202);
    assert_eq!(rows.last().map(String::as_str), Some("  [1 more]"));
}

#[gpui::test]
async fn test_failed_retry_shows_the_error_again(cx: &mut TestAppContext) {
    let mut graph = graph_with_incoming(vec![("helper", vec![call("main", "main.rs", 0)])]);
    graph.failures_left = 2;
    let (harness, cx) = setup(graph, cx).await;

    harness.show_callers_at(1, cx);
    assert_eq!(harness.rows(cx), vec!["helper", "  [error]"]);

    harness.select_row(1, cx);
    harness.dispatch(OpenSelected, cx);
    assert_eq!(
        harness.rows(cx),
        vec!["helper", "  [error]"],
        "a retry that fails again shows the error again, not a spinner"
    );
    assert_eq!(harness.requests_answered(), 2);

    harness.select_row(1, cx);
    harness.dispatch(OpenSelected, cx);
    assert_eq!(harness.rows(cx), vec!["helper", "  main"]);
    assert_eq!(harness.selected_name(cx).as_deref(), Some("helper"));
}

#[gpui::test]
async fn test_follow_cursor_asks_only_when_leaving_the_root(cx: &mut TestAppContext) {
    let (harness, cx) = setup(graph_with_incoming(vec![]), cx).await;
    harness.show_callers_at(1, cx);
    harness.dispatch(ToggleFollowCursor, cx);
    let prepares = |cx: &mut VisualTestContext| {
        cx.executor().advance_clock(FOLLOW_CURSOR_DEBOUNCE);
        cx.executor().run_until_parked();
        harness.graph.lock().expect("graph lock").prepares
    };
    let before = prepares(cx);

    for column in [5, 7, 9] {
        harness.editor.update_in(cx, |editor, window, cx| {
            editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
                selections.select_ranges([Point::new(1, column)..Point::new(1, column)])
            });
        });
        cx.executor().run_until_parked();
    }
    assert_eq!(
        prepares(cx),
        before,
        "moving within the root's function asks nothing"
    );

    harness.move_cursor(2, cx);
    assert_eq!(prepares(cx), before + 1);
    assert_eq!(harness.rows(cx), vec!["leaf", "  [empty]"]);

    harness.workspace.update_in(cx, |workspace, window, cx| {
        workspace.close_panel::<CallGraphPanel>(window, cx);
    });
    harness.move_cursor(3, cx);
    assert_eq!(
        prepares(cx),
        before + 1,
        "a hidden panel does not follow the cursor"
    );
}

#[gpui::test]
async fn test_closing_the_workspace_releases_the_panel_while_asking(cx: &mut TestAppContext) {
    let mut graph = graph_with_incoming(vec![("helper", vec![call("main", "main.rs", 0)])]);
    let (release, held) = oneshot::channel();
    graph.held_incoming.insert("helper", held);
    let (harness, cx) = setup(graph, cx).await;
    harness.show_callers_at(1, cx);
    assert_eq!(harness.rows(cx), vec!["helper [loading]"]);

    let Harness {
        workspace, panel, ..
    } = harness;
    let weak_panel = panel.downgrade();
    drop(panel);
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.close_panel::<CallGraphPanel>(window, cx);
        let panel = workspace
            .panel::<CallGraphPanel>(cx)
            .expect("the panel is still registered");
        workspace.remove_panel(&panel, window, cx);
    });
    cx.executor().run_until_parked();
    assert!(
        weak_panel.upgrade().is_none(),
        "a request still in flight does not keep the panel alive"
    );
    release.send(()).ok();
    cx.executor().run_until_parked();
}

// The symbol index (ADR-F68): C and C++ calls found by name.

const MAIN_C: &str = "static int helper(void) { return leaf(); }
int leaf(void) { return 0; }
#define LOG(x) leaf()
int main(void) {
  helper();
  printk(\"x\");
  ops->open(0);
  dup();
  missing_fn();
  LOG(1);
  return helper();
}
int caller_two(void) { return leaf(); }
int value = leaf();
";
const OTHER_C: &str = "static int helper(void) { return 2; }
static int init(void) { return helper(); }
int foo_open(int x) { return x; }
struct file_operations fops = { .open = foo_open };
int use_ops(struct ops *o) { return o->caller_two(); }
";
const A_C: &str = "int dup(void) { return 1; }\n";
const B_C: &str = "int dup(void) { return 2; }\n";

fn c_lang() -> Arc<Language> {
    Arc::new(Language::new(
        LanguageConfig {
            name: LanguageName::new("C"),
            matcher: Arc::new(LanguageMatcher {
                path_suffixes: vec!["c".into(), "h".into()],
                ..LanguageMatcher::default()
            }),
            ..LanguageConfig::default()
        },
        None,
    ))
}

fn c_files() -> Vec<(&'static str, &'static str)> {
    vec![
        ("main.c", MAIN_C),
        ("other.c", OTHER_C),
        ("a.c", A_C),
        ("b.c", B_C),
    ]
}

/// The C project of `c_files`, with its symbol index and, with `with_server`,
/// a fake language server that answers from `graph`.
async fn setup_c(
    with_server: bool,
    graph: FakeGraph,
    cx: &mut TestAppContext,
) -> (Harness, &mut VisualTestContext, tempfile::TempDir) {
    let files = c_files();
    let tree = serde_json::Value::Object(
        files
            .iter()
            .map(|(name, text)| (name.to_string(), json!(text)))
            .collect(),
    );
    let (harness, cx) = setup_project(tree, "src/main.c", c_lang(), with_server, graph, cx).await;
    let index = harness.install_index(&files, cx);
    (harness, cx, index)
}

/// The lines of `MAIN_C` the fake language server knows functions on.
fn c_prepare_items() -> Vec<(&'static str, u32, &'static str)> {
    vec![
        ("main.c", 0, "helper"),
        ("main.c", 1, "leaf"),
        ("main.c", 3, "main"),
        ("main.c", 12, "caller_two"),
    ]
}

/// The call site `(line, start column, end column)` of `needle` in `source`.
fn site_of(source: &str, line: u32, needle: &str) -> (u32, u32, u32) {
    let text = source.lines().nth(line as usize).expect("the line exists");
    let start = text.find(needle).expect("the needle is on the line") as u32;
    (line, start, start + needle.len() as u32)
}

impl Harness {
    /// Gives the project a symbol index of `files`, which are in `src`.
    fn install_index(
        &self,
        files: &[(&str, &str)],
        cx: &mut VisualTestContext,
    ) -> tempfile::TempDir {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let paths = files
            .iter()
            .map(|(name, text)| (format!("src/{name}"), *text))
            .collect::<Vec<_>>();
        let indexed = paths
            .iter()
            .map(|(path, text)| (path.as_str(), *text))
            .collect::<Vec<_>>();
        let index = project::symbol_index_store::build_index_for_tests(directory.path(), &indexed)
            .expect("building the index");
        let worktree_id = self.worktree_id;
        self.project.update(cx, |project, cx| {
            project.symbol_index_store().update(cx, |store, cx| {
                store.insert_index_for_tests(worktree_id, index, cx)
            })
        });
        cx.executor().run_until_parked();
        directory
    }

    fn move_cursor_to(&self, point: Point, cx: &mut VisualTestContext) {
        self.editor.update_in(cx, |editor, window, cx| {
            editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
                selections.select_ranges([point..point])
            });
            window.focus(&editor.focus_handle(cx), cx);
        });
    }

    fn show_callees_at(&self, line: u32, cx: &mut VisualTestContext) {
        self.move_cursor(line, cx);
        cx.dispatch_action(ShowCallees);
        cx.executor().run_until_parked();
    }

    fn set_sources(&self, sources: SourceFilter, cx: &mut VisualTestContext) {
        self.panel.update_in(cx, |panel, window, cx| {
            panel.set_sources(sources, window, cx)
        });
        cx.executor().run_until_parked();
    }

    fn select_row_named(&self, row: &str, cx: &mut VisualTestContext) {
        let index = self
            .rows(cx)
            .iter()
            .position(|shown| shown.trim() == row)
            .unwrap_or_else(|| panic!("no row {row}"));
        self.select_row(index, cx);
    }

    fn call_site_rows(&self, name: &str, cx: &mut VisualTestContext) -> Vec<u32> {
        self.panel.read_with(cx, |panel, _| {
            panel
                .rows
                .iter()
                .find_map(|row| {
                    let node = panel.tree.node(row.node())?;
                    (matches!(row, Row::Node { .. }) && node.symbol.name == name).then(|| {
                        node.symbol
                            .call_sites
                            .iter()
                            .map(|site| site.points.start_row())
                            .collect()
                    })
                })
                .unwrap_or_default()
        })
    }
}

#[gpui::test]
async fn test_index_answers_without_a_language_server(cx: &mut TestAppContext) {
    let (harness, cx, _index) = setup_c(false, FakeGraph::default(), cx).await;

    harness.show_callers_at(1, cx);
    assert_eq!(
        harness.rows(cx),
        vec![
            "leaf [name match]",
            "  helper [name match]",
            "  LOG [macro]",
            "  caller_two [name match]",
            "  (file scope)",
        ],
        "the root and its callers come from the index, marked as name matches"
    );
    assert_eq!(
        harness.panel.read_with(cx, |panel, _| panel.state.clone()),
        PanelState::Ready
    );

    // `helper` is static in main.c; other.c's own static `helper` is called
    // by other.c's `init`, which must not be listed.
    harness.show_callers_at(0, cx);
    assert_eq!(
        harness.rows(cx),
        vec!["helper [name match]", "  main [name match] [x2]"]
    );

    // In C, `o->caller_two()` calls a member, never the function of that name.
    harness.show_callers_at(12, cx);
    assert_eq!(
        harness.rows(cx),
        vec!["caller_two [name match]", "  [empty]"]
    );

    // A cursor inside a function's body, not on a function's name, makes
    // the enclosing function the root.
    harness.move_cursor_to(Point::new(4, 0), cx);
    cx.dispatch_action(ShowCallers);
    cx.executor().run_until_parked();
    assert_eq!(
        harness.rows(cx).first().map(String::as_str),
        Some("main [name match]")
    );
}

#[gpui::test]
async fn test_callees_from_the_index(cx: &mut TestAppContext) {
    let (harness, cx, _index) = setup_c(false, FakeGraph::default(), cx).await;

    harness.show_callees_at(3, cx);
    assert_eq!(
        harness.rows(cx),
        vec![
            "main [name match]",
            "  helper [name match] [x2]",
            "  indirect .open",
            "  dup [2 definitions]",
            "  missing_fn [no definition]",
            "  LOG [macro]",
            "  [1 excluded]",
        ]
    );

    harness.select("open", cx);
    harness.dispatch(ExpandSelected, cx);
    harness.select("dup", cx);
    harness.dispatch(ExpandSelected, cx);
    harness.select("LOG", cx);
    harness.dispatch(ExpandSelected, cx);
    assert_eq!(
        harness.rows(cx),
        vec![
            "main [name match]",
            "  helper [name match] [x2]",
            "  indirect .open",
            "    foo_open [pointer candidate]",
            "  dup [2 definitions]",
            "    dup [name match]",
            "    dup [name match]",
            "  missing_fn [no definition]",
            "  LOG [macro]",
            "    leaf [name match]",
            "  [1 excluded]",
        ]
    );
    let candidate_files = harness.panel.read_with(cx, |panel, _| {
        panel
            .rows
            .iter()
            .filter_map(|row| panel.tree.node(row.node()))
            .filter(|node| node.depth == 2 && node.symbol.name == "dup")
            .filter_map(|node| node.symbol.file_name())
            .map(|name| name.to_string())
            .collect::<Vec<_>>()
    });
    assert_eq!(candidate_files.len(), 2);
    assert!(candidate_files.contains(&"a.c".to_string()));
    assert!(candidate_files.contains(&"b.c".to_string()));

    // A candidate is followed further like any function.
    harness.select("foo_open", cx);
    harness.dispatch(ExpandSelected, cx);
    assert!(
        harness.rows(cx).contains(&"      [empty]".to_string()),
        "foo_open calls nothing: {:?}",
        harness.rows(cx)
    );

    // A callee without a definition cannot be expanded.
    let before = harness.rows(cx);
    harness.select("missing_fn", cx);
    harness.dispatch(ExpandSelected, cx);
    assert_eq!(harness.rows(cx), before);

    harness.select_row_named("[1 excluded]", cx);
    harness.dispatch(OpenSelected, cx);
    assert_eq!(
        harness.rows(cx).last().map(String::as_str),
        Some("  printk [no definition]"),
        "the excluded callee is shown on request"
    );
}

#[gpui::test]
async fn test_both_sources_are_merged(cx: &mut TestAppContext) {
    let graph = FakeGraph {
        prepare_items: c_prepare_items(),
        incoming: [
            (
                "leaf",
                vec![FakeCall {
                    sites: vec![site_of(MAIN_C, 0, "leaf")],
                    ..call("helper", "main.c", 0)
                }],
            ),
            (
                "helper",
                vec![FakeCall {
                    sites: vec![site_of(MAIN_C, 4, "helper")],
                    ..call("main", "main.c", 3)
                }],
            ),
        ]
        .into_iter()
        .collect(),
        ..FakeGraph::default()
    };
    let (harness, cx, _index) = setup_c(true, graph, cx).await;

    harness.show_callers_at(1, cx);
    assert_eq!(
        harness.rows(cx),
        vec![
            "leaf",
            "  helper",
            "  LOG [macro]",
            "  caller_two [name match]",
            "  (file scope)",
        ],
        "a caller both sources found is one exact row, and name matches follow it"
    );
    assert_eq!(harness.call_site_rows("helper", cx), vec![0]);

    harness.show_callers_at(0, cx);
    assert_eq!(harness.rows(cx), vec!["helper", "  main [x2]"]);
    assert_eq!(
        harness.call_site_rows("main", cx),
        vec![4, 10],
        "the call the language server missed is added to its row"
    );
}

#[gpui::test]
async fn test_an_empty_exact_answer_does_not_hide_name_matches(cx: &mut TestAppContext) {
    let graph = FakeGraph {
        prepare_items: c_prepare_items(),
        ..FakeGraph::default()
    };
    let (harness, cx, _index) = setup_c(true, graph, cx).await;
    harness.show_callers_at(1, cx);
    assert_eq!(
        harness.rows(cx),
        vec![
            "leaf",
            "  helper [name match]",
            "  LOG [macro]",
            "  caller_two [name match]",
            "  (file scope)",
        ]
    );
    assert_eq!(
        harness.requests_answered(),
        1,
        "the language server was asked"
    );
}

#[gpui::test]
async fn test_name_matches_show_before_a_slow_language_server(cx: &mut TestAppContext) {
    let (release, held) = oneshot::channel();
    let mut graph = FakeGraph {
        prepare_items: c_prepare_items(),
        incoming: [(
            "leaf",
            vec![FakeCall {
                sites: vec![site_of(MAIN_C, 0, "leaf")],
                ..call("helper", "main.c", 0)
            }],
        )]
        .into_iter()
        .collect(),
        ..FakeGraph::default()
    };
    graph.held_incoming.insert("leaf", held);
    let (harness, cx, _index) = setup_c(true, graph, cx).await;

    harness.show_callers_at(1, cx);
    assert_eq!(
        harness.rows(cx),
        vec![
            "leaf [loading]",
            "  helper [name match]",
            "  LOG [macro]",
            "  caller_two [name match]",
            "  (file scope)",
        ],
        "the index's answer is shown while the language server works"
    );
    harness.select("helper", cx);
    let helper_node = harness
        .panel
        .read_with(cx, |panel, _| panel.selected.map(|row| row.node()));

    release.send(()).ok();
    cx.executor().run_until_parked();
    assert_eq!(
        harness.rows(cx),
        vec![
            "leaf",
            "  helper",
            "  LOG [macro]",
            "  caller_two [name match]",
            "  (file scope)",
        ]
    );
    assert_eq!(
        harness
            .panel
            .read_with(cx, |panel, _| panel.selected.map(|row| row.node())),
        helper_node,
        "the selected row stays selected when the exact answer is merged in"
    );
}

#[gpui::test]
async fn test_several_roots_by_one_name_are_offered(cx: &mut TestAppContext) {
    let (harness, cx, _index) = setup_c(false, FakeGraph::default(), cx).await;
    harness.move_cursor_to(Point::new(7, 3), cx);
    cx.dispatch_action(ShowCallers);
    cx.executor().run_until_parked();

    let (state, candidates) = harness.panel.read_with(cx, |panel, _| {
        (
            panel.state.clone(),
            panel
                .root_candidates
                .iter()
                .filter_map(|symbol| symbol.file_name())
                .map(|name| name.to_string())
                .collect::<Vec<_>>(),
        )
    });
    assert_eq!(state, PanelState::ChooseRoot);
    assert_eq!(candidates.len(), 2);
    assert!(candidates.contains(&"a.c".to_string()) && candidates.contains(&"b.c".to_string()));
    assert!(harness.rows(cx).is_empty());

    harness.panel.update_in(cx, |panel, window, cx| {
        panel.choose_root(1, window, cx);
    });
    cx.executor().run_until_parked();
    assert_eq!(
        harness.rows(cx),
        vec!["dup [name match]", "  main [name match]"]
    );
    assert!(
        harness
            .panel
            .read_with(cx, |panel, _| panel.root_candidates.is_empty())
    );
}

const DUPLICATES_MAIN_C: &str = "int main(void) {\n  dup();\n}\n";

/// A C project whose `main` calls `dup`, which `count` files each define.
async fn setup_duplicates(
    count: usize,
    cx: &mut TestAppContext,
) -> (Harness, &mut VisualTestContext, tempfile::TempDir) {
    let names = (0..count)
        .map(|index| format!("dup{index}.c"))
        .collect::<Vec<_>>();
    let texts = (0..count)
        .map(|index| format!("int dup(void) {{ return {index}; }}\n"))
        .collect::<Vec<_>>();
    let mut files = vec![("main.c", DUPLICATES_MAIN_C)];
    files.extend(
        names
            .iter()
            .zip(&texts)
            .map(|(name, text)| (name.as_str(), text.as_str())),
    );
    let tree = serde_json::Value::Object(
        files
            .iter()
            .map(|(name, text)| (name.to_string(), json!(text)))
            .collect(),
    );
    let (harness, cx) = setup_project(
        tree,
        "src/main.c",
        c_lang(),
        false,
        FakeGraph::default(),
        cx,
    )
    .await;
    let index = harness.install_index(&files, cx);
    (harness, cx, index)
}

fn root_file(harness: &Harness, cx: &mut VisualTestContext) -> Option<String> {
    harness.panel.read_with(cx, |panel, _| {
        panel
            .root_symbol()
            .and_then(|symbol| symbol.file_name())
            .map(|name| name.to_string())
    })
}

fn candidate_file(harness: &Harness, index: usize, cx: &mut VisualTestContext) -> Option<String> {
    harness.panel.read_with(cx, |panel, _| {
        panel
            .root_candidates
            .get(index)
            .and_then(|symbol| symbol.file_name())
            .map(|name| name.to_string())
    })
}

#[gpui::test]
async fn test_root_candidates_are_chosen_with_the_keyboard(cx: &mut TestAppContext) {
    let (harness, cx, _index) = setup_duplicates(3, cx).await;
    harness.show_callers_at(0, cx);
    assert_eq!(root_file(&harness, cx).as_deref(), Some("main.c"));

    harness.show_callers_at(1, cx);
    assert_eq!(
        harness.panel.read_with(cx, |panel, _| (
            panel.state.clone(),
            panel.root_candidates.len(),
            panel.selected_root_candidate
        )),
        (PanelState::ChooseRoot, 3, 0),
        "the first candidate is selected when the candidates are offered"
    );
    let third = candidate_file(&harness, 2, cx);
    assert!(third.is_some());

    harness.dispatch(menu::SelectNext, cx);
    harness.dispatch(menu::SelectNext, cx);
    harness.dispatch(menu::SelectNext, cx);
    assert_eq!(
        harness
            .panel
            .read_with(cx, |panel, _| panel.selected_root_candidate),
        2,
        "the selection stops at the last candidate"
    );
    harness.dispatch(menu::SelectPrevious, cx);
    harness.dispatch(menu::SelectNext, cx);
    harness.dispatch(menu::Confirm, cx);
    assert_eq!(root_file(&harness, cx), third, "Enter makes it the root");
    assert_eq!(
        harness.panel.read_with(cx, |panel, _| panel.state.clone()),
        PanelState::Ready
    );
    assert_eq!(
        harness.rows(cx),
        vec!["dup [name match]", "  main [name match]"]
    );

    // Escape goes back to the root shown before the candidates.
    harness.show_callers_at(0, cx);
    harness.show_callers_at(1, cx);
    assert_eq!(
        harness.panel.read_with(cx, |panel, _| panel.state.clone()),
        PanelState::ChooseRoot
    );
    harness.dispatch(menu::SelectNext, cx);
    harness.dispatch(menu::Cancel, cx);
    assert_eq!(
        harness.panel.read_with(cx, |panel, _| (
            panel.state.clone(),
            panel.root_candidates.is_empty()
        )),
        (PanelState::Ready, true)
    );
    assert_eq!(root_file(&harness, cx).as_deref(), Some("main.c"));
    assert_eq!(
        harness.rows(cx).first().map(String::as_str),
        Some("main [name match]")
    );
}

#[gpui::test]
async fn test_escaping_the_first_root_candidates_leaves_the_panel_empty(cx: &mut TestAppContext) {
    let (harness, cx, _index) = setup_duplicates(2, cx).await;
    harness.show_callers_at(1, cx);
    assert_eq!(
        harness.panel.read_with(cx, |panel, _| panel.state.clone()),
        PanelState::ChooseRoot
    );

    harness.dispatch(menu::Cancel, cx);
    assert_eq!(
        harness.panel.read_with(cx, |panel, _| (
            panel.state.clone(),
            panel.root_candidates.is_empty()
        )),
        (PanelState::Idle, true)
    );
    assert_eq!(root_file(&harness, cx), None);
    assert!(harness.rows(cx).is_empty());
}

#[gpui::test]
async fn test_only_the_first_root_candidates_are_listed(cx: &mut TestAppContext) {
    let count = MAX_ROOT_CANDIDATES + 5;
    let (harness, cx, _index) = setup_duplicates(count, cx).await;
    harness.show_callers_at(1, cx);
    assert_eq!(
        harness.panel.read_with(cx, |panel, _| {
            let (listed, not_listed) = panel.listed_root_candidates();
            (panel.root_candidates.len(), listed.len(), not_listed)
        }),
        (count, MAX_ROOT_CANDIDATES, 5)
    );

    harness.dispatch(menu::SelectLast, cx);
    harness.dispatch(menu::SelectNext, cx);
    assert_eq!(
        harness
            .panel
            .read_with(cx, |panel, _| panel.selected_root_candidate),
        MAX_ROOT_CANDIDATES - 1,
        "a candidate that is not listed cannot be selected"
    );
    let last_listed = candidate_file(&harness, MAX_ROOT_CANDIDATES - 1, cx);
    harness.dispatch(menu::Confirm, cx);
    assert_eq!(root_file(&harness, cx), last_listed);

    harness.dispatch(menu::SelectFirst, cx);
    assert_eq!(
        harness
            .panel
            .read_with(cx, |panel, _| panel.selected.map(|row| row.depth())),
        Some(0),
        "after choosing, the keys move through the tree again"
    );
}

fn static_shadow_files() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "a.c",
            "int f(void) { return 0; }\nint g(void) { return f(); }\n",
        ),
        (
            "b.c",
            "static int f(void) { return 1; }\nint h(void) { return f(); }\n",
        ),
        ("c.c", "int k(void) { return f(); }\n"),
    ]
}

/// The callers of the `f` that `open` defines on its first line, sorted.
async fn callers_of_f_in(open: &str, cx: &mut TestAppContext) -> Vec<String> {
    callers_of_f_among(static_shadow_files(), open, cx).await
}

/// [`callers_of_f_in`] in a project of `files`.
async fn callers_of_f_among(
    files: Vec<(&'static str, &'static str)>,
    open: &str,
    cx: &mut TestAppContext,
) -> Vec<String> {
    let tree = serde_json::Value::Object(
        files
            .iter()
            .map(|(name, text)| (name.to_string(), json!(text)))
            .collect(),
    );
    let (harness, cx) = setup_project(
        tree,
        &format!("src/{open}"),
        c_lang(),
        false,
        FakeGraph::default(),
        cx,
    )
    .await;
    let _index = harness.install_index(&files, cx);
    let line = files
        .iter()
        .find(|(name, _)| *name == open)
        .and_then(|(_, text)| text.lines().next())
        .unwrap_or_default();
    let column = line.find("f(").expect("f is defined on the first line") as u32;
    harness.move_cursor_to(Point::new(0, column), cx);
    cx.dispatch_action(ShowCallers);
    cx.executor().run_until_parked();
    assert_eq!(root_file(&harness, cx).as_deref(), Some(open));
    let mut rows = harness.rows(cx);
    rows.sort();
    rows
}

#[gpui::test]
async fn test_calls_of_another_files_static_function_are_not_callers(cx: &mut TestAppContext) {
    assert_eq!(
        callers_of_f_in("a.c", cx).await,
        vec!["  g [name match]", "  k [name match]", "f [name match]"],
        "b.c calls its own static f, not a.c's"
    );
}

#[gpui::test]
async fn test_a_static_function_is_called_only_from_its_own_file(cx: &mut TestAppContext) {
    assert_eq!(
        callers_of_f_in("b.c", cx).await,
        vec!["  h [name match]", "f [name match]"]
    );
}

/// A header's `static inline` function is compiled into each file that includes it, so a
/// header defining one does not make the calls in it its own: they stay callers by name.
#[gpui::test]
async fn test_calls_in_a_header_with_a_static_function_stay_callers(cx: &mut TestAppContext) {
    let mut files = static_shadow_files();
    files.push((
        "inl.h",
        "static inline int f(void) { return 9; }\nstatic inline int in_header(void) { return f(); }\n",
    ));
    assert_eq!(
        callers_of_f_among(files, "a.c", cx).await,
        vec![
            "  g [name match]",
            "  in_header [name match]",
            "  k [name match]",
            "f [name match]"
        ]
    );
}

/// The state of the panel, its candidates' files and its root's file once the callers of the
/// `f` at `needle` on `line` of `open` are asked for.
async fn root_of_f_at(
    open: &str,
    line: u32,
    needle: &str,
    cx: &mut TestAppContext,
) -> (PanelState, Vec<String>, Option<(String, u32)>) {
    root_of_f_in(static_shadow_files(), open, line, needle, cx).await
}

/// [`root_of_f_at`] in a project of `files`; the root is given with the line of its name.
async fn root_of_f_in(
    files: Vec<(&'static str, &'static str)>,
    open: &str,
    line: u32,
    needle: &str,
    cx: &mut TestAppContext,
) -> (PanelState, Vec<String>, Option<(String, u32)>) {
    let tree = serde_json::Value::Object(
        files
            .iter()
            .map(|(name, text)| (name.to_string(), json!(text)))
            .collect(),
    );
    let (harness, cx) = setup_project(
        tree,
        &format!("src/{open}"),
        c_lang(),
        false,
        FakeGraph::default(),
        cx,
    )
    .await;
    let _index = harness.install_index(&files, cx);
    let text = files
        .iter()
        .find(|(name, _)| *name == open)
        .map(|(_, text)| *text)
        .unwrap_or_default();
    let (_, start, _) = site_of(text, line, needle);
    harness.move_cursor_to(Point::new(line, start), cx);
    cx.dispatch_action(ShowCallers);
    cx.executor().run_until_parked();
    let (state, candidates) = harness.panel.read_with(cx, |panel, _| {
        (
            panel.state.clone(),
            panel
                .root_candidates
                .iter()
                .filter_map(|symbol| symbol.file_name())
                .map(|name| name.to_string())
                .collect(),
        )
    });
    let root = harness.panel.read_with(cx, |panel, _| {
        panel
            .root_symbol()
            .and_then(|symbol| Some((symbol.file_name()?.to_string(), symbol.display_row)))
    });
    (state, candidates, root)
}

/// b.c sees its own `static f` and a.c's `f`; on the name of its definition, the root is that
/// definition, not a choice between the two.
#[gpui::test]
async fn test_the_definition_under_the_cursor_is_the_root(cx: &mut TestAppContext) {
    assert_eq!(
        root_of_f_at("b.c", 0, "f(", cx).await,
        (PanelState::Ready, Vec::new(), Some(("b.c".to_string(), 0)))
    );
}

/// With two definitions in the file (one per configuration), the one under the cursor.
#[gpui::test]
async fn test_of_two_definitions_in_a_file_the_one_under_the_cursor_is_the_root(
    cx: &mut TestAppContext,
) {
    let mut files = static_shadow_files();
    files.push((
        "d.c",
        "#ifdef ONE\nint f(void) { return 2; }\n#else\nint f(void) { return 3; }\n#endif\n",
    ));
    assert_eq!(
        root_of_f_in(files, "d.c", 3, "f(", cx).await,
        (PanelState::Ready, Vec::new(), Some(("d.c".to_string(), 3)))
    );
}

/// A call in b.c goes to b.c's own `static f`, not to a.c's.
#[gpui::test]
async fn test_a_call_goes_to_its_own_files_definition(cx: &mut TestAppContext) {
    assert_eq!(
        root_of_f_at("b.c", 1, "f()", cx).await,
        (PanelState::Ready, Vec::new(), Some(("b.c".to_string(), 0)))
    );
}

/// c.c has no `f` of its own and does not see b.c's `static` one: a.c's is the only one.
#[gpui::test]
async fn test_a_call_without_a_definition_in_its_file_goes_to_the_one_it_sees(
    cx: &mut TestAppContext,
) {
    assert_eq!(
        root_of_f_at("c.c", 0, "f()", cx).await,
        (PanelState::Ready, Vec::new(), Some(("a.c".to_string(), 0)))
    );
}

#[gpui::test]
async fn test_other_languages_do_not_ask_the_index(cx: &mut TestAppContext) {
    let (harness, cx) = setup(
        graph_with_incoming(vec![("helper", vec![call("main", "main.rs", 0)])]),
        cx,
    )
    .await;
    // A C file in the project calls a function of the same name.
    let lib_c = "int helper(void) { return 0; }\nint c_caller(void) { return helper(); }\n";
    let fs = harness
        .project
        .read_with(cx, |project, _| project.fs().clone());
    fs.as_fake()
        .insert_file(path!("/test/src/lib.c"), lib_c.as_bytes().to_vec())
        .await;
    cx.executor().run_until_parked();
    let _index = harness.install_index(&[("lib.c", lib_c)], cx);
    harness.show_callers_at(1, cx);
    assert_eq!(harness.rows(cx), vec!["helper", "  main"]);
}

#[gpui::test]
async fn test_sources_can_be_limited(cx: &mut TestAppContext) {
    let graph = FakeGraph {
        prepare_items: c_prepare_items(),
        incoming: [(
            "leaf",
            vec![FakeCall {
                sites: vec![site_of(MAIN_C, 0, "leaf")],
                ..call("helper", "main.c", 0)
            }],
        )]
        .into_iter()
        .collect(),
        ..FakeGraph::default()
    };
    let (harness, cx, _index) = setup_c(true, graph, cx).await;
    harness.show_callers_at(1, cx);
    assert_eq!(harness.rows(cx).len(), 5);

    harness.set_sources(SourceFilter::LanguageServer, cx);
    assert_eq!(harness.rows(cx), vec!["leaf", "  helper"]);

    let asked = harness.requests_answered();
    harness.set_sources(SourceFilter::NameMatch, cx);
    assert_eq!(
        harness.rows(cx),
        vec![
            "leaf",
            "  helper [name match]",
            "  LOG [macro]",
            "  caller_two [name match]",
            "  (file scope)",
        ]
    );
    assert_eq!(
        harness.requests_answered(),
        asked,
        "name matches only do not ask the language server"
    );

    cx.update(|_, cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |settings| {
                settings.call_graph_panel.get_or_insert_default().sources =
                    Some(settings::CallGraphSources::LanguageServer);
            });
        });
    });
    cx.executor().run_until_parked();
    assert_eq!(
        harness.panel.read_with(cx, |panel, _| panel.sources),
        SourceFilter::LanguageServer,
        "the setting chooses the sources"
    );
    assert_eq!(harness.rows(cx), vec!["leaf", "  helper"]);
}

#[gpui::test]
async fn test_truncated_callers_are_marked(cx: &mut TestAppContext) {
    let mut many_calls = String::from("int leaf(void) { return 0; }\nint big(void) {\n");
    for _ in 0..MAX_TEST_CALLERS + 1 {
        many_calls.push_str("  leaf();\n");
    }
    many_calls.push_str("  return 0;\n}\n");
    let many_calls: &'static str = Box::leak(many_calls.into_boxed_str());
    let (harness, cx) = setup_project(
        json!({ "main.c": many_calls }),
        "src/main.c",
        c_lang(),
        false,
        FakeGraph::default(),
        cx,
    )
    .await;
    let _index = harness.install_index(&[("main.c", many_calls)], cx);
    harness.show_callers_at(0, cx);
    assert_eq!(
        harness.rows(cx),
        vec![
            "leaf [name match]",
            format!("  big [name match] [x{MAX_TEST_CALLERS}]").as_str(),
            "  [truncated]",
        ]
    );
}

const MAX_TEST_CALLERS: usize = project::symbol_index_store::MAX_CALLERS_LIMIT;

#[gpui::test]
async fn test_finished_indexing_asks_again_for_partial_answers(cx: &mut TestAppContext) {
    let (harness, cx, _index) = setup_c(false, FakeGraph::default(), cx).await;
    harness.show_callers_at(1, cx);
    assert_eq!(harness.rows(cx).len(), 5);

    // As if the answer had come while the index was being built.
    harness.panel.update(cx, |panel, _| {
        panel.index_was_building = true;
        if let Some(root) = panel.tree.root()
            && let Some(node) = panel.tree.node_mut(root)
        {
            node.index_partial = true;
        }
    });
    let with_new_caller = MAIN_C.replace(
        "int value = leaf();\n",
        "int value = leaf();\nint caller_three(void) { return leaf(); }\n",
    );
    let mut files = c_files();
    files[0] = ("main.c", with_new_caller.as_str());
    let _updated = harness.install_index(&files, cx);
    assert_eq!(
        harness.rows(cx).len(),
        5,
        "the answer is asked for again only after the retry interval"
    );
    advance(INDEXING_RETRY_INTERVAL, cx);
    assert_eq!(
        harness.rows(cx).last().map(String::as_str),
        Some("  caller_three [name match]"),
        "finishing the index asks again for what it answered while it was being built"
    );

    // The new answer is complete, so later changes of the status ask nothing.
    files[0] = ("main.c", MAIN_C);
    let _reverted = harness.install_index(&files, cx);
    advance(INDEXING_RETRY_INTERVAL * 600, cx);
    assert_eq!(
        harness.rows(cx).last().map(String::as_str),
        Some("  caller_three [name match]")
    );
}

/// The store reports the index as being built, then built: the progress shows in the status
/// row, and a row expanded meanwhile is asked again once, when the build ends, not at each
/// step of its progress.
#[gpui::test]
async fn test_a_row_expanded_while_the_index_is_built_is_asked_again_when_it_is_built(
    cx: &mut TestAppContext,
) {
    let (harness, cx, _index) = setup_c(false, FakeGraph::default(), cx).await;
    let worktree_id = harness.worktree_id;
    let set_status = |status: SymbolIndexStatus, cx: &mut VisualTestContext| {
        harness.project.update(cx, |project, cx| {
            project.symbol_index_store().update(cx, |store, cx| {
                store.set_status_for_tests(worktree_id, status, cx)
            })
        });
        cx.executor().run_until_parked();
    };
    let status_row = |cx: &mut VisualTestContext| {
        harness.panel.update(cx, |panel, cx| {
            panel
                .shown_index_summary(cx)
                .as_ref()
                .and_then(index_summary_text)
                .map(|(message, _)| message)
        })
    };
    let root_retries = |cx: &mut VisualTestContext| {
        harness.panel.read_with(cx, |panel, _| {
            panel
                .tree
                .root()
                .and_then(|root| panel.tree.node(root))
                .map(|node| (node.index_partial, node.empty_retries))
        })
    };

    set_status(SymbolIndexStatus::Indexing { done: 1, total: 4 }, cx);
    harness.show_callers_at(1, cx);
    assert_eq!(
        status_row(cx).as_deref(),
        Some("Indexing symbols 1 / 4"),
        "the progress shows while a C file's callers are listed"
    );
    assert_eq!(harness.rows(cx).len(), 5);
    assert_eq!(
        root_retries(cx),
        Some((true, 0)),
        "an answer given while the index is built may be partial"
    );

    // The index gains a caller without the build ending: nothing is asked yet.
    let with_new_caller = MAIN_C.replace(
        "int value = leaf();\n",
        "int value = leaf();\nint caller_three(void) { return leaf(); }\n",
    );
    let mut files = c_files();
    files[0] = ("main.c", with_new_caller.as_str());
    let directory = tempfile::tempdir().expect("a temporary directory");
    let indexed = files
        .iter()
        .map(|(name, text)| (format!("src/{name}"), *text))
        .collect::<Vec<_>>();
    let indexed = indexed
        .iter()
        .map(|(path, text)| (path.as_str(), *text))
        .collect::<Vec<_>>();
    let index = project::symbol_index_store::build_index_for_tests(directory.path(), &indexed)
        .expect("building the index");
    harness.project.update(cx, |project, cx| {
        project.symbol_index_store().update(cx, |store, cx| {
            store.insert_index_for_tests(worktree_id, index, cx);
            store.set_status_for_tests(
                worktree_id,
                SymbolIndexStatus::Indexing { done: 2, total: 4 },
                cx,
            );
        })
    });
    cx.executor().run_until_parked();
    set_status(SymbolIndexStatus::Indexing { done: 3, total: 4 }, cx);
    advance(INDEXING_RETRY_INTERVAL * 4, cx);
    assert_eq!(status_row(cx).as_deref(), Some("Indexing symbols 3 / 4"));
    assert_eq!(harness.rows(cx).len(), 5, "progress alone asks nothing");
    assert_eq!(root_retries(cx), Some((true, 0)));

    set_status(
        SymbolIndexStatus::Ready {
            stats: Default::default(),
        },
        cx,
    );
    assert_eq!(status_row(cx), None, "a built index needs no status row");
    advance(INDEXING_RETRY_INTERVAL, cx);
    assert_eq!(
        harness.rows(cx).last().map(String::as_str),
        Some("  caller_three [name match]"),
        "the end of the build asks again"
    );
    advance(INDEXING_RETRY_INTERVAL * 600, cx);
    assert_eq!(
        root_retries(cx),
        Some((false, 1)),
        "asked again once, and the new answer is complete"
    );
}

#[gpui::test]
async fn test_recent_buffers_are_capped_and_released(cx: &mut TestAppContext) {
    let (harness, cx) = setup(FakeGraph::default(), cx).await;
    let buffers = (0..RECENT_BUFFER_LIMIT + 1)
        .map(|index| cx.update(|_, cx| cx.new(|cx| Buffer::local(format!("{index}"), cx))))
        .collect::<Vec<_>>();
    let weak_buffers = buffers
        .iter()
        .map(|buffer| buffer.downgrade())
        .collect::<Vec<_>>();
    harness
        .panel
        .update(cx, |panel, cx| panel.keep_buffers(buffers, cx));
    cx.executor().run_until_parked();
    let alive = |cx: &mut VisualTestContext| {
        cx.executor().run_until_parked();
        weak_buffers
            .iter()
            .map(|buffer| buffer.upgrade().is_some())
            .collect::<Vec<_>>()
    };
    let mut expected = vec![true; RECENT_BUFFER_LIMIT + 1];
    if let Some(first) = expected.first_mut() {
        *first = false;
    }
    assert_eq!(alive(cx), expected, "the ninth buffer releases the first");

    advance(RECENT_BUFFER_LIFETIME - Duration::from_secs(1), cx);
    assert_eq!(alive(cx), expected, "kept until their time is up");
    advance(Duration::from_secs(1), cx);
    assert_eq!(alive(cx), vec![false; RECENT_BUFFER_LIMIT + 1]);
}

#[gpui::test]
async fn test_changing_the_excluded_callees_applies_to_the_tree(cx: &mut TestAppContext) {
    let (harness, cx, _index) = setup_c(false, FakeGraph::default(), cx).await;
    harness.show_callees_at(3, cx);
    assert_eq!(
        harness.rows(cx).last().map(String::as_str),
        Some("  [1 excluded]")
    );

    cx.update(|_, cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |settings| {
                settings
                    .call_graph_panel
                    .get_or_insert_default()
                    .excluded_callees = Some(vec!["missing_fn".to_string()]);
            });
        });
    });
    cx.executor().run_until_parked();
    let rows = harness.rows(cx);
    assert!(
        rows.contains(&"  printk [no definition]".to_string())
            && !rows.contains(&"  missing_fn [no definition]".to_string())
            && rows.last().map(String::as_str) == Some("  [1 excluded]"),
        "the new list is applied without asking for it: {rows:?}"
    );
}

#[gpui::test]
async fn test_a_status_change_other_than_the_end_of_a_build_asks_nothing(cx: &mut TestAppContext) {
    let (harness, cx, _index) = setup_c(false, FakeGraph::default(), cx).await;
    harness.show_callers_at(1, cx);
    assert_eq!(harness.rows(cx).len(), 5);
    harness.panel.update(cx, |panel, _| {
        panel.index_was_building = false;
        if let Some(root) = panel.tree.root()
            && let Some(node) = panel.tree.node_mut(root)
        {
            node.index_partial = true;
        }
    });
    let with_new_caller = MAIN_C.replace(
        "int value = leaf();\n",
        "int value = leaf();\nint caller_three(void) { return leaf(); }\n",
    );
    let mut files = c_files();
    files[0] = ("main.c", with_new_caller.as_str());
    let _updated = harness.install_index(&files, cx);
    advance(INDEXING_RETRY_INTERVAL * 4, cx);
    assert_eq!(
        harness.rows(cx).len(),
        5,
        "the index's status changed without a build ending, so nothing is asked again"
    );
}

#[gpui::test]
async fn test_a_name_match_opens_at_its_byte_column(cx: &mut TestAppContext) {
    // The index counts columns in UTF-8 bytes of the file on disk, which has
    // Windows line endings here; a language server would count UTF-16 units.
    let source = "int leaf(void) { return 0; }\r\nint caller(void) { /* 日本語の説明 */ return leaf(); }\r\n";
    let (harness, cx) = setup_project(
        json!({ "main.c": source }),
        "src/main.c",
        c_lang(),
        false,
        FakeGraph::default(),
        cx,
    )
    .await;
    let _index = harness.install_index(&[("main.c", source)], cx);
    harness.show_callers_at(0, cx);
    assert_eq!(
        harness.rows(cx),
        vec!["leaf [name match]", "  caller [name match]"]
    );
    harness.select("caller", cx);
    harness.dispatch(OpenSelected, cx);
    let line = source.lines().nth(1).expect("the second line");
    let column = line.find("leaf()").expect("the call") as u32;
    let editor = harness
        .workspace
        .read_with(cx, |workspace, cx| workspace.active_item_as::<Editor>(cx))
        .expect("an editor is active");
    let cursor = editor.update(cx, |editor, cx| {
        editor
            .selections
            .newest::<Point>(&editor.display_snapshot(cx))
            .head()
    });
    assert_eq!(cursor, Point::new(1, column));
}

#[gpui::test]
async fn test_removing_a_folder_releases_its_recent_buffers(cx: &mut TestAppContext) {
    let (harness, cx) = setup(FakeGraph::default(), cx).await;
    let buffer = harness
        .project
        .update(cx, |project, cx| {
            project.open_buffer((harness.worktree_id, rel_path("src/other.rs")), cx)
        })
        .await
        .expect("opening other.rs");
    let weak_buffer = buffer.downgrade();
    harness
        .panel
        .update(cx, |panel, cx| panel.keep_buffers(vec![buffer], cx));
    cx.executor().run_until_parked();
    assert!(weak_buffer.upgrade().is_some(), "kept for a while");

    let worktree_id = harness.worktree_id;
    harness.project.update(cx, |project, cx| {
        project.remove_worktree(worktree_id, cx);
    });
    cx.executor().run_until_parked();
    assert!(
        weak_buffer.upgrade().is_none(),
        "a removed folder's files are not kept open"
    );
}

#[test]
fn test_index_status_summary() {
    use project::symbol_index_store::SymbolIndexStats;
    let ready = |files, definitions| SymbolIndexStatus::Ready {
        stats: SymbolIndexStats {
            files,
            definitions,
            ..SymbolIndexStats::default()
        },
    };
    assert_eq!(index_summary(&[]), None);
    assert_eq!(
        index_summary(&[SymbolIndexStatus::Disabled]),
        Some(IndexSummary::Disabled)
    );
    assert_eq!(
        index_summary(&[ready(1, 2), SymbolIndexStatus::Disabled, ready(10, 20)]),
        Some(IndexSummary::Ready {
            files: 11,
            definitions: 22
        })
    );
    assert_eq!(
        index_summary(&[
            ready(1, 2),
            SymbolIndexStatus::Indexing {
                done: 12_000,
                total: 50_000
            },
            SymbolIndexStatus::Indexing {
                done: 345,
                total: 10_000
            },
        ]),
        Some(IndexSummary::Building {
            done: 12_345,
            total: 60_000
        })
    );
    assert_eq!(
        index_summary(&[ready(1, 2), SymbolIndexStatus::Scanning]),
        Some(IndexSummary::Preparing)
    );
    assert_eq!(
        index_summary(&[
            SymbolIndexStatus::Unavailable {
                reason: SymbolIndexUnavailable::InUseByAnotherWindow
            },
            SymbolIndexStatus::Indexing { done: 1, total: 2 },
        ]),
        Some(IndexSummary::Building { done: 1, total: 2 }),
        "an index still being built matters more than one that cannot be used"
    );
    assert_eq!(
        index_summary(&[
            ready(1, 2),
            SymbolIndexStatus::Unavailable {
                reason: SymbolIndexUnavailable::InUseByAnotherWindow
            }
        ]),
        Some(IndexSummary::Unavailable(
            SymbolIndexUnavailable::InUseByAnotherWindow
        ))
    );
    assert_eq!(group_digits(60_000), "60,000");
    assert_eq!(group_digits(999), "999");
    assert_eq!(group_digits(1_234_567), "1,234,567");
    assert_eq!(
        index_summary_text(&IndexSummary::Building {
            done: 12_345,
            total: 60_000
        })
        .map(|(message, _)| message),
        Some("Indexing symbols 12,345 / 60,000".to_string())
    );
    assert_eq!(
        index_summary_text(&IndexSummary::Ready {
            files: 1,
            definitions: 2
        }),
        None,
        "a ready index needs no status line"
    );
}

#[test]
fn test_index_texts_are_translated() {
    let _japanese = i18n::test_support::LocaleOverride::new(Some(
        i18n_catalog::test_support::bundled_catalog("ja"),
    ));
    let message = |summary: IndexSummary| {
        index_summary_text(&summary)
            .map(|(message, _)| message)
            .unwrap_or_default()
    };
    assert_eq!(
        message(IndexSummary::Building {
            done: 12_345,
            total: 60_000
        }),
        "索引を作成中 12,345 / 60,000"
    );
    assert_eq!(message(IndexSummary::Disabled), "索引は無効です");
    assert_eq!(
        message(IndexSummary::Unavailable(
            SymbolIndexUnavailable::InUseByAnotherWindow
        )),
        "索引: 別のウィンドウが使用中"
    );
    assert_eq!(
        message(IndexSummary::Unavailable(
            SymbolIndexUnavailable::UnsupportedHost
        )),
        "索引: このサーバーは未対応"
    );
    assert_eq!(origin_label(CallOrigin::NameMatch), "名前一致");
    assert_eq!(
        origin_label(CallOrigin::FunctionPointerCandidate),
        "関数ポインタ候補"
    );
    assert_eq!(origin_label(CallOrigin::Macro), "マクロ");
    assert_eq!(excluded_label(3), "3 件を除外");
    assert_eq!(truncated_label(), "先頭 5,000 件のみ");
    assert_eq!(
        row_note(SymbolRole::SameNameDefinitions(2)).as_deref(),
        Some("2 件の同名定義")
    );
    assert_eq!(
        row_note(SymbolRole::Unresolved).as_deref(),
        Some("定義が見つかりません")
    );
    assert_eq!(source_filter_label(SourceFilter::All), "すべて");
    assert_eq!(
        source_filter_label(SourceFilter::LanguageServer),
        "言語サーバーのみ"
    );
    assert_eq!(source_filter_label(SourceFilter::NameMatch), "名前一致のみ");
    assert_eq!(i18n::tc!("call graph panel", "Refresh"), "更新");
}

/// The first build of a large tree takes a while: its progress shows while no root has been
/// asked for and the open file is not C, and goes once the index is built. The states that
/// only matter for C (the index is off, say) stay out of the way.
#[gpui::test]
async fn test_an_index_being_built_shows_whatever_the_panel_shows(cx: &mut TestAppContext) {
    let (harness, cx) = setup_project(
        json!({ "main.rs": MAIN_SOURCE }),
        "src/main.rs",
        rust_lang(),
        false,
        FakeGraph::default(),
        cx,
    )
    .await;
    let _index = harness.install_index(&[("main.c", MAIN_C)], cx);
    harness.workspace.update_in(cx, |workspace, window, cx| {
        workspace.open_panel::<CallGraphPanel>(window, cx)
    });
    let worktree_id = harness.worktree_id;
    let set_status = |status: SymbolIndexStatus, cx: &mut VisualTestContext| {
        harness.project.update(cx, |project, cx| {
            project.symbol_index_store().update(cx, |store, cx| {
                store.set_status_for_tests(worktree_id, status, cx)
            })
        });
        cx.update(|window, _| window.refresh());
        cx.executor().run_until_parked();
    };
    let shown = |cx: &mut VisualTestContext| {
        harness
            .panel
            .read_with(cx, |panel, cx| panel.shown_index_summary(cx))
    };
    let status_row_bounds = |cx: &mut VisualTestContext| cx.debug_bounds("call-graph-index-status");

    assert!(
        harness
            .panel
            .read_with(cx, |panel, _| panel.root_symbol().is_none()),
        "no root has been asked for"
    );
    set_status(
        SymbolIndexStatus::Indexing {
            done: 12_345,
            total: 60_000,
        },
        cx,
    );
    assert_eq!(
        shown(cx),
        Some(IndexSummary::Building {
            done: 12_345,
            total: 60_000
        }),
        "the progress shows with a Rust file open and no root"
    );
    let bounds = status_row_bounds(cx).expect("the status row is drawn");
    let panel_bounds = cx
        .debug_bounds("call-graph-panel")
        .expect("the panel is drawn");
    assert!(bounds.size.height > px(0.), "{bounds:?}");
    assert!(
        bounds.top() >= panel_bounds.top() && bounds.bottom() <= panel_bounds.bottom(),
        "the row is inside the panel, above the empty state: {bounds:?} in {panel_bounds:?}"
    );

    set_status(SymbolIndexStatus::Scanning, cx);
    assert_eq!(shown(cx), Some(IndexSummary::Preparing));

    set_status(
        SymbolIndexStatus::Ready {
            stats: Default::default(),
        },
        cx,
    );
    assert_eq!(shown(cx), None, "a built index needs no status row");
    assert_eq!(status_row_bounds(cx), None);

    set_status(SymbolIndexStatus::Disabled, cx);
    assert_eq!(
        shown(cx),
        None,
        "that the index is off only matters where it answers"
    );
}

/// Outside a git repository, folders deeper than `file_scan_depth` are not scanned, so their
/// files are not in the index: the status row says so, and goes once the depth no longer
/// leaves anything out.
#[gpui::test]
async fn test_folders_past_the_scan_depth_are_pointed_out(cx: &mut TestAppContext) {
    init_test(cx);
    let set_scan_depth = |depth: u32, cx: &mut App| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |settings| {
                settings.project.worktree.file_scan_depth = Some(depth);
            });
        });
    };
    cx.update(|cx| set_scan_depth(2, cx));
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/test"),
        json!({
            "src": {
                "main.c": MAIN_C,
                "deep": { "inner": { "more.c": "int more(void) { return 0; }\n" } },
            },
        }),
    )
    .await;
    let project = Project::test(fs, [path!("/test").as_ref()], cx).await;
    project.read_with(cx, |project, _| project.languages().add(c_lang()));
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace =
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
    let worktree_id = project.read_with(cx, |project, cx| {
        project
            .worktrees(cx)
            .next()
            .expect("the test worktree")
            .read(cx)
            .id()
    });
    let panel = workspace.update_in(cx, |workspace, window, cx| {
        let panel = CallGraphPanel::new(workspace, window, cx);
        workspace.add_panel(panel.clone(), window, cx);
        workspace.open_panel::<CallGraphPanel>(window, cx);
        panel
    });
    let directory = tempfile::tempdir().expect("a temporary directory");
    let index = project::symbol_index_store::build_index_for_tests(
        directory.path(),
        &[("src/main.c", MAIN_C)],
    )
    .expect("building the index");
    project.update(cx, |project, cx| {
        project.symbol_index_store().update(cx, |store, cx| {
            store.insert_index_for_tests(worktree_id, index, cx);
            store.set_status_for_tests(
                worktree_id,
                SymbolIndexStatus::Indexing { done: 1, total: 2 },
                cx,
            );
        })
    });
    let notice_shown = |cx: &mut VisualTestContext| {
        cx.update(|window, _| window.refresh());
        cx.executor().run_until_parked();
        cx.debug_bounds("call-graph-scan-depth-notice").is_some()
    };

    assert!(
        notice_shown(cx),
        "src/deep is past the depth of 2: {:?}",
        panel.read_with(cx, |panel, cx| panel.shown_index_summary(cx))
    );

    cx.update(|_, cx| set_scan_depth(8, cx));
    cx.executor().run_until_parked();
    let deferred = project.read_with(cx, |project, cx| {
        project
            .worktree_for_id(worktree_id, cx)
            .map(|worktree| worktree.read(cx).deferred_scan_dir_count())
    });
    assert_eq!(deferred, Some(0), "a deeper scan reaches every folder");
    assert!(!notice_shown(cx), "nothing is left out at a depth of 8");

    let _locale = i18n::test_support::LocaleOverride::new(Some(
        i18n_catalog::test_support::bundled_catalog("ja") as &'static dyn i18n::Catalog,
    ));
    assert_ne!(
        scan_depth_notice(),
        "Folders deeper than the file scan depth outside git repositories are not in the symbol index."
    );
}

/// A name longer than the panel is wide wraps: the row grows taller, and neither the name nor
/// the location beside it reaches past the panel's edge, at any depth.
#[gpui::test]
async fn test_a_long_name_wraps_inside_the_panel(cx: &mut TestAppContext) {
    const LONG_NAME: &str =
        "compute_the_weighted_average_of_all_the_leaves_in_the_tree_and_then_some_more";
    let (harness, cx) = setup(
        graph_with_incoming(vec![("helper", vec![call(LONG_NAME, "main.rs", 0)])]),
        cx,
    )
    .await;
    harness.show_callers_at(1, cx);
    assert_eq!(
        harness.rows(cx),
        vec!["helper".to_string(), format!("  {LONG_NAME}")]
    );
    cx.update(|window, _| window.refresh());
    cx.executor().run_until_parked();

    let panel = cx
        .debug_bounds("call-graph-panel")
        .expect("the panel is drawn");
    let id = harness.panel.read_with(cx, |panel, _| {
        panel
            .rows
            .iter()
            .find_map(|row| match row {
                Row::Node { id, .. }
                    if panel
                        .tree
                        .node(*id)
                        .is_some_and(|node| node.symbol.name == LONG_NAME) =>
                {
                    Some(id.index())
                }
                _ => None,
            })
            .expect("the long row")
    });
    let short_id = harness.panel.read_with(cx, |panel, _| {
        panel
            .rows
            .first()
            .map(|row| row.node().index())
            .expect("the root row")
    });
    let names = cx
        .debug_bounds(format!("call-graph-row-names-{id}").leak())
        .expect("the names are drawn");
    let location = cx
        .debug_bounds(format!("call-graph-row-location-{id}").leak())
        .expect("the location is drawn");
    let short_names = cx
        .debug_bounds(format!("call-graph-row-names-{short_id}").leak())
        .expect("the root's names are drawn");
    assert!(
        names.size.height > short_names.size.height * 1.5,
        "the long name wraps: {names:?}, a short one is {short_names:?}"
    );
    let short_location = cx
        .debug_bounds(format!("call-graph-row-location-{short_id}").leak())
        .expect("the root's location is drawn");
    assert!(
        names.right() <= location.left() && location.right() <= panel.right(),
        "the name, then the location, inside the panel: {names:?} {location:?} in {panel:?}"
    );
    assert_eq!(
        location.right(),
        short_location.right(),
        "locations line up at every depth"
    );
}
