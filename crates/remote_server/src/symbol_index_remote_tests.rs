//! ADR-F68: a client of a remote server asks the server's symbol index.

use std::time::Duration;

use fs::FakeFs;
use futures::FutureExt as _;
use gpui::{AppContext as _, Entity, TestAppContext};
use project::{
    Project, ProjectPath, SymbolIndexStore, SymbolIndexUnavailable, WorktreeId,
    symbol_index_store::{
        ChainRootOrigin, LeaveSymbolIndexUnansweredForTests, MAX_TYPE_WINDOW_BYTES,
        MemberExpansionTarget, MemberSelector, SearchMode, SymbolIndexStatus, TypeHoverSubject,
        TypeTreeOptions, TypeTreeSubject, build_index_for_tests,
    },
};
use serde_json::json;
use util::{path, rel_path::rel_path};

use crate::{HeadlessProject, remote_editing_tests::init_test};

const MAIN_C: &str = "\
static int helper(void) { return 1; }
int foo(void) { return helper(); }
int main(void) { return foo(); }
";

fn client_store(project: &Entity<Project>, cx: &mut TestAppContext) -> Entity<SymbolIndexStore> {
    project.read_with(cx, |project, _| project.symbol_index_store().clone())
}

async fn add_worktree(project: &Entity<Project>, cx: &mut TestAppContext) -> WorktreeId {
    let (worktree, _) = project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path!("/code/project1"), true, cx)
        })
        .await
        .expect("adding the worktree");
    cx.executor().run_until_parked();
    worktree.read_with(cx, |worktree, _| worktree.id())
}

/// The server builds its index on its own threads, so the test waits in real
/// time, moving both clocks past the debounce and the request timeout.
fn wait_until(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
    what: &str,
    mut condition: impl FnMut(&mut TestAppContext) -> bool,
) {
    for _ in 0..1000 {
        server_cx
            .executor()
            .advance_clock(Duration::from_millis(100));
        server_cx.run_until_parked();
        cx.run_until_parked();
        if condition(cx) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting until {what}");
}

fn set_server_index_root(
    headless: &Entity<HeadlessProject>,
    root: &std::path::Path,
    server_cx: &mut TestAppContext,
) {
    let store = headless.read_with(server_cx, |headless, _| headless.symbol_index_store.clone());
    store.update(server_cx, |store, cx| {
        store.set_index_root_for_tests(Some(root.to_path_buf()), cx)
    });
}

#[gpui::test]
async fn test_remote_symbol_index_answers_the_client(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    server_cx.executor().allow_parking();
    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        path!("/code"),
        json!({ "project1": { "main.c": MAIN_C, "other.c": "void other(void) { foo(); }\n" } }),
    )
    .await;
    let (project, headless) = init_test(&fs, cx, server_cx).await;
    let index_root = tempfile::tempdir().expect("a temporary directory");
    set_server_index_root(&headless, index_root.path(), server_cx);
    let worktree_id = add_worktree(&project, cx).await;
    let store = client_store(&project, cx);

    wait_until(cx, server_cx, "the client hears the index is ready", |cx| {
        matches!(
            store.read_with(cx, |store, _| store.status(worktree_id)),
            SymbolIndexStatus::Ready { .. }
        )
    });
    let SymbolIndexStatus::Ready { stats } =
        store.read_with(cx, |store, _| store.status(worktree_id))
    else {
        panic!("the index should be ready");
    };
    assert_eq!(stats.files, 2);

    let near = ProjectPath {
        worktree_id,
        path: rel_path("main.c").into_arc(),
    };
    let definitions = store
        .update(cx, |store, cx| store.definitions("helper", Some(near), cx))
        .await
        .expect("definitions");
    assert_eq!(
        definitions
            .iter()
            .map(|definition| (
                definition.path.clone(),
                definition.name_range.start.row,
                definition.is_static
            ))
            .collect::<Vec<_>>(),
        vec![(
            ProjectPath {
                worktree_id,
                path: rel_path("main.c").into_arc()
            },
            0,
            true
        )]
    );

    let callers = store
        .update(cx, |store, cx| store.callers("foo", 10, cx))
        .await
        .expect("callers");
    let mut callers = callers
        .calls
        .iter()
        .map(|call| {
            call.caller
                .as_ref()
                .map(|caller| caller.name.clone())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>();
    callers.sort();
    assert_eq!(callers, vec!["main".to_string(), "other".to_string()]);

    let limited = store
        .update(cx, |store, cx| store.callers("foo", 1, cx))
        .await
        .expect("callers");
    assert_eq!(limited.calls.len(), 1);
    assert!(
        limited.truncated,
        "the limit and `truncated` cross the wire"
    );

    let found = store
        .update(cx, |store, cx| {
            store.search("mai", SearchMode::Prefix, 10, cx)
        })
        .await
        .expect("search");
    assert_eq!(
        found
            .iter()
            .map(|found| found.definition.name.as_str())
            .collect::<Vec<_>>(),
        vec!["main"]
    );

    let callees = store
        .update(cx, |store, cx| {
            store.callees(
                ProjectPath {
                    worktree_id,
                    path: rel_path("main.c").into_arc(),
                },
                language::Point::new(1, 20),
                cx,
            )
        })
        .await
        .expect("callees")
        .expect("foo contains the position");
    assert_eq!(callees.function.name, "foo");
    assert_eq!(callees.callees.len(), 1);

    // A change on the server reaches the client's status and answers.
    fs.insert_file(
        path!("/code/project1/late.c"),
        b"int late(void) { return 0; }\n".to_vec(),
    )
    .await;
    let mut found_late = false;
    for _ in 0..1000 {
        server_cx
            .executor()
            .advance_clock(Duration::from_millis(100));
        server_cx.run_until_parked();
        cx.run_until_parked();
        let late = store
            .update(cx, |store, cx| store.definitions("late", None, cx))
            .await
            .expect("definitions");
        if !late.is_empty() {
            found_late = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(found_late, "the new file is found from the client");
}

#[gpui::test]
async fn test_remote_symbol_index_from_an_older_server_is_unavailable(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(path!("/code"), json!({ "project1": { "main.c": MAIN_C } }))
        .await;
    server_cx.update(|cx| cx.set_global(LeaveSymbolIndexUnansweredForTests));
    let (project, _headless) = init_test(&fs, cx, server_cx).await;
    let worktree_id = add_worktree(&project, cx).await;
    let store = client_store(&project, cx);
    assert_eq!(
        store.read_with(cx, |store, _| store.status(worktree_id)),
        SymbolIndexStatus::Scanning,
        "the client does not know yet"
    );
    // While the first status request is out, a "go to definition" or a
    // symbol search must not wait for the server that never answers.
    let definitions = store.update(cx, |store, cx| store.definitions("foo", None, cx));
    let search = store.update(cx, |store, cx| {
        store.search("fo", SearchMode::Prefix, 10, cx)
    });
    cx.run_until_parked();
    assert!(
        definitions
            .now_or_never()
            .expect("answered without waiting")
            .expect("no error")
            .is_empty()
    );
    assert!(
        search
            .now_or_never()
            .expect("answered without waiting")
            .expect("no error")
            .is_empty()
    );

    cx.executor().advance_clock(Duration::from_secs(20));
    cx.run_until_parked();
    assert_eq!(
        store.read_with(cx, |store, _| store.status(worktree_id)),
        SymbolIndexStatus::Unavailable {
            reason: SymbolIndexUnavailable::UnsupportedHost
        }
    );
    let definitions = store
        .update(cx, |store, cx| store.definitions("foo", None, cx))
        .await
        .expect("an unavailable index answers with nothing rather than an error");
    assert!(definitions.is_empty());
}

const TYPES_H: &str = "\
struct inner { int depth; };
struct outer {
    struct inner in;
    struct { int a; } anon;
};
typedef struct outer outer_t;
";

const USE_C: &str = "\
#include \"types.h\"
int use(outer_t *value)
{
    return value->in.depth;
}
";

/// A server whose index of `/code/project1` was built by the test, and a client that knows
/// the server has one.
async fn types_setup(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) -> (
    Entity<Project>,
    Entity<HeadlessProject>,
    WorktreeId,
    tempfile::TempDir,
) {
    cx.executor().allow_parking();
    server_cx.executor().allow_parking();
    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        path!("/code"),
        json!({ "project1": { "types.h": TYPES_H, "use.c": USE_C } }),
    )
    .await;
    let (project, headless) = init_test(&fs, cx, server_cx).await;
    let worktree_id = add_worktree(&project, cx).await;
    server_cx.run_until_parked();
    let index_directory = tempfile::tempdir().expect("a temporary directory");
    let index = build_index_for_tests(
        index_directory.path(),
        &[("types.h", TYPES_H), ("use.c", USE_C)],
    )
    .expect("building the index");
    let server_store =
        headless.read_with(server_cx, |headless, _| headless.symbol_index_store.clone());
    server_store.update(server_cx, |store, cx| {
        store.insert_index_for_tests(worktree_id, index, cx)
    });
    let store = client_store(&project, cx);
    wait_until(cx, server_cx, "the client hears the index is ready", |cx| {
        matches!(
            store.read_with(cx, |store, _| store.status(worktree_id)),
            SymbolIndexStatus::Ready { .. }
        )
    });
    (project, headless, worktree_id, index_directory)
}

#[gpui::test]
async fn test_remote_type_queries_answer_the_client(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let (project, _headless, worktree_id, _index_directory) = types_setup(cx, server_cx).await;
    let store = client_store(&project, cx);
    let use_c = ProjectPath {
        worktree_id,
        path: rel_path("use.c").into_arc(),
    };

    let hover = store
        .update(cx, |store, cx| {
            store.describe_type_at(
                use_c.clone(),
                language::Point::new(3, 21),
                None,
                None,
                1,
                cx,
            )
        })
        .await
        .expect("describe_type_at")
        .expect("a hover");
    assert_eq!(hover.name, "depth");
    let TypeHoverSubject::Member {
        steps,
        complete,
        root_origin,
        root_name,
        ..
    } = &hover.subject
    else {
        panic!("{:#?}", hover.subject);
    };
    assert!(complete);
    assert_eq!(steps.len(), 2);
    assert_eq!(
        (*root_origin, root_name.as_deref()),
        (Some(ChainRootOrigin::InFile), Some("value")),
        "where the root's type came from crosses the wire"
    );

    let resolved = store
        .update(cx, |store, cx| {
            store.resolve_type("outer_t", None, Some(use_c.clone()), 1, cx)
        })
        .await
        .expect("resolve_type");
    let outer = resolved.types.first().expect("the definition");
    assert_eq!(outer.name, "struct outer");
    assert_eq!(outer.typedef_chain.len(), 1);
    assert_eq!(
        outer
            .members
            .iter()
            .map(|member| member.name.as_deref())
            .collect::<Vec<_>>(),
        vec![Some("in"), Some("anon")]
    );
    assert!(
        outer.members[1].inline.is_some(),
        "bodies in place cross the wire"
    );

    let owner = outer.type_ref.clone().expect("a reference");
    let expansion = store
        .update(cx, |store, cx| {
            store.expand_member(owner.clone(), MemberSelector::Name("in".to_string()), 1, cx)
        })
        .await
        .expect("expand_member");
    let MemberExpansionTarget::Types(types) = &expansion.target else {
        panic!("{expansion:#?}");
    };
    assert_eq!(types.types[0].name, "struct inner");
    let inline = store
        .update(cx, |store, cx| {
            store.expand_member(owner.clone(), MemberSelector::Path(vec![1]), 1, cx)
        })
        .await
        .expect("expand_member");
    assert!(matches!(inline.target, MemberExpansionTarget::Inline(_)));

    let tree = store
        .update(cx, |store, cx| {
            store.type_tree_text(TypeTreeSubject::Ref(owner), TypeTreeOptions::default(), cx)
        })
        .await
        .expect("type_tree_text")
        .expect("a tree");
    assert!(tree.text.contains("int depth"), "{}", tree.text);
    assert!(
        tree.definitions
            .iter()
            .all(|(path, _)| path.worktree_id == worktree_id),
        "{:?}",
        tree.definitions
    );
}

/// A client's edits not saved yet reach the host's buffer, which the host reads instead of the
/// file on disk.
#[gpui::test]
async fn test_remote_type_queries_read_the_clients_unsaved_edits(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let (project, _headless, worktree_id, _index_directory) = types_setup(cx, server_cx).await;
    let store = client_store(&project, cx);
    let resolved = store
        .update(cx, |store, cx| {
            store.resolve_type("outer", None, None, 1, cx)
        })
        .await
        .expect("resolve_type");
    let owner = resolved.types[0].type_ref.clone().expect("a reference");
    let expand_extra = async |cx: &mut TestAppContext| {
        let owner = owner.clone();
        store
            .update(cx, |store, cx| {
                store.expand_member(owner, MemberSelector::Name("extra".to_string()), 1, cx)
            })
            .await
            .expect("expand_member")
    };
    assert_eq!(
        expand_extra(cx).await.target,
        MemberExpansionTarget::NotFound
    );

    let buffer = project
        .update(cx, |project, cx| {
            project.open_buffer(
                ProjectPath {
                    worktree_id,
                    path: rel_path("types.h").into_arc(),
                },
                cx,
            )
        })
        .await
        .expect("opening the header");
    let anchor = "    struct inner in;\n";
    let offset = TYPES_H.find(anchor).expect("the member") + anchor.len();
    buffer.update(cx, |buffer, cx| {
        buffer.edit([(offset..offset, "    struct inner extra;\n")], None, cx)
    });
    cx.run_until_parked();
    server_cx.run_until_parked();

    let extra = expand_extra(cx).await;
    let MemberExpansionTarget::Types(types) = &extra.target else {
        panic!("{extra:#?}");
    };
    assert_eq!(types.types[0].name, "struct inner");
}

#[gpui::test]
async fn test_remote_hover_sends_only_a_window_of_the_buffer(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let (project, _headless, worktree_id, _index_directory) = types_setup(cx, server_cx).await;
    let store = client_store(&project, cx);
    // Not saved: a local variable only the client's buffer has, far below a long comment
    // that does not fit in the window.
    let padding = format!("/* {} */\n", "x".repeat(MAX_TYPE_WINDOW_BYTES * 2));
    let edited = format!(
        "#include \"types.h\"\n{padding}int use(outer_t *value)\n{{\n    struct inner fresh;\n    return fresh.depth;\n}}\n"
    );
    let buffer = cx.new(|cx| language::Buffer::local(edited.clone(), cx));
    let snapshot = buffer.read_with(cx, |buffer, _| buffer.snapshot());
    let row = edited
        .lines()
        .position(|line| line.contains("return fresh"))
        .expect("row") as u32;
    let hover = store
        .update(cx, |store, cx| {
            store.describe_type_at(
                ProjectPath {
                    worktree_id,
                    path: rel_path("use.c").into_arc(),
                },
                language::Point::new(row, 11),
                Some(snapshot),
                None,
                1,
                cx,
            )
        })
        .await
        .expect("describe_type_at")
        .expect("a hover");
    assert_eq!(hover.name, "fresh");
    assert_eq!(
        hover.range.start,
        language::Point::new(row, 11),
        "rows of the buffer"
    );
    match &hover.subject {
        TypeHoverSubject::Variable {
            core_name,
            declaration,
            ..
        } => {
            assert_eq!(core_name.as_deref(), Some("inner"));
            assert_eq!(
                declaration.as_ref().map(|range| range.start.row),
                Some(row - 1)
            );
        }
        other => panic!("{other:#?}"),
    }
    assert_eq!(
        hover
            .types
            .types
            .first()
            .map(|summary| summary.name.as_str()),
        Some("struct inner"),
        "the type is found on the server from its index and disk"
    );
}

#[gpui::test]
async fn test_remote_type_queries_of_an_older_server_answer_at_once(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(path!("/code"), json!({ "project1": { "use.c": USE_C } }))
        .await;
    server_cx.update(|cx| cx.set_global(LeaveSymbolIndexUnansweredForTests));
    let (project, _headless) = init_test(&fs, cx, server_cx).await;
    let worktree_id = add_worktree(&project, cx).await;
    let store = client_store(&project, cx);
    let check = |cx: &mut TestAppContext| {
        let path = ProjectPath {
            worktree_id,
            path: rel_path("use.c").into_arc(),
        };
        let hover = store.update(cx, |store, cx| {
            store.describe_type_at(path, language::Point::new(3, 18), None, None, 1, cx)
        });
        let resolved = store.update(cx, |store, cx| {
            store.resolve_type("outer", None, None, 1, cx)
        });
        let tree = store.update(cx, |store, cx| {
            store.type_tree_text(
                TypeTreeSubject::Name {
                    name: "outer".to_string(),
                    tag: None,
                    near: None,
                },
                TypeTreeOptions::default(),
                cx,
            )
        });
        cx.run_until_parked();
        assert!(
            hover
                .now_or_never()
                .expect("answered without waiting")
                .expect("no error")
                .is_none()
        );
        assert!(
            resolved
                .now_or_never()
                .expect("answered without waiting")
                .expect("no error")
                .types
                .is_empty()
        );
        assert!(
            tree.now_or_never()
                .expect("answered without waiting")
                .expect("no error")
                .is_none()
        );
    };
    // While the status request is out, and after it timed out.
    check(cx);
    cx.executor().advance_clock(Duration::from_secs(20));
    cx.run_until_parked();
    assert_eq!(
        store.read_with(cx, |store, _| store.status(worktree_id)),
        SymbolIndexStatus::Unavailable {
            reason: SymbolIndexUnavailable::UnsupportedHost
        }
    );
    check(cx);
}

#[gpui::test]
async fn test_remote_symbol_index_status_is_asked_again_after_a_reconnect(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    server_cx.executor().allow_parking();
    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(path!("/code"), json!({ "project1": { "main.c": MAIN_C } }))
        .await;
    // The server answers nothing at first, so the client gives up on it.
    server_cx.update(|cx| cx.set_global(LeaveSymbolIndexUnansweredForTests));
    let (project, headless) = init_test(&fs, cx, server_cx).await;
    let worktree_id = add_worktree(&project, cx).await;
    let store = client_store(&project, cx);
    cx.executor().advance_clock(Duration::from_secs(20));
    cx.run_until_parked();
    assert_eq!(
        store.read_with(cx, |store, _| store.status(worktree_id)),
        SymbolIndexStatus::Unavailable {
            reason: SymbolIndexUnavailable::UnsupportedHost
        }
    );

    // The server gets an index without telling anyone, then answers again.
    let index_directory = tempfile::tempdir().expect("a temporary directory");
    let index = build_index_for_tests(index_directory.path(), &[("main.c", MAIN_C)])
        .expect("building the index");
    let server_store =
        headless.read_with(server_cx, |headless, _| headless.symbol_index_store.clone());
    server_store.update(server_cx, |store, cx| {
        store.insert_index_for_tests(worktree_id, index, cx)
    });
    server_cx.update(|cx| cx.remove_global::<LeaveSymbolIndexUnansweredForTests>());
    server_cx.run_until_parked();
    cx.run_until_parked();
    assert!(
        matches!(
            store.read_with(cx, |store, _| store.status(worktree_id)),
            SymbolIndexStatus::Unavailable { .. }
        ),
        "nothing tells the client"
    );

    let client = cx.read(|cx| project.read(cx).remote_client().expect("a remote project"));
    client
        .update(cx, |client, cx| client.simulate_disconnect(cx))
        .detach();
    wait_until(cx, server_cx, "the client hears the index is ready", |cx| {
        matches!(
            store.read_with(cx, |store, _| store.status(worktree_id)),
            SymbolIndexStatus::Ready { .. }
        )
    });
    let definitions = store
        .update(cx, |store, cx| store.definitions("foo", None, cx))
        .await
        .expect("definitions");
    assert_eq!(definitions.len(), 1);

    // A server that answered before keeps its index through a reconnect it is slow after.
    server_cx.update(|cx| cx.set_global(LeaveSymbolIndexUnansweredForTests));
    client
        .update(cx, |client, cx| client.simulate_disconnect(cx))
        .detach();
    cx.run_until_parked();
    server_cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_secs(20));
    cx.run_until_parked();
    assert!(
        matches!(
            store.read_with(cx, |store, _| store.status(worktree_id)),
            SymbolIndexStatus::Ready { .. }
        ),
        "{:?}",
        store.read_with(cx, |store, _| store.status(worktree_id))
    );
}
