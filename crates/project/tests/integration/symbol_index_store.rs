//! ADR-F68: the project's name-based symbol index of C and C++ files.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use fs::{FakeFs, Fs as _, RemoveOptions, RenameOptions};
use gpui::{Entity, TestAppContext, UpdateGlobal as _};
use project::{
    Project, ProjectPath, SymbolIndexStore, SymbolIndexUnavailable, WorktreeId,
    symbol_index_store::{
        CalleeTarget, IndexedDefinition, SearchMode, SymbolIndexStatus, build_index_for_tests,
    },
};
use serde_json::json;
use settings::{SettingsStore, SymbolIndexEnabled, SymbolIndexSettingsContent};
use text::Point;
use util::{path, rel_path::rel_path};

use crate::init_test;

const A_C: &str = "\
#include \"inc/x.h\"
static int helper(int value) { return value + 1; }
int foo(int value) {
    return helper(value);
}
void bar(void) { foo(1); }
";

const B_C: &str = "\
#include \"inc/x.h\"
static int helper(int value) { return value * 2; }
int main(void) { return foo(helper(2)); }
";

const X_H: &str = "\
int foo(int value);
struct ops { int (*open)(void); };
int real_open(void);
static struct ops default_ops = { .open = real_open };
static inline int shared_inline(int value) { return value; }
";

fn project_files() -> serde_json::Value {
    json!({
        "a.c": A_C,
        "b.c": B_C,
        "inc": { "x.h": X_H },
        "README.md": "not C",
    })
}

async fn new_project(
    tree: serde_json::Value,
    cx: &mut TestAppContext,
) -> (Entity<Project>, Arc<FakeFs>, WorktreeId) {
    cx.executor().allow_parking();
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/root"), tree).await;
    let project = Project::test(fs.clone(), [Path::new(path!("/root"))], cx).await;
    let worktree_id = project.read_with(cx, |project, cx| {
        project
            .worktrees(cx)
            .next()
            .expect("the project has a worktree")
            .read(cx)
            .id()
    });
    (project, fs, worktree_id)
}

fn store(project: &Entity<Project>, cx: &mut TestAppContext) -> Entity<SymbolIndexStore> {
    project.read_with(cx, |project, _| project.symbol_index_store().clone())
}

fn set_index_root(project: &Entity<Project>, root: &Path, cx: &mut TestAppContext) {
    let store = store(project, cx);
    store.update(cx, |store, cx| {
        store.set_index_root_for_tests(Some(root.to_path_buf()), cx)
    });
}

fn status(
    project: &Entity<Project>,
    worktree_id: WorktreeId,
    cx: &mut TestAppContext,
) -> SymbolIndexStatus {
    let store = store(project, cx);
    store.read_with(cx, |store, _| store.status(worktree_id))
}

/// The index is built on its own threads, so the test waits in real time.
fn wait_until(
    cx: &mut TestAppContext,
    what: &str,
    mut condition: impl FnMut(&mut TestAppContext) -> bool,
) {
    for _ in 0..1000 {
        // File events wait out a debounce timer before the index reads them.
        cx.executor().advance_clock(Duration::from_millis(100));
        cx.run_until_parked();
        if condition(cx) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting until {what}");
}

fn wait_for_ready(project: &Entity<Project>, worktree_id: WorktreeId, cx: &mut TestAppContext) {
    wait_until(cx, "the index is ready", |cx| {
        matches!(
            status(project, worktree_id, cx),
            SymbolIndexStatus::Ready { .. }
        )
    });
}

async fn definitions(
    project: &Entity<Project>,
    name: &str,
    near: Option<ProjectPath>,
    cx: &mut TestAppContext,
) -> Vec<IndexedDefinition> {
    let store = store(project, cx);
    let task = store.update(cx, |store, cx| store.definitions(name, near, cx));
    task.await.expect("definitions")
}

fn locations(definitions: &[IndexedDefinition]) -> Vec<(String, u32, bool)> {
    definitions
        .iter()
        .map(|definition| {
            (
                definition.path.path.as_unix_str().to_string(),
                definition.name_range.start.row,
                definition.is_declaration,
            )
        })
        .collect()
}

/// Asks for `name` until `accept` takes the answer, while the index follows
/// file system events on its own threads.
async fn wait_for_definitions(
    project: &Entity<Project>,
    name: &str,
    what: &str,
    cx: &mut TestAppContext,
    accept: impl Fn(&[IndexedDefinition]) -> bool,
) {
    for _ in 0..1000 {
        // File events wait out a debounce timer before the index reads them.
        cx.executor().advance_clock(Duration::from_millis(100));
        cx.run_until_parked();
        if accept(&definitions(project, name, None, cx).await) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting until {what}");
}

fn project_path(worktree_id: WorktreeId, path: &str) -> ProjectPath {
    ProjectPath {
        worktree_id,
        path: rel_path(path).into_arc(),
    }
}

fn update_symbol_index_settings(
    cx: &mut TestAppContext,
    update: impl FnOnce(&mut SymbolIndexSettingsContent),
) {
    cx.update(|cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |settings| {
                update(settings.project.symbol_index.get_or_insert_default())
            });
        });
    });
}

#[gpui::test]
async fn test_symbol_index_answers_after_the_first_build(cx: &mut TestAppContext) {
    init_test(cx);
    let (project, _fs, worktree_id) = new_project(project_files(), cx).await;
    let index_root = tempfile::tempdir().expect("a temporary directory");
    set_index_root(&project, index_root.path(), cx);
    wait_for_ready(&project, worktree_id, cx);

    let SymbolIndexStatus::Ready { stats } = status(&project, worktree_id, cx) else {
        panic!("the index should be ready");
    };
    assert_eq!(stats.files, 3, "the three C files, not the README");

    assert_eq!(
        locations(&definitions(&project, "foo", None, cx).await),
        vec![
            ("a.c".to_string(), 2, false),
            ("inc/x.h".to_string(), 0, true)
        ],
        "the definition comes before the declaration"
    );

    // Each file's `static` helper is the only candidate from that file.
    assert_eq!(
        locations(
            &definitions(
                &project,
                "helper",
                Some(project_path(worktree_id, "a.c")),
                cx
            )
            .await
        ),
        vec![("a.c".to_string(), 1, false)]
    );
    assert_eq!(
        locations(
            &definitions(
                &project,
                "helper",
                Some(project_path(worktree_id, "b.c")),
                cx
            )
            .await
        ),
        vec![("b.c".to_string(), 1, false)]
    );
    assert_eq!(
        definitions(&project, "helper", None, cx).await.len(),
        2,
        "without a starting file, every helper is a candidate"
    );
    // A file without its own `static` helper sees neither.
    assert!(
        definitions(
            &project,
            "helper",
            Some(project_path(worktree_id, "inc/x.h")),
            cx
        )
        .await
        .is_empty()
    );

    // A header's `static inline` function is every includer's.
    assert_eq!(
        locations(
            &definitions(
                &project,
                "shared_inline",
                Some(project_path(worktree_id, "a.c")),
                cx
            )
            .await
        ),
        vec![("inc/x.h".to_string(), 4, false)]
    );

    let store = store(&project, cx);
    let callers = store
        .update(cx, |store, cx| store.callers("foo", 100, cx))
        .await
        .expect("callers");
    assert!(!callers.truncated);
    let mut caller_names = callers
        .calls
        .iter()
        .map(|call| {
            (
                call.path.path.as_unix_str().to_string(),
                call.caller
                    .as_ref()
                    .map(|caller| caller.name.clone())
                    .unwrap_or_default(),
            )
        })
        .collect::<Vec<_>>();
    caller_names.sort();
    assert_eq!(
        caller_names,
        vec![
            ("a.c".to_string(), "bar".to_string()),
            ("b.c".to_string(), "main".to_string())
        ]
    );

    let truncated = store
        .update(cx, |store, cx| store.callers("foo", 1, cx))
        .await
        .expect("callers");
    assert_eq!(truncated.calls.len(), 1);
    assert!(truncated.truncated);

    let callees = store
        .update(cx, |store, cx| {
            store.callees(project_path(worktree_id, "a.c"), Point::new(3, 8), cx)
        })
        .await
        .expect("callees")
        .expect("a function contains the position");
    assert_eq!(callees.function.name, "foo");
    assert_eq!(
        callees
            .callees
            .iter()
            .map(|callee| (callee.callee.as_str(), callee.target))
            .collect::<Vec<_>>(),
        vec![("helper", CalleeTarget::Function)]
    );
    assert!(
        store
            .update(cx, |store, cx| {
                store.callees(project_path(worktree_id, "a.c"), Point::new(0, 0), cx)
            })
            .await
            .expect("callees")
            .is_none(),
        "no function contains the include line"
    );

    let found = store
        .update(cx, |store, cx| {
            store.search("fo", SearchMode::Prefix, 10, cx)
        })
        .await
        .expect("search");
    assert!(found.iter().any(|found| found.definition.name == "foo"));

    let assignments = store
        .update(cx, |store, cx| store.assignments("open", false, cx))
        .await
        .expect("assignments");
    assert_eq!(
        assignments
            .iter()
            .map(|assignment| assignment.function.as_str())
            .collect::<Vec<_>>(),
        vec!["real_open"]
    );
}

#[gpui::test]
async fn test_symbol_index_follows_file_changes(cx: &mut TestAppContext) {
    init_test(cx);
    let (project, fs, worktree_id) = new_project(project_files(), cx).await;
    let index_root = tempfile::tempdir().expect("a temporary directory");
    set_index_root(&project, index_root.path(), cx);
    wait_for_ready(&project, worktree_id, cx);
    assert!(definitions(&project, "added", None, cx).await.is_empty());

    fs.insert_file(
        path!("/root/new.c"),
        b"int added(void) { return 0; }\n".to_vec(),
    )
    .await;
    // Changes wait out the debounce on the (fake) clock before the update
    // thread is started, however long it is given in real time.
    for _ in 0..4 {
        cx.executor().advance_clock(Duration::from_millis(100));
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        definitions(&project, "added", None, cx).await.is_empty(),
        "read before the debounce ended"
    );
    wait_for_definitions(&project, "added", "the new file is indexed", cx, |found| {
        !found.is_empty()
    })
    .await;

    fs.remove_file(Path::new(path!("/root/b.c")), RemoveOptions::default())
        .await
        .expect("removing b.c");
    wait_for_definitions(
        &project,
        "main",
        "the removed file leaves the index",
        cx,
        |found| found.is_empty(),
    )
    .await;

    fs.rename(
        Path::new(path!("/root/a.c")),
        Path::new(path!("/root/moved.c")),
        RenameOptions::default(),
    )
    .await
    .expect("renaming a.c");
    wait_for_definitions(
        &project,
        "bar",
        "the renamed file is indexed under its new name",
        cx,
        |found| locations(found) == vec![("moved.c".to_string(), 5, false)],
    )
    .await;
}

#[gpui::test]
async fn test_symbol_index_leaves_out_excluded_private_and_ignored_files(cx: &mut TestAppContext) {
    init_test(cx);
    cx.update(|cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |settings| {
                settings.project.worktree.private_files =
                    Some(vec!["**/secret/**".to_string()].into());
                settings.project.worktree.file_scan_exclusions =
                    Some(vec!["**/vendor".to_string()].into());
            });
        });
    });
    let (project, fs, worktree_id) = new_project(
        json!({
            ".git": {},
            ".gitignore": "build/\nignored*.c\n",
            "main.c": "int visible(void) { return 0; }\n",
            "ignored.c": "int hidden_ignored_file(void) { return 4; }\n",
            "secret": { "key.c": "int hidden_private(void) { return 1; }\n" },
            "vendor": { "lib.c": "int hidden_excluded(void) { return 2; }\n" },
            "build": { "gen.c": "int hidden_ignored(void) { return 3; }\n" },
        }),
        cx,
    )
    .await;
    let index_root = tempfile::tempdir().expect("a temporary directory");
    set_index_root(&project, index_root.path(), cx);
    wait_for_ready(&project, worktree_id, cx);

    let SymbolIndexStatus::Ready { stats } = status(&project, worktree_id, cx) else {
        panic!("the index should be ready");
    };
    assert_eq!(stats.files, 1, "only main.c is indexed");
    assert_eq!(definitions(&project, "visible", None, cx).await.len(), 1);
    for name in [
        "hidden_private",
        "hidden_excluded",
        "hidden_ignored",
        "hidden_ignored_file",
    ] {
        assert!(
            definitions(&project, name, None, cx).await.is_empty(),
            "{name} must not be found"
        );
    }

    // Files that appear later are held to the same rules.
    fs.insert_file(
        path!("/root/ignored_later.c"),
        b"int hidden_ignored_later(void) { return 5; }\n".to_vec(),
    )
    .await;
    fs.insert_file(
        path!("/root/later.c"),
        b"int visible_later(void) { return 6; }\n".to_vec(),
    )
    .await;
    wait_for_definitions(
        &project,
        "visible_later",
        "the new file is indexed",
        cx,
        |found| !found.is_empty(),
    )
    .await;
    let SymbolIndexStatus::Ready { stats } = status(&project, worktree_id, cx) else {
        panic!("the index should be ready");
    };
    assert_eq!(stats.files, 2, "main.c and later.c, not the ignored file");
}

#[gpui::test]
async fn test_symbol_index_results_are_checked_against_current_settings(cx: &mut TestAppContext) {
    init_test(cx);
    let (project, _fs, worktree_id) = new_project(
        json!({
            "main.c": "int visible(void) { return 0; }\n",
            "secret": { "key.c": "int later_private(void) { return 1; }\n" },
        }),
        cx,
    )
    .await;
    // An index built before the file became private still holds it.
    let index_directory = tempfile::tempdir().expect("a temporary directory");
    let index = build_index_for_tests(
        index_directory.path(),
        &[
            ("main.c", "int visible(void) { return 0; }\n"),
            ("secret/key.c", "int later_private(void) { return 1; }\n"),
        ],
    )
    .expect("building the index");
    let store = store(&project, cx);
    store.update(cx, |store, cx| {
        store.insert_index_for_tests(worktree_id, index, cx)
    });
    assert_eq!(
        definitions(&project, "later_private", None, cx).await.len(),
        1
    );

    cx.update(|cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |settings| {
                settings.project.worktree.private_files =
                    Some(vec!["**/secret/**".to_string()].into());
            });
        });
    });
    cx.run_until_parked();
    assert!(
        definitions(&project, "later_private", None, cx)
            .await
            .is_empty(),
        "a file made private since it was indexed is left out"
    );
    let store = store.clone();
    let found = store
        .update(cx, |store, cx| {
            store.search("later", SearchMode::Prefix, 10, cx)
        })
        .await
        .expect("search");
    assert!(found.is_empty());
    assert_eq!(definitions(&project, "visible", None, cx).await.len(), 1);
}

fn indexed_files(
    project: &Entity<Project>,
    worktree_id: WorktreeId,
    cx: &mut TestAppContext,
) -> u64 {
    match status(project, worktree_id, cx) {
        SymbolIndexStatus::Ready { stats } => stats.files,
        _ => u64::MAX,
    }
}

/// The worktree marks its entries with the root's settings only, so a
/// folder's own `.zed/settings.json` and links inside the worktree are for
/// the index itself to check.
#[gpui::test]
async fn test_symbol_index_leaves_out_what_folder_settings_and_links_hide(cx: &mut TestAppContext) {
    init_test(cx);
    cx.update(|cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |settings| {
                settings.project.worktree.private_files =
                    Some(vec!["**/secret/**".to_string()].into());
            });
        });
    });
    let (project, fs, worktree_id) = new_project(
        json!({
            "main.c": "int visible(void) { return 0; }\n",
            "secret": { "key.c": "int hidden_private(void) { return 1; }\n" },
            "sub": {
                ".zed": { "settings.json": r#"{ "private_files": ["**/sub/**"] }"# },
                "nested.c": "int hidden_nested(void) { return 2; }\n",
            },
        }),
        cx,
    )
    .await;
    fs.insert_tree(
        path!("/outside"),
        json!({ "lib.c": "int hidden_external(void) { return 3; }\n" }),
    )
    .await;
    fs.insert_symlink(
        path!("/root/alias.c"),
        PathBuf::from(path!("/root/secret/key.c")),
    )
    .await;
    fs.insert_symlink(
        path!("/root/linked.c"),
        PathBuf::from(path!("/outside/lib.c")),
    )
    .await;
    cx.run_until_parked();
    let nested_entry_is_private = project.read_with(cx, |project, cx| {
        project
            .worktree_for_id(worktree_id, cx)
            .and_then(|worktree| {
                worktree
                    .read(cx)
                    .entry_for_path(rel_path("sub/nested.c"))
                    .map(|entry| entry.is_private)
            })
    });
    assert_eq!(
        nested_entry_is_private,
        Some(false),
        "the worktree does not mark what a folder's settings make private"
    );

    let index_root = tempfile::tempdir().expect("a temporary directory");
    set_index_root(&project, index_root.path(), cx);
    wait_until(cx, "only main.c is indexed", |cx| {
        indexed_files(&project, worktree_id, cx) == 1
    });
    for name in ["hidden_private", "hidden_nested", "hidden_external"] {
        assert!(
            definitions(&project, name, None, cx).await.is_empty(),
            "{name} must not be found"
        );
    }

    // Files that appear later, and so reach the index as single changes,
    // are held to the same rules.
    fs.insert_file(
        path!("/root/sub/later.c"),
        b"int hidden_later(void) { return 4; }\n".to_vec(),
    )
    .await;
    fs.insert_symlink(
        path!("/root/alias_later.c"),
        PathBuf::from(path!("/root/sub/nested.c")),
    )
    .await;
    fs.insert_file(
        path!("/root/later.c"),
        b"int visible_later(void) { return 5; }\n".to_vec(),
    )
    .await;
    wait_for_definitions(
        &project,
        "visible_later",
        "the new file is indexed",
        cx,
        |found| !found.is_empty(),
    )
    .await;
    assert_eq!(
        indexed_files(&project, worktree_id, cx),
        2,
        "main.c and later.c, not sub/later.c or the link to sub/nested.c"
    );
}

/// Each of the two checks made when a result is shown works on its own: here
/// the settings see a file as private while its worktree entry does not.
#[gpui::test]
async fn test_symbol_index_results_hidden_by_folder_settings_alone(cx: &mut TestAppContext) {
    init_test(cx);
    let (project, _fs, worktree_id) = new_project(
        json!({
            "main.c": "int visible(void) { return 0; }\n",
            "sub": {
                ".zed": { "settings.json": r#"{ "private_files": ["**/sub/**"] }"# },
                "nested.c": "int hidden_nested(void) { return 2; }\n",
            },
        }),
        cx,
    )
    .await;
    cx.run_until_parked();
    let nested_entry_is_private = project.read_with(cx, |project, cx| {
        project
            .worktree_for_id(worktree_id, cx)
            .and_then(|worktree| {
                worktree
                    .read(cx)
                    .entry_for_path(rel_path("sub/nested.c"))
                    .map(|entry| entry.is_private)
            })
    });
    assert_eq!(nested_entry_is_private, Some(false));

    // An index built without the folder's settings holds the file.
    let index_directory = tempfile::tempdir().expect("a temporary directory");
    let index = build_index_for_tests(
        index_directory.path(),
        &[
            ("main.c", "int visible(void) { return 0; }\n"),
            ("sub/nested.c", "int hidden_nested(void) { return 2; }\n"),
        ],
    )
    .expect("building the index");
    let store = store(&project, cx);
    store.update(cx, |store, cx| {
        store.insert_index_for_tests(worktree_id, index, cx)
    });

    assert!(
        definitions(&project, "hidden_nested", None, cx)
            .await
            .is_empty()
    );
    let found = store
        .update(cx, |store, cx| {
            store.search("hidden", SearchMode::Prefix, 10, cx)
        })
        .await
        .expect("search");
    assert!(found.is_empty(), "{found:?}");
    let callees = store
        .update(cx, |store, cx| {
            store.callees(
                project_path(worktree_id, "sub/nested.c"),
                Point::new(0, 20),
                cx,
            )
        })
        .await
        .expect("callees");
    assert!(callees.is_none());
    assert_eq!(definitions(&project, "visible", None, cx).await.len(), 1);
}

#[gpui::test]
async fn test_symbol_index_off_indexes_nothing(cx: &mut TestAppContext) {
    init_test(cx);
    update_symbol_index_settings(cx, |settings| {
        settings.enabled = Some(SymbolIndexEnabled::Off)
    });
    let (project, _fs, worktree_id) = new_project(project_files(), cx).await;
    let index_root = tempfile::tempdir().expect("a temporary directory");
    set_index_root(&project, index_root.path(), cx);
    for _ in 0..20 {
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        status(&project, worktree_id, cx),
        SymbolIndexStatus::Disabled
    );
    assert!(definitions(&project, "foo", None, cx).await.is_empty());
    assert_eq!(
        std::fs::read_dir(index_root.path())
            .expect("reading the index root")
            .count(),
        0,
        "no index directory is created"
    );

    update_symbol_index_settings(cx, |settings| {
        settings.enabled = Some(SymbolIndexEnabled::Auto)
    });
    wait_for_ready(&project, worktree_id, cx);
    assert_eq!(definitions(&project, "foo", None, cx).await.len(), 2);
}

#[gpui::test]
async fn test_symbol_index_auto_waits_for_c_files(cx: &mut TestAppContext) {
    init_test(cx);
    let (project, fs, worktree_id) = new_project(json!({ "notes.md": "no code" }), cx).await;
    let index_root = tempfile::tempdir().expect("a temporary directory");
    set_index_root(&project, index_root.path(), cx);
    wait_until(cx, "the index is judged unnecessary", |cx| {
        status(&project, worktree_id, cx) == SymbolIndexStatus::Disabled
    });
    assert_eq!(
        std::fs::read_dir(index_root.path())
            .expect("reading the index root")
            .count(),
        0
    );

    fs.insert_file(
        path!("/root/first.c"),
        b"int first(void) { return 0; }\n".to_vec(),
    )
    .await;
    wait_for_ready(&project, worktree_id, cx);
    assert_eq!(definitions(&project, "first", None, cx).await.len(), 1);
}

#[gpui::test]
async fn test_symbol_index_in_use_elsewhere_falls_back(cx: &mut TestAppContext) {
    init_test(cx);
    let (project, _fs, worktree_id) = new_project(project_files(), cx).await;
    let index_root = tempfile::tempdir().expect("a temporary directory");
    let worktree_root = PathBuf::from(path!("/root"));
    let directory = SymbolIndexStore::index_directory(index_root.path(), &worktree_root);
    let held = symbol_index::SymbolIndex::open(&directory, symbol_index::IndexConfig::default())
        .expect("another window opens the index first");

    set_index_root(&project, index_root.path(), cx);
    wait_until(cx, "the index is reported in use", |cx| {
        status(&project, worktree_id, cx)
            == SymbolIndexStatus::Unavailable {
                reason: SymbolIndexUnavailable::InUseByAnotherWindow,
            }
    });
    assert!(
        definitions(&project, "foo", None, cx).await.is_empty(),
        "queries answer with nothing rather than fail or wait"
    );

    drop(held);
    let store = store(&project, cx);
    store
        .update(cx, |store, cx| store.rebuild(cx))
        .await
        .expect("rebuild");
    wait_for_ready(&project, worktree_id, cx);
    assert_eq!(definitions(&project, "foo", None, cx).await.len(), 2);
}

#[gpui::test]
async fn test_symbol_index_is_released_when_the_worktree_closes(cx: &mut TestAppContext) {
    init_test(cx);
    let (project, _fs, worktree_id) = new_project(project_files(), cx).await;
    let index_root = tempfile::tempdir().expect("a temporary directory");
    set_index_root(&project, index_root.path(), cx);
    wait_for_ready(&project, worktree_id, cx);

    project.update(cx, |project, cx| project.remove_worktree(worktree_id, cx));
    let directory =
        SymbolIndexStore::index_directory(index_root.path(), &PathBuf::from(path!("/root")));
    wait_until(cx, "the index directory is free again", |cx| {
        cx.run_until_parked();
        symbol_index::SymbolIndex::open(&directory, symbol_index::IndexConfig::default()).is_ok()
    });
    assert_eq!(
        status(&project, worktree_id, cx),
        SymbolIndexStatus::Disabled,
        "a removed worktree has no index"
    );
}

#[gpui::test]
fn test_language_server_timeout_is_kept_within_bounds(cx: &mut TestAppContext) {
    init_test(cx);
    let timeout = |cx: &mut TestAppContext| {
        cx.update(|cx| {
            <project::project_settings::ProjectSettings as settings::Settings>::get_global(cx)
                .symbol_index
                .language_server_timeout
        })
    };
    assert_eq!(timeout(cx), Duration::from_millis(1500));
    for (written, used) in [
        (0, 100),
        (5, 100),
        (100, 100),
        (2500, 2500),
        (10_000, 10_000),
        (u64::MAX, 10_000),
    ] {
        update_symbol_index_settings(cx, |settings| {
            settings.language_server_timeout_ms = Some(written)
        });
        assert_eq!(timeout(cx), Duration::from_millis(used), "{written}");
    }
}

#[gpui::test]
async fn test_unused_indexes_are_removed_and_open_ones_kept(cx: &mut TestAppContext) {
    init_test(cx);
    let (project, _fs, worktree_id) = new_project(project_files(), cx).await;
    let index_root = tempfile::tempdir().expect("a temporary directory");
    let unused = index_root.path().join("0123456789abcdef0123456789abcdef");
    drop(
        symbol_index::SymbolIndex::open(&unused, symbol_index::IndexConfig::default())
            .expect("an index of a folder opened long ago"),
    );
    set_index_root(&project, index_root.path(), cx);
    wait_for_ready(&project, worktree_id, cx);
    let open_directory =
        SymbolIndexStore::index_directory(index_root.path(), &PathBuf::from(path!("/root")));
    assert!(open_directory.is_dir());

    let store = store(&project, cx);
    let later = std::time::SystemTime::now()
        + project::symbol_index_store::UNUSED_INDEX_AGE
        + Duration::from_secs(24 * 60 * 60);
    let outcomes = store
        .read_with(cx, |store, cx| store.remove_unused_indexes(later, cx))
        .await;
    let outcome = |outcomes: &[(PathBuf, symbol_index::StaleIndexOutcome)], path: &Path| {
        outcomes
            .iter()
            .find(|(directory, _)| directory == path)
            .map(|(_, outcome)| outcome.clone())
    };
    assert_eq!(
        outcome(&outcomes, &unused),
        Some(symbol_index::StaleIndexOutcome::Removed)
    );
    assert!(!unused.exists());
    assert_eq!(
        outcome(&outcomes, &open_directory),
        Some(symbol_index::StaleIndexOutcome::Kept)
    );
    assert_eq!(definitions(&project, "foo", None, cx).await.len(), 2);

    // Closed, the folder's index is unused too; opening the folder again builds it again.
    project.update(cx, |project, cx| project.remove_worktree(worktree_id, cx));
    wait_until(cx, "the index directory is free again", |cx| {
        cx.run_until_parked();
        symbol_index::SymbolIndex::open(&open_directory, symbol_index::IndexConfig::default())
            .is_ok()
    });
    let outcomes = store
        .read_with(cx, |store, cx| store.remove_unused_indexes(later, cx))
        .await;
    assert_eq!(
        outcome(&outcomes, &open_directory),
        Some(symbol_index::StaleIndexOutcome::Removed)
    );
    assert!(!open_directory.exists());

    let (worktree, _) = project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path!("/root"), true, cx)
        })
        .await
        .expect("opening the folder again");
    let worktree_id = worktree.read_with(cx, |worktree, _| worktree.id());
    wait_for_ready(&project, worktree_id, cx);
    assert!(open_directory.is_dir(), "the index is built again");
    assert_eq!(definitions(&project, "foo", None, cx).await.len(), 2);
}

/// The index of an open folder that has not been opened (yet, or at all) is still the folder's.
#[gpui::test]
async fn test_unused_index_removal_keeps_the_index_of_an_open_folder_not_opened(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    update_symbol_index_settings(cx, |settings| {
        settings.enabled = Some(SymbolIndexEnabled::Off)
    });
    let (project, _fs, _worktree_id) = new_project(project_files(), cx).await;
    let index_root = tempfile::tempdir().expect("a temporary directory");
    let folder_index =
        SymbolIndexStore::index_directory(index_root.path(), &PathBuf::from(path!("/root")));
    drop(
        symbol_index::SymbolIndex::open(&folder_index, symbol_index::IndexConfig::default())
            .expect("an index of the folder from earlier"),
    );
    let unused = index_root.path().join("0123456789abcdef0123456789abcdef");
    drop(
        symbol_index::SymbolIndex::open(&unused, symbol_index::IndexConfig::default())
            .expect("an index of another folder"),
    );
    set_index_root(&project, index_root.path(), cx);
    cx.run_until_parked();

    let later = std::time::SystemTime::now()
        + project::symbol_index_store::UNUSED_INDEX_AGE
        + Duration::from_secs(24 * 60 * 60);
    let store = store(&project, cx);
    let outcomes = store
        .read_with(cx, |store, cx| store.remove_unused_indexes(later, cx))
        .await;
    let outcome = |path: &Path| {
        outcomes
            .iter()
            .find(|(directory, _)| directory == path)
            .map(|(_, outcome)| outcome.clone())
    };
    assert_eq!(
        outcome(&folder_index),
        Some(symbol_index::StaleIndexOutcome::Kept)
    );
    assert!(folder_index.is_dir());
    assert_eq!(
        outcome(&unused),
        Some(symbol_index::StaleIndexOutcome::Removed)
    );
}
