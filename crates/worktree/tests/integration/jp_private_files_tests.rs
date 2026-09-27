use gpui::{Entity, TestAppContext};
use rpc::{AnyProtoClient, NoopProtoClient, proto};
use settings::SettingsStore;
use util::{paths::PathStyle, rel_path::rel_path};
use worktree::Worktree;

fn remote_worktree(cx: &mut TestAppContext) -> Entity<Worktree> {
    remote_worktree_named("project", cx)
}

fn remote_worktree_named(root_name: &str, cx: &mut TestAppContext) -> Entity<Worktree> {
    cx.update(|cx| {
        if !cx.has_global::<SettingsStore>() {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
        }
    });
    let client = AnyProtoClient::new(NoopProtoClient::new());
    cx.update(|cx| {
        Worktree::remote(
            1,
            clock::ReplicaId::new(1),
            proto::WorktreeMetadata {
                id: 1,
                root_name: root_name.to_string(),
                visible: true,
                abs_path: format!("/home/user/{root_name}"),
                root_repo_common_dir: None,
                root_repo_is_linked_worktree: false,
            },
            client,
            PathStyle::Unix,
            cx,
        )
    })
}

fn entry(id: u64, path: &str, is_dir: bool, is_private: bool) -> proto::Entry {
    proto::Entry {
        id,
        is_dir,
        path: path.to_string(),
        inode: id,
        mtime: Some(proto::Timestamp {
            seconds: 0,
            nanos: 0,
        }),
        is_private,
        ..Default::default()
    }
}

/// zed-jp-enhance: a host whose `is_private` does not arrive (another wire
/// revision, a collab server that drops it, settings that have not caught up)
/// must not open `.env` and keys to agents: this side's own `private_files`
/// marks them too, and the host's verdict still counts on its own.
#[gpui::test]
async fn remote_entries_are_private_by_this_sides_settings_too(cx: &mut TestAppContext) {
    let worktree = remote_worktree(cx);
    worktree.update(cx, |worktree, _| {
        worktree
            .as_remote()
            .expect("a remote worktree")
            .update_from_remote(proto::UpdateWorktree {
                project_id: 1,
                worktree_id: 1,
                abs_path: "/home/user/project".to_string(),
                root_name: "project".to_string(),
                updated_entries: vec![
                    entry(1, "", true, false),
                    // What a host that never sent the flag leaves: false.
                    entry(2, ".env", false, false),
                    entry(3, "certs", true, false),
                    entry(4, "certs/server.pem", false, false),
                    entry(5, "src", true, false),
                    entry(6, "src/main.rs", false, false),
                    // The host's own verdict, which this side's settings do
                    // not cover.
                    entry(7, "src/host_secret.txt", false, true),
                ],
                scan_id: 1,
                is_last_update: true,
                ..Default::default()
            });
    });
    cx.run_until_parked();

    worktree.read_with(cx, |worktree, _| {
        let is_private = |path: &str| {
            worktree
                .entry_for_path(rel_path(path))
                .unwrap_or_else(|| panic!("{path} should have arrived"))
                .is_private
        };
        assert!(
            is_private(".env"),
            "the default `private_files` covers .env"
        );
        assert!(is_private("certs/server.pem"));
        assert!(
            is_private("src/host_secret.txt"),
            "the host's verdict is kept"
        );
        assert!(!is_private("src/main.rs"));
        assert!(!is_private("src"));
    });
}

/// A worktree opened on a single file sends that file as its root, under the
/// empty path; the host judges it by its name, and so must this side.
#[gpui::test]
async fn a_single_file_worktree_is_private_by_its_name(cx: &mut TestAppContext) {
    for (root_name, expected) in [(".env", true), ("main.rs", false)] {
        let worktree = remote_worktree_named(root_name, cx);
        worktree.update(cx, |worktree, _| {
            worktree
                .as_remote()
                .expect("a remote worktree")
                .update_from_remote(proto::UpdateWorktree {
                    project_id: 1,
                    worktree_id: 1,
                    abs_path: format!("/home/user/{root_name}"),
                    root_name: root_name.to_string(),
                    updated_entries: vec![entry(1, "", false, false)],
                    scan_id: 1,
                    is_last_update: true,
                    ..Default::default()
                });
        });
        cx.run_until_parked();
        worktree.read_with(cx, |worktree, _| {
            let root = worktree
                .entry_for_path(rel_path(""))
                .expect("the file should have arrived");
            assert_eq!(root.is_private, expected, "{root_name}");
        });
    }
}

/// A pattern that names a directory covers everything under it, however the
/// entries of one update are ordered.
#[gpui::test]
async fn entries_under_a_private_directory_are_private(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let mut store = SettingsStore::test(cx);
        store.update_user_settings(cx, |settings| {
            settings.project.worktree.private_files = Some(vec!["**/vault".to_string()].into());
        });
        cx.set_global(store);
    });
    let worktree = remote_worktree(cx);
    worktree.update(cx, |worktree, _| {
        worktree
            .as_remote()
            .expect("a remote worktree")
            .update_from_remote(proto::UpdateWorktree {
                project_id: 1,
                worktree_id: 1,
                abs_path: "/home/user/project".to_string(),
                root_name: "project".to_string(),
                updated_entries: vec![
                    entry(1, "", true, false),
                    // Before its directories, which the verdicts are kept for.
                    entry(2, "a/vault/deep/notes.txt", false, false),
                    entry(3, "a", true, false),
                    entry(4, "a/vault", true, false),
                    entry(5, "a/vault/deep", true, false),
                    entry(6, "a/vault_notes.txt", false, false),
                    entry(7, "a/other/notes.txt", false, false),
                    entry(8, "a/other", true, false),
                ],
                scan_id: 1,
                is_last_update: true,
                ..Default::default()
            });
    });
    cx.run_until_parked();
    worktree.read_with(cx, |worktree, _| {
        let is_private = |path: &str| {
            worktree
                .entry_for_path(rel_path(path))
                .unwrap_or_else(|| panic!("{path} should have arrived"))
                .is_private
        };
        for private in ["a/vault", "a/vault/deep", "a/vault/deep/notes.txt"] {
            assert!(is_private(private), "{private}");
        }
        for public in ["a", "a/vault_notes.txt", "a/other", "a/other/notes.txt"] {
            assert!(!is_private(public), "{public}");
        }
    });
}
