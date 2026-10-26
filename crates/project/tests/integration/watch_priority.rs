use std::sync::Arc;

use gpui::{Entity, TestAppContext};
use project::{
    FakeFs, Project, ProjectPath,
    watch_priority::{MAX_WATCH_PRIORITY_PATHS, WATCH_PRIORITY_DEBOUNCE},
};
use serde_json::json;
use util::{
    path,
    rel_path::{RelPath, rel_path},
};
use worktree::WorktreeId;

use crate::init_test;

fn directories(paths: &[&str]) -> Vec<Arc<RelPath>> {
    paths.iter().map(|path| rel_path(path).into_arc()).collect()
}

fn watch_priority(
    project: &Entity<Project>,
    worktree_id: WorktreeId,
    cx: &mut TestAppContext,
) -> Vec<String> {
    project.read_with(cx, |project, cx| {
        project
            .worktree_for_id(worktree_id, cx)
            .expect("the worktree")
            .read(cx)
            .as_local()
            .expect("a local worktree")
            .watch_priority_paths()
            .iter()
            .map(|path| path.as_unix_str().to_owned())
            .collect()
    })
}

fn set_panel(
    project: &Entity<Project>,
    worktree_id: WorktreeId,
    paths: &[&str],
    cx: &mut TestAppContext,
) {
    project.update(cx, |project, cx| {
        project.set_panel_watch_priority(worktree_id, directories(paths), cx)
    });
}

fn settle(cx: &mut TestAppContext) {
    cx.executor().advance_clock(WATCH_PRIORITY_DEBOUNCE);
    cx.run_until_parked();
}

#[gpui::test]
async fn test_watch_priority_combines_panel_and_open_file_directories(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/root"),
        json!({
            "a": { "one.txt": "" },
            "b": { "c": { "two.txt": "" } },
            "d": {},
            "e": {},
        }),
    )
    .await;
    let project = Project::test(fs, [path!("/root").as_ref()], cx).await;
    let worktree_id = project.read_with(cx, |project, cx| {
        project
            .worktrees(cx)
            .next()
            .expect("a worktree")
            .read(cx)
            .id()
    });
    cx.run_until_parked();

    set_panel(&project, worktree_id, &["d", "", "a"], cx);
    let two = project
        .update(cx, |project, cx| {
            project.open_local_buffer(path!("/root/b/c/two.txt"), cx)
        })
        .await
        .expect("opened");
    cx.run_until_parked();
    assert!(
        watch_priority(&project, worktree_id, cx).is_empty(),
        "nothing is sent before the debounce"
    );
    settle(cx);
    assert_eq!(
        watch_priority(&project, worktree_id, cx),
        ["b/c", "", "a", "d"],
        "open-file directories first, then the panel's, each sorted"
    );

    // Each change restarts the wait.
    cx.executor().advance_clock(WATCH_PRIORITY_DEBOUNCE / 2);
    set_panel(&project, worktree_id, &["e"], cx);
    cx.executor().advance_clock(WATCH_PRIORITY_DEBOUNCE / 2);
    set_panel(&project, worktree_id, &["e", "a"], cx);
    cx.executor().advance_clock(WATCH_PRIORITY_DEBOUNCE * 3 / 4);
    cx.run_until_parked();
    assert_eq!(
        watch_priority(&project, worktree_id, cx),
        ["b/c", "", "a", "d"]
    );
    settle(cx);
    assert_eq!(watch_priority(&project, worktree_id, cx), ["b/c", "a", "e"]);

    // An unchanged set is not sent again: what the worktree holds stays as it
    // was set behind the tracker's back.
    let worktree = project.read_with(cx, |project, cx| {
        project
            .worktree_for_id(worktree_id, cx)
            .expect("the worktree")
    });
    worktree.update(cx, |worktree, cx| {
        worktree.set_watch_priority(directories(&["sentinel"]), cx)
    });
    set_panel(&project, worktree_id, &["a", "e"], cx);
    settle(cx);
    assert_eq!(watch_priority(&project, worktree_id, cx), ["sentinel"]);

    // A directory both open and in the panel is listed once.
    let one = project
        .update(cx, |project, cx| {
            project.open_local_buffer(path!("/root/a/one.txt"), cx)
        })
        .await
        .expect("opened");
    settle(cx);
    assert_eq!(watch_priority(&project, worktree_id, cx), ["a", "b/c", "e"]);

    // Saving under another path moves the directory along.
    project
        .update(cx, |project, cx| {
            project.save_buffer_as(
                two.clone(),
                ProjectPath {
                    worktree_id,
                    path: rel_path("d/two.txt").into_arc(),
                },
                cx,
            )
        })
        .await
        .expect("saved");
    settle(cx);
    assert_eq!(watch_priority(&project, worktree_id, cx), ["a", "d", "e"]);

    // Closing a buffer drops its directory.
    set_panel(&project, worktree_id, &["e"], cx);
    cx.update(|_| drop(one));
    cx.run_until_parked();
    settle(cx);
    assert_eq!(watch_priority(&project, worktree_id, cx), ["d", "e"]);
    cx.update(|_| drop(two));
    cx.run_until_parked();
    settle(cx);
    assert_eq!(watch_priority(&project, worktree_id, cx), ["e"]);
}

#[gpui::test]
async fn test_watch_priority_is_capped_with_open_files_first(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/root"), json!({ "zzz": { "open.txt": "" } }))
        .await;
    let project = Project::test(fs, [path!("/root").as_ref()], cx).await;
    let worktree_id = project.read_with(cx, |project, cx| {
        project
            .worktrees(cx)
            .next()
            .expect("a worktree")
            .read(cx)
            .id()
    });
    let panel = (0..MAX_WATCH_PRIORITY_PATHS + 100)
        .map(|index| format!("dir{index:05}"))
        .collect::<Vec<_>>();
    let panel = panel.iter().map(String::as_str).collect::<Vec<_>>();
    set_panel(&project, worktree_id, &panel, cx);
    let _open = project
        .update(cx, |project, cx| {
            project.open_local_buffer(path!("/root/zzz/open.txt"), cx)
        })
        .await
        .expect("opened");
    settle(cx);

    let sent = watch_priority(&project, worktree_id, cx);
    assert_eq!(sent.len(), MAX_WATCH_PRIORITY_PATHS);
    assert_eq!(sent[0], "zzz");
    assert_eq!(sent[1], "dir00000");
    assert_eq!(
        sent[MAX_WATCH_PRIORITY_PATHS - 1],
        format!("dir{:05}", MAX_WATCH_PRIORITY_PATHS - 2)
    );
}
