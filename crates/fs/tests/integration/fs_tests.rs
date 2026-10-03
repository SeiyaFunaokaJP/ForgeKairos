mod fake_git_repo_tests;

use std::{
    collections::BTreeSet,
    io::Write,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    time::Duration,
};

use futures::{FutureExt, StreamExt};

use fs::*;
use gpui::{BackgroundExecutor, TestAppContext};
use serde_json::json;
use tempfile::TempDir;
use util::path;

#[gpui::test]
async fn test_watcher_diagnostics_do_not_change_event_delivery(executor: BackgroundExecutor) {
    let fs = FakeFs::new(executor);
    let root = Path::new(path!("/root"));
    let file = root.join("file");
    fs.create_dir(root).await.unwrap();
    let (mut events, watcher) = fs.watch(root, Duration::ZERO).await;
    let recording = fs.record_watcher_diagnostics().unwrap();
    assert_eq!(
        recording.snapshot().watchers[0].roots[0].path,
        root.to_string_lossy()
    );
    assert!(recording.snapshot().events.is_empty());

    fs.write(&file, b"first").await.unwrap();
    let batch = events.next().await.unwrap();
    assert!(batch.iter().any(|event| event.path == file));
    assert!(recording.snapshot().events.iter().any(|event| {
        event.operation == "event" && event.paths.contains(&file.to_string_lossy().into_owned())
    }));

    fs.simulate_watcher_overflow(root);
    let batch = events.next().await.unwrap();
    assert!(
        batch
            .iter()
            .any(|event| event.kind == Some(PathEventKind::Rescan))
    );
    assert!(recording.snapshot().events.iter().any(|event| event.rescan));

    drop(recording);
    fs.write(&file, b"second").await.unwrap();
    let batch = events.next().await.unwrap();
    assert!(batch.iter().any(|event| event.path == file));
    assert!(
        fs.record_watcher_diagnostics()
            .unwrap()
            .snapshot()
            .events
            .is_empty()
    );
    drop(events);
    drop(watcher);
    assert!(
        fs.record_watcher_diagnostics().unwrap().snapshot().watchers[0]
            .roots
            .is_empty()
    );
}

#[gpui::test]
async fn test_fake_fs(executor: BackgroundExecutor) {
    let fs = FakeFs::new(executor.clone());
    fs.insert_tree(
        path!("/root"),
        json!({
            "dir1": {
                "a": "A",
                "b": "B"
            },
            "dir2": {
                "c": "C",
                "dir3": {
                    "d": "D"
                }
            }
        }),
    )
    .await;

    assert_eq!(
        fs.files(),
        vec![
            PathBuf::from(path!("/root/dir1/a")),
            PathBuf::from(path!("/root/dir1/b")),
            PathBuf::from(path!("/root/dir2/c")),
            PathBuf::from(path!("/root/dir2/dir3/d")),
        ]
    );

    fs.create_symlink(path!("/root/dir2/link-to-dir3").as_ref(), "./dir3".into())
        .await
        .unwrap();

    assert_eq!(
        fs.canonicalize(path!("/root/dir2/link-to-dir3").as_ref())
            .await
            .unwrap(),
        PathBuf::from(path!("/root/dir2/dir3")),
    );
    assert_eq!(
        fs.canonicalize(path!("/root/dir2/link-to-dir3/d").as_ref())
            .await
            .unwrap(),
        PathBuf::from(path!("/root/dir2/dir3/d")),
    );
    assert_eq!(
        fs.load(path!("/root/dir2/link-to-dir3/d").as_ref())
            .await
            .unwrap(),
        "D",
    );
}

#[gpui::test]
async fn test_copy_recursive_with_single_file(executor: BackgroundExecutor) {
    let fs = FakeFs::new(executor.clone());
    fs.insert_tree(
        path!("/outer"),
        json!({
            "a": "A",
            "b": "B",
            "inner": {}
        }),
    )
    .await;

    assert_eq!(
        fs.files(),
        vec![
            PathBuf::from(path!("/outer/a")),
            PathBuf::from(path!("/outer/b")),
        ]
    );

    let source = Path::new(path!("/outer/a"));
    let target = Path::new(path!("/outer/a copy"));
    copy_recursive(fs.as_ref(), source, target, Default::default())
        .await
        .unwrap();

    assert_eq!(
        fs.files(),
        vec![
            PathBuf::from(path!("/outer/a")),
            PathBuf::from(path!("/outer/a copy")),
            PathBuf::from(path!("/outer/b")),
        ]
    );

    let source = Path::new(path!("/outer/a"));
    let target = Path::new(path!("/outer/inner/a copy"));
    copy_recursive(fs.as_ref(), source, target, Default::default())
        .await
        .unwrap();

    assert_eq!(
        fs.files(),
        vec![
            PathBuf::from(path!("/outer/a")),
            PathBuf::from(path!("/outer/a copy")),
            PathBuf::from(path!("/outer/b")),
            PathBuf::from(path!("/outer/inner/a copy")),
        ]
    );
}

#[gpui::test]
async fn test_copy_recursive_with_single_dir(executor: BackgroundExecutor) {
    let fs = FakeFs::new(executor.clone());
    fs.insert_tree(
        path!("/outer"),
        json!({
            "a": "A",
            "empty": {},
            "non-empty": {
                "b": "B",
            }
        }),
    )
    .await;

    assert_eq!(
        fs.files(),
        vec![
            PathBuf::from(path!("/outer/a")),
            PathBuf::from(path!("/outer/non-empty/b")),
        ]
    );
    assert_eq!(
        fs.directories(false),
        vec![
            PathBuf::from(path!("/")),
            PathBuf::from(path!("/outer")),
            PathBuf::from(path!("/outer/empty")),
            PathBuf::from(path!("/outer/non-empty")),
        ]
    );

    let source = Path::new(path!("/outer/empty"));
    let target = Path::new(path!("/outer/empty copy"));
    copy_recursive(fs.as_ref(), source, target, Default::default())
        .await
        .unwrap();

    assert_eq!(
        fs.files(),
        vec![
            PathBuf::from(path!("/outer/a")),
            PathBuf::from(path!("/outer/non-empty/b")),
        ]
    );
    assert_eq!(
        fs.directories(false),
        vec![
            PathBuf::from(path!("/")),
            PathBuf::from(path!("/outer")),
            PathBuf::from(path!("/outer/empty")),
            PathBuf::from(path!("/outer/empty copy")),
            PathBuf::from(path!("/outer/non-empty")),
        ]
    );

    let source = Path::new(path!("/outer/non-empty"));
    let target = Path::new(path!("/outer/non-empty copy"));
    copy_recursive(fs.as_ref(), source, target, Default::default())
        .await
        .unwrap();

    assert_eq!(
        fs.files(),
        vec![
            PathBuf::from(path!("/outer/a")),
            PathBuf::from(path!("/outer/non-empty/b")),
            PathBuf::from(path!("/outer/non-empty copy/b")),
        ]
    );
    assert_eq!(
        fs.directories(false),
        vec![
            PathBuf::from(path!("/")),
            PathBuf::from(path!("/outer")),
            PathBuf::from(path!("/outer/empty")),
            PathBuf::from(path!("/outer/empty copy")),
            PathBuf::from(path!("/outer/non-empty")),
            PathBuf::from(path!("/outer/non-empty copy")),
        ]
    );
}

#[gpui::test]
async fn test_copy_recursive(executor: BackgroundExecutor) {
    let fs = FakeFs::new(executor.clone());
    fs.insert_tree(
        path!("/outer"),
        json!({
            "inner1": {
                "a": "A",
                "b": "B",
                "inner3": {
                    "d": "D",
                },
                "inner4": {}
            },
            "inner2": {
                "c": "C",
            }
        }),
    )
    .await;

    assert_eq!(
        fs.files(),
        vec![
            PathBuf::from(path!("/outer/inner1/a")),
            PathBuf::from(path!("/outer/inner1/b")),
            PathBuf::from(path!("/outer/inner2/c")),
            PathBuf::from(path!("/outer/inner1/inner3/d")),
        ]
    );
    assert_eq!(
        fs.directories(false),
        vec![
            PathBuf::from(path!("/")),
            PathBuf::from(path!("/outer")),
            PathBuf::from(path!("/outer/inner1")),
            PathBuf::from(path!("/outer/inner2")),
            PathBuf::from(path!("/outer/inner1/inner3")),
            PathBuf::from(path!("/outer/inner1/inner4")),
        ]
    );

    let source = Path::new(path!("/outer"));
    let target = Path::new(path!("/outer/inner1/outer"));
    copy_recursive(fs.as_ref(), source, target, Default::default())
        .await
        .unwrap();

    assert_eq!(
        fs.files(),
        vec![
            PathBuf::from(path!("/outer/inner1/a")),
            PathBuf::from(path!("/outer/inner1/b")),
            PathBuf::from(path!("/outer/inner2/c")),
            PathBuf::from(path!("/outer/inner1/inner3/d")),
            PathBuf::from(path!("/outer/inner1/outer/inner1/a")),
            PathBuf::from(path!("/outer/inner1/outer/inner1/b")),
            PathBuf::from(path!("/outer/inner1/outer/inner2/c")),
            PathBuf::from(path!("/outer/inner1/outer/inner1/inner3/d")),
        ]
    );
    assert_eq!(
        fs.directories(false),
        vec![
            PathBuf::from(path!("/")),
            PathBuf::from(path!("/outer")),
            PathBuf::from(path!("/outer/inner1")),
            PathBuf::from(path!("/outer/inner2")),
            PathBuf::from(path!("/outer/inner1/inner3")),
            PathBuf::from(path!("/outer/inner1/inner4")),
            PathBuf::from(path!("/outer/inner1/outer")),
            PathBuf::from(path!("/outer/inner1/outer/inner1")),
            PathBuf::from(path!("/outer/inner1/outer/inner2")),
            PathBuf::from(path!("/outer/inner1/outer/inner1/inner3")),
            PathBuf::from(path!("/outer/inner1/outer/inner1/inner4")),
        ]
    );
}

#[gpui::test]
async fn test_copy_recursive_with_overwriting(executor: BackgroundExecutor) {
    let fs = FakeFs::new(executor.clone());
    fs.insert_tree(
        path!("/outer"),
        json!({
            "inner1": {
                "a": "A",
                "b": "B",
                "outer": {
                    "inner1": {
                        "a": "B"
                    }
                }
            },
            "inner2": {
                "c": "C",
            }
        }),
    )
    .await;

    assert_eq!(
        fs.files(),
        vec![
            PathBuf::from(path!("/outer/inner1/a")),
            PathBuf::from(path!("/outer/inner1/b")),
            PathBuf::from(path!("/outer/inner2/c")),
            PathBuf::from(path!("/outer/inner1/outer/inner1/a")),
        ]
    );
    assert_eq!(
        fs.load(path!("/outer/inner1/outer/inner1/a").as_ref())
            .await
            .unwrap(),
        "B",
    );

    let source = Path::new(path!("/outer"));
    let target = Path::new(path!("/outer/inner1/outer"));
    copy_recursive(
        fs.as_ref(),
        source,
        target,
        CopyOptions {
            overwrite: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    assert_eq!(
        fs.files(),
        vec![
            PathBuf::from(path!("/outer/inner1/a")),
            PathBuf::from(path!("/outer/inner1/b")),
            PathBuf::from(path!("/outer/inner2/c")),
            PathBuf::from(path!("/outer/inner1/outer/inner1/a")),
            PathBuf::from(path!("/outer/inner1/outer/inner1/b")),
            PathBuf::from(path!("/outer/inner1/outer/inner2/c")),
            PathBuf::from(path!("/outer/inner1/outer/inner1/outer/inner1/a")),
        ]
    );
    assert_eq!(
        fs.load(path!("/outer/inner1/outer/inner1/a").as_ref())
            .await
            .unwrap(),
        "A"
    );
}

#[gpui::test]
async fn test_copy_recursive_with_ignoring(executor: BackgroundExecutor) {
    let fs = FakeFs::new(executor.clone());
    fs.insert_tree(
        path!("/outer"),
        json!({
            "inner1": {
                "a": "A",
                "b": "B",
                "outer": {
                    "inner1": {
                        "a": "B"
                    }
                }
            },
            "inner2": {
                "c": "C",
            }
        }),
    )
    .await;

    assert_eq!(
        fs.files(),
        vec![
            PathBuf::from(path!("/outer/inner1/a")),
            PathBuf::from(path!("/outer/inner1/b")),
            PathBuf::from(path!("/outer/inner2/c")),
            PathBuf::from(path!("/outer/inner1/outer/inner1/a")),
        ]
    );
    assert_eq!(
        fs.load(path!("/outer/inner1/outer/inner1/a").as_ref())
            .await
            .unwrap(),
        "B",
    );

    let source = Path::new(path!("/outer"));
    let target = Path::new(path!("/outer/inner1/outer"));
    copy_recursive(
        fs.as_ref(),
        source,
        target,
        CopyOptions {
            ignore_if_exists: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    assert_eq!(
        fs.files(),
        vec![
            PathBuf::from(path!("/outer/inner1/a")),
            PathBuf::from(path!("/outer/inner1/b")),
            PathBuf::from(path!("/outer/inner2/c")),
            PathBuf::from(path!("/outer/inner1/outer/inner1/a")),
            PathBuf::from(path!("/outer/inner1/outer/inner1/b")),
            PathBuf::from(path!("/outer/inner1/outer/inner2/c")),
            PathBuf::from(path!("/outer/inner1/outer/inner1/outer/inner1/a")),
        ]
    );
    assert_eq!(
        fs.load(path!("/outer/inner1/outer/inner1/a").as_ref())
            .await
            .unwrap(),
        "B"
    );
}

#[gpui::test]
async fn test_realfs_atomic_write(executor: BackgroundExecutor) {
    // With the file handle still open, the file should be replaced
    // https://github.com/zed-industries/zed/issues/30054
    let fs = RealFs::new(None, executor);
    let temp_dir = TempDir::new().unwrap();
    let file_to_be_replaced = temp_dir.path().join("file.txt");
    let mut file = std::fs::File::create_new(&file_to_be_replaced).unwrap();
    file.write_all(b"Hello").unwrap();
    // drop(file);  // We still hold the file handle here
    let content = std::fs::read_to_string(&file_to_be_replaced).unwrap();
    assert_eq!(content, "Hello");
    gpui::block_on(fs.atomic_write(file_to_be_replaced.clone(), "World".into())).unwrap();
    let content = std::fs::read_to_string(&file_to_be_replaced).unwrap();
    assert_eq!(content, "World");
}

#[gpui::test]
async fn test_realfs_atomic_write_non_existing_file(executor: BackgroundExecutor) {
    let fs = RealFs::new(None, executor);
    let temp_dir = TempDir::new().unwrap();
    let file_to_be_replaced = temp_dir.path().join("file.txt");
    gpui::block_on(fs.atomic_write(file_to_be_replaced.clone(), "Hello".into())).unwrap();
    let content = std::fs::read_to_string(&file_to_be_replaced).unwrap();
    assert_eq!(content, "Hello");
}

#[gpui::test]
#[allow(clippy::disallowed_methods)]
async fn test_realfs_git_init_sets_initial_branch(executor: BackgroundExecutor) {
    executor.allow_parking();
    let fs = RealFs::new(None, executor);
    let temp_dir = TempDir::new().unwrap();
    let repository_path = temp_dir.path().join("repository");
    std::fs::create_dir(&repository_path).unwrap();

    // `git_init` prefers the user's `init.defaultBranch`, so expect whatever this machine has.
    let configured_default_branch = std::process::Command::new("git")
        .args(["config", "--global", "--get", "init.defaultBranch"])
        .output()
        .ok()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|branch| !branch.is_empty());
    let expected_head = format!(
        "ref: refs/heads/{}",
        configured_default_branch.unwrap_or_else(|| "zed-fallback".to_string())
    );
    let read_head = || {
        std::fs::read_to_string(repository_path.join(".git").join("HEAD"))
            .unwrap()
            .trim()
            .to_string()
    };

    fs.git_init(&repository_path, "zed-fallback".to_string())
        .await
        .unwrap();
    assert_eq!(read_head(), expected_head);

    // Like `git init -b`, re-initializing must not switch an existing repository's branch.
    fs.git_init(&repository_path, "another-fallback".to_string())
        .await
        .unwrap();
    assert_eq!(read_head(), expected_head);
}

#[gpui::test]
#[cfg(target_os = "windows")]
async fn test_realfs_canonicalize(executor: BackgroundExecutor) {
    use util::paths::SanitizedPath;

    let fs = RealFs::new(None, executor);
    let temp_dir = TempDir::new().unwrap();
    let file = temp_dir.path().join("test (1).txt");
    let file = SanitizedPath::new(&file);
    std::fs::write(&file, "test").unwrap();

    let canonicalized = fs.canonicalize(file.as_path()).await;
    assert!(canonicalized.is_ok());
}

#[gpui::test]
async fn test_rename(executor: BackgroundExecutor) {
    let fs = FakeFs::new(executor.clone());
    fs.insert_tree(
        path!("/root"),
        json!({
            "src": {
                "file_a.txt": "content a",
                "file_b.txt": "content b"
            }
        }),
    )
    .await;

    fs.rename(
        Path::new(path!("/root/src/file_a.txt")),
        Path::new(path!("/root/src/new/renamed_a.txt")),
        RenameOptions {
            create_parents: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    // Assert that the `file_a.txt` file was being renamed and moved to a
    // different directory that did not exist before.
    assert_eq!(
        fs.files(),
        vec![
            PathBuf::from(path!("/root/src/file_b.txt")),
            PathBuf::from(path!("/root/src/new/renamed_a.txt")),
        ]
    );

    let result = fs
        .rename(
            Path::new(path!("/root/src/file_b.txt")),
            Path::new(path!("/root/src/old/renamed_b.txt")),
            RenameOptions {
                create_parents: false,
                ..Default::default()
            },
        )
        .await;

    // Assert that the `file_b.txt` file was not renamed nor moved, as
    // `create_parents` was set to `false`.
    // different directory that did not exist before.
    assert!(result.is_err());
    assert_eq!(
        fs.files(),
        vec![
            PathBuf::from(path!("/root/src/file_b.txt")),
            PathBuf::from(path!("/root/src/new/renamed_a.txt")),
        ]
    );
}

#[gpui::test]
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
async fn test_realfs_parallel_rename_without_overwrite_preserves_losing_source(
    executor: BackgroundExecutor,
) {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path();
    let source_a = root.join("dir_a/shared.txt");
    let source_b = root.join("dir_b/shared.txt");
    let target = root.join("shared.txt");

    std::fs::create_dir_all(source_a.parent().unwrap()).unwrap();
    std::fs::create_dir_all(source_b.parent().unwrap()).unwrap();
    std::fs::write(&source_a, "from a").unwrap();
    std::fs::write(&source_b, "from b").unwrap();

    let fs = RealFs::new(None, executor);
    let (first_result, second_result) = futures::future::join(
        fs.rename(&source_a, &target, RenameOptions::default()),
        fs.rename(&source_b, &target, RenameOptions::default()),
    )
    .await;

    assert_ne!(first_result.is_ok(), second_result.is_ok());
    assert!(target.exists());
    assert_eq!(source_a.exists() as u8 + source_b.exists() as u8, 1);
}

#[gpui::test]
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
async fn test_realfs_rename_ignore_if_exists_leaves_source_and_target_unchanged(
    executor: BackgroundExecutor,
) {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path();
    let source = root.join("source.txt");
    let target = root.join("target.txt");

    std::fs::write(&source, "from source").unwrap();
    std::fs::write(&target, "from target").unwrap();

    let fs = RealFs::new(None, executor);
    let result = fs
        .rename(
            &source,
            &target,
            RenameOptions {
                ignore_if_exists: true,
                ..Default::default()
            },
        )
        .await;

    assert!(result.is_ok());

    assert_eq!(std::fs::read_to_string(&source).unwrap(), "from source");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "from target");
}

#[gpui::test]
async fn test_fake_fs_rename_ignore_if_exists_leaves_source_and_target_unchanged(
    executor: BackgroundExecutor,
) {
    let fs = FakeFs::new(executor);
    fs.insert_tree(
        path!("/root"),
        json!({
            "source.txt": "from source",
            "target.txt": "from target",
        }),
    )
    .await;

    let handle = fs
        .open_handle(Path::new(path!("/root/source.txt")))
        .await
        .unwrap();

    let result = fs
        .rename(
            Path::new(path!("/root/source.txt")),
            Path::new(path!("/root/target.txt")),
            RenameOptions {
                ignore_if_exists: true,
                ..Default::default()
            },
        )
        .await;

    assert!(result.is_ok());

    assert_eq!(
        fs.load(Path::new(path!("/root/source.txt"))).await.unwrap(),
        "from source"
    );
    assert_eq!(
        fs.load(Path::new(path!("/root/target.txt"))).await.unwrap(),
        "from target"
    );

    // A handle held across an ignored rename must keep reporting the path the
    // file is actually at, not the one it never went to.
    assert_eq!(
        handle.current_path(&(fs.clone() as Arc<dyn Fs>)).unwrap(),
        PathBuf::from(path!("/root/source.txt"))
    );
}

async fn assert_copy_and_remove_semantics(root: &Path, fs: &dyn Fs) {
    let source = root.join("source.txt");
    let target = root.join("target.txt");
    let overwrite = CopyOptions {
        overwrite: true,
        ignore_if_exists: false,
    };

    let target_inode_before = fs.metadata(&target).await.unwrap().unwrap().inode;
    fs.copy_file(&source, &target, overwrite).await.unwrap();
    assert_eq!(fs.load(&target).await.unwrap(), "from source");

    cfg_select! {
        unix => assert_eq!(
            fs.metadata(&target).await.unwrap().unwrap().inode,
            target_inode_before
        ),
        _ => { let _ = target_inode_before; }
    }

    fs.copy_file(&source, &target, CopyOptions::default())
        .await
        .unwrap_err();
    fs.copy_file(
        &source,
        &target,
        CopyOptions {
            overwrite: false,
            ignore_if_exists: true,
        },
    )
    .await
    .unwrap();

    fs.copy_file(&source, &root.join("dir"), overwrite)
        .await
        .unwrap_err();
    assert!(fs.is_dir(&root.join("dir")).await);
}

/// Removing a symlink to a directory removes the link, not the directory.
async fn assert_remove_file_unlinks_symlink(root: &Path, fs: &dyn Fs) {
    let link = root.join("link");
    fs.remove_file(&link, RemoveOptions::default())
        .await
        .unwrap();
    assert!(fs.metadata(&link).await.unwrap().is_none());
    assert!(fs.is_dir(&root.join("dir")).await);
    assert_eq!(
        fs.load(&root.join("dir").join("inner.txt")).await.unwrap(),
        "inner"
    );
}

#[gpui::test]
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
async fn test_realfs_copy_and_remove_semantics(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let executor = cx.executor();
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path();
    std::fs::write(root.join("source.txt"), "from source").unwrap();
    std::fs::write(root.join("target.txt"), "from target").unwrap();
    std::fs::create_dir(root.join("dir")).unwrap();
    std::fs::write(root.join("dir").join("inner.txt"), "inner").unwrap();

    let fs = RealFs::new(None, executor);
    assert_copy_and_remove_semantics(root, fs.as_ref()).await;

    // Creating symlinks requires elevated privileges on Windows, so like the
    // watcher tests above, skip that part when it is not possible.
    match make_dir_symlink(&root.join("dir"), &root.join("link")) {
        Ok(()) => assert_remove_file_unlinks_symlink(root, fs.as_ref()).await,
        Err(error) => eprintln!("skipping symlink removal check (cannot symlink: {error})"),
    }
}

#[gpui::test]
async fn test_fake_fs_copy_and_remove_semantics(executor: BackgroundExecutor) {
    let fs = FakeFs::new(executor);
    fs.insert_tree(
        path!("/root"),
        json!({
            "source.txt": "from source",
            "target.txt": "from target",
            "dir": { "inner.txt": "inner" },
        }),
    )
    .await;
    fs.insert_symlink(path!("/root/link"), PathBuf::from(path!("/root/dir")))
        .await;

    let root = Path::new(path!("/root"));
    assert_copy_and_remove_semantics(root, fs.as_ref()).await;
    assert_remove_file_unlinks_symlink(root, fs.as_ref()).await;
}

#[gpui::test]
async fn test_fake_fs_rename_onto_itself_keeps_the_file(executor: BackgroundExecutor) {
    let fs = FakeFs::new(executor);
    fs.insert_tree(
        path!("/root"),
        json!({
            "a.txt": "content",
        }),
    )
    .await;

    let path = Path::new(path!("/root/a.txt"));
    let result = fs
        .rename(
            path,
            path,
            RenameOptions {
                overwrite: true,
                ..Default::default()
            },
        )
        .await;

    assert!(result.is_ok());
    assert_eq!(fs.load(path).await.unwrap(), "content");
}

#[gpui::test]
#[cfg(unix)]
async fn test_realfs_executable_metadata(executor: BackgroundExecutor) {
    use std::os::unix::fs::PermissionsExt as _;

    let tempdir = TempDir::new().unwrap();
    let path = tempdir.path();
    let non_executable_path = path.join("non-executable.sh");
    let executable_path = path.join("executable.sh");
    let symlink_path = path.join("executable-symlink.sh");

    std::fs::write(&non_executable_path, "#!/bin/sh\n").unwrap();
    std::fs::write(&executable_path, "#!/bin/sh\n").unwrap();
    let mut permissions = std::fs::metadata(&executable_path).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&executable_path, permissions).unwrap();

    let fs = RealFs::new(None, executor);
    gpui::block_on(fs.create_symlink(&symlink_path, PathBuf::from("executable.sh"))).unwrap();

    let non_executable_metadata = fs
        .metadata(&non_executable_path)
        .await
        .expect("metadata call succeeds")
        .expect("metadata returned");
    assert!(!non_executable_metadata.is_executable);

    let executable_metadata = fs
        .metadata(&executable_path)
        .await
        .expect("metadata call succeeds")
        .expect("metadata returned");
    assert!(executable_metadata.is_executable);

    let symlink_metadata = fs
        .metadata(&symlink_path)
        .await
        .expect("metadata call succeeds")
        .expect("metadata returned");
    assert!(symlink_metadata.is_symlink);
    assert!(symlink_metadata.is_executable);
}

#[gpui::test]
#[cfg(unix)]
async fn test_realfs_broken_symlink_metadata(executor: BackgroundExecutor) {
    let tempdir = TempDir::new().unwrap();
    let path = tempdir.path();
    let fs = RealFs::new(None, executor);
    let symlink_path = path.join("symlink");
    gpui::block_on(fs.create_symlink(&symlink_path, PathBuf::from("file_a.txt"))).unwrap();
    let metadata = fs
        .metadata(&symlink_path)
        .await
        .expect("metadata call succeeds")
        .expect("metadata returned");
    assert!(metadata.is_symlink);
    assert!(!metadata.is_dir);
    assert!(!metadata.is_fifo);
    assert!(!metadata.is_executable);
    // don't care about len or mtime on symlinks?
}

#[gpui::test]
#[cfg(unix)]
async fn test_realfs_symlink_loop_metadata(executor: BackgroundExecutor) {
    let tempdir = TempDir::new().unwrap();
    let path = tempdir.path();
    let fs = RealFs::new(None, executor);
    let symlink_path = path.join("symlink");
    gpui::block_on(fs.create_symlink(&symlink_path, PathBuf::from("symlink"))).unwrap();
    let metadata = fs
        .metadata(&symlink_path)
        .await
        .expect("metadata call succeeds")
        .expect("metadata returned");
    assert!(metadata.is_symlink);
    assert!(!metadata.is_dir);
    assert!(!metadata.is_fifo);
    assert!(!metadata.is_executable);
    // don't care about len or mtime on symlinks?
}

/// `metadata` opens a file only in ways that a writer sharing it the way git for Windows appends
/// to `.git/logs/HEAD` (`FILE_SHARE_READ | FILE_SHARE_WRITE`) can coexist with. Sharing works both
/// ways: an extensionless program that `metadata` could not open next to such a writer would read
/// as not executable, and the same open made first would have kept git from appending.
#[gpui::test]
#[cfg(windows)]
async fn test_realfs_metadata_shares_extensionless_files_with_writers(
    executor: BackgroundExecutor,
) {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_SHARE_READ: u32 = 0x1;
    const FILE_SHARE_WRITE: u32 = 0x2;

    executor.allow_parking();
    let tempdir = TempDir::new().unwrap();
    let fs = RealFs::new(None, executor);
    let is_executable = async |path: &Path| fs.metadata(path).await.unwrap().unwrap().is_executable;

    let program = tempdir.path().join("tool");
    let system_root = std::env::var_os("SystemRoot").expect("SystemRoot is set");
    std::fs::copy(
        Path::new(&system_root).join("System32").join("cmd.exe"),
        &program,
    )
    .unwrap();
    let reflog = tempdir.path().join("HEAD");
    std::fs::write(
        &reflog,
        "0 1 A <a@example.com> 1700000000 +0900\tcommit: x\n",
    )
    .unwrap();
    let script = tempdir.path().join("run.CMD");
    std::fs::write(&script, "@echo off\n").unwrap();
    let text = tempdir.path().join("notes.txt");
    std::fs::write(&text, "MZ\n").unwrap();

    assert!(is_executable(&program).await);
    assert!(is_executable(&script).await);
    assert!(!is_executable(&reflog).await);
    assert!(!is_executable(&text).await);
    assert!(!is_executable(tempdir.path()).await);

    let append_like_git = |path: &Path| {
        std::fs::OpenOptions::new()
            .append(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .open(path)
            .unwrap()
    };
    let _program_writer = append_like_git(&program);
    let _reflog_writer = append_like_git(&reflog);
    assert!(is_executable(&program).await);
    assert!(!is_executable(&reflog).await);
}

#[gpui::test]
async fn test_fake_fs_trash(executor: BackgroundExecutor) {
    let fs = FakeFs::new(executor.clone());
    fs.insert_tree(
        path!("/root"),
        json!({
            "src": {
                "file_c.txt": "File C",
                "file_d.txt": "File D"
            },
            "file_a.txt": "File A",
            "file_b.txt": "File B",
        }),
    )
    .await;

    // Trashing a file.
    let path = path!("/root/file_a.txt").as_ref();
    fs.trash(path, Default::default())
        .await
        .expect("should be able to trash {path:?}");

    assert_eq!(
        fs.files(),
        vec![
            PathBuf::from(path!("/root/file_b.txt")),
            PathBuf::from(path!("/root/src/file_c.txt")),
            PathBuf::from(path!("/root/src/file_d.txt"))
        ]
    );

    // Trashing a directory.
    let path = path!("/root/src").as_ref();
    fs.trash(
        path,
        RemoveOptions {
            recursive: true,
            ..Default::default()
        },
    )
    .await
    .expect("should be able to trash {path:?}");

    assert_eq!(fs.files(), vec![PathBuf::from(path!("/root/file_b.txt"))]);
}

#[gpui::test]
async fn test_fake_fs_restore(executor: BackgroundExecutor) {
    let fs = FakeFs::new(executor.clone());
    fs.insert_tree(
        path!("/root"),
        json!({
            "src": {
                "file_a.txt": "File A",
                "file_b.txt": "File B",
            },
            "file_c.txt": "File C",
        }),
    )
    .await;

    // Attempt deleting a file, asserting that the filesystem no longer reports
    // it as part of its list of files, restore it and verify that the list of
    // files and trash has been updated accordingly.
    let path = path!("/root/src/file_a.txt").as_ref();
    let trashed_entry = fs.trash(path, Default::default()).await.unwrap();

    assert_eq!(
        fs.original_path_for_trash_id(trashed_entry),
        Some(path.to_path_buf())
    );

    fs.restore(trashed_entry).await.unwrap();
    assert_eq!(fs.original_path_for_trash_id(trashed_entry), None);

    assert_eq!(
        fs.files(),
        vec![
            PathBuf::from(path!("/root/file_c.txt")),
            PathBuf::from(path!("/root/src/file_a.txt")),
            PathBuf::from(path!("/root/src/file_b.txt"))
        ]
    );

    // Deleting and restoring a directory should also remove all of its files
    // but create a single trashed entry, which should be removed after
    // restoration.
    let options = RemoveOptions {
        recursive: true,
        ..Default::default()
    };
    let path = path!("/root/src/").as_ref();
    let trashed_entry = fs.trash(path, options).await.unwrap();

    assert_eq!(fs.files(), vec![PathBuf::from(path!("/root/file_c.txt"))]);

    fs.restore(trashed_entry).await.unwrap();

    assert_eq!(
        fs.files(),
        vec![
            PathBuf::from(path!("/root/file_c.txt")),
            PathBuf::from(path!("/root/src/file_a.txt")),
            PathBuf::from(path!("/root/src/file_b.txt"))
        ]
    );

    // A collision error should be returned in case a file is being restored to
    // a path where a file already exists.
    let path = path!("/root/src/file_a.txt").as_ref();
    let trashed_entry = fs.trash(path, Default::default()).await.unwrap();

    assert_eq!(
        fs.files(),
        vec![
            PathBuf::from(path!("/root/file_c.txt")),
            PathBuf::from(path!("/root/src/file_b.txt"))
        ]
    );

    fs.write(path, "New File A".as_bytes()).await.unwrap();

    assert_eq!(
        fs.files(),
        vec![
            PathBuf::from(path!("/root/file_c.txt")),
            PathBuf::from(path!("/root/src/file_a.txt")),
            PathBuf::from(path!("/root/src/file_b.txt"))
        ]
    );

    let file_contents = fs.files_with_contents(path);
    assert!(fs.restore(trashed_entry).await.is_err());
    assert_eq!(
        file_contents,
        vec![(PathBuf::from(path), b"New File A".to_vec())]
    );

    // A collision error should be returned in case a directory is being
    // restored to a path where a directory already exists.
    let options = RemoveOptions {
        recursive: true,
        ..Default::default()
    };
    let path = path!("/root/src/").as_ref();
    let trashed_entry = fs.trash(path, options).await.unwrap();

    assert_eq!(fs.files(), vec![PathBuf::from(path!("/root/file_c.txt"))]);

    fs.create_dir(path).await.unwrap();

    assert_eq!(fs.files(), vec![PathBuf::from(path!("/root/file_c.txt"))]);

    let result = fs.restore(trashed_entry).await;
    assert!(result.is_err());

    assert_eq!(fs.files(), vec![PathBuf::from(path!("/root/file_c.txt"))]);
}

/// Create a directory symlink (`link` -> `target`) in a cross-platform way.
///
/// Returns `Err` when the platform cannot create symlinks (e.g. Windows without
/// the create-symlink privilege), so callers can skip a scenario gracefully
/// rather than failing the whole test.
fn make_dir_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::symlink_dir(target, link)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (target, link);
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "symlinks are not supported on this platform",
        ))
    }
}

/// Waits up to `timeout` for `events` to deliver something that covers the
/// written file: either a path event whose path satisfies `path_matches`, or a
/// `Rescan` (which tells the consumer to re-scan this watcher's whole tree, so
/// it would discover the file anyway). Returns `false` if nothing relevant
/// arrives before the timeout.
async fn watcher_delivered_event(
    events: &mut (impl futures::Stream<Item = Vec<PathEvent>> + Unpin),
    executor: &BackgroundExecutor,
    timeout: Duration,
    path_matches: &(dyn Fn(&Path) -> bool + Send + Sync),
) -> bool {
    let timeout = executor.timer(timeout).fuse();
    futures::pin_mut!(timeout);
    loop {
        futures::select_biased! {
            batch = events.next().fuse() => {
                let Some(batch) = batch else { return false };
                let covered = batch.iter().any(|event| {
                    path_matches(&event.path) || event.kind == Some(PathEventKind::Rescan)
                });
                if covered {
                    return true;
                }
            }
            _ = timeout => return false,
        }
    }
}

#[gpui::test]
async fn test_realfs_watcher_diagnostics(executor: BackgroundExecutor, cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let fs = RealFs::new(None, executor.clone());
    let directory = TempDir::new().unwrap();
    let root = std::fs::canonicalize(directory.path()).unwrap();
    // Windows canonicalization adds a verbatim prefix that watcher paths omit.
    let root = util::paths::SanitizedPath::new(&root)
        .as_path()
        .to_path_buf();
    let recording = fs.record_watcher_diagnostics().unwrap();
    let (mut events, watcher) = fs.watch(&root, Duration::from_millis(10)).await;
    let file = root.join("watcher-diagnostics.txt");
    fs.write(&file, b"first").await.unwrap();
    assert!(
        watcher_delivered_event(&mut events, &executor, Duration::from_secs(5), &|path| {
            path == file
        })
        .await,
        "no watcher event matched {file:?}: {:#?}",
        recording.snapshot()
    );
    let snapshot = recording.snapshot();
    assert!(
        snapshot
            .events
            .iter()
            .any(|event| event.operation == "watch")
    );
    assert!(snapshot.events.iter().any(|event| {
        event.operation == "event"
            && (event.rescan || event.paths.contains(&file.to_string_lossy().into_owned()))
    }));
    assert!(snapshot.watchers.iter().any(|watcher| {
        watcher
            .roots
            .iter()
            .any(|entry| entry.path == root.to_string_lossy())
    }));
    serde_json::to_string_pretty(&snapshot).unwrap();

    drop(recording);
    fs.write(&file, b"second").await.unwrap();
    assert!(
        watcher_delivered_event(&mut events, &executor, Duration::from_secs(5), &|path| {
            path == file
        })
        .await
    );
    drop(events);
    drop(watcher);
}

/// Exercises a spread of real watchers whose registered watch path is spelled
/// differently from the path the OS reports events under. Each scenario watches
/// some directory and then mutates the on-disk file; a correct watcher must
/// deliver an event (or a rescan) for every scenario.
///
/// This asserts the residual path-aliasing bugs that real-casing the watch root
/// at add-time does NOT fix. The headline failure is `symlink_ancestor`:
/// watching a path that traverses a symlinked ancestor. On macOS FSEvents
/// reports events under the resolved real path, which no longer has the
/// symlinked prefix the watch root was registered with, so the events are
/// filtered out and never delivered.
///
/// Platform notes (the test runs everywhere but scenarios self-skip when they
/// cannot apply):
/// - Case scenarios require a case-insensitive filesystem (macOS/Windows
///   default; case-sensitive Linux/APFS skip them).
/// - Symlink scenarios require symlink creation (skipped on Windows without the
///   privilege).
/// - `symlink_ancestor` fails on macOS (FSEvents canonicalizes) but is expected
///   to pass on Linux (notify reconstructs paths from the watch path you pass),
///   which is itself a useful demonstration that this is an FSEvents-specific
///   bug.
#[gpui::test]
async fn test_realfs_watch_aliased_watch_paths_deliver_events(
    executor: BackgroundExecutor,
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();

    let fs = RealFs::new(None, executor.clone());
    let temp_dir = TempDir::new().expect("create temp dir");
    let root = temp_dir.path().to_path_buf();
    let latency = Duration::from_millis(10);

    // Probe the real filesystem for case sensitivity rather than guessing from
    // the platform.
    std::fs::create_dir_all(root.join("CaseProbe")).expect("create case probe dir");
    let case_insensitive = root.join("caseprobe").exists();
    eprintln!("filesystem is case-insensitive: {case_insensitive}");

    struct Scenario {
        name: &'static str,
        events: Pin<Box<dyn Send + futures::Stream<Item = Vec<PathEvent>>>>,
        _watcher: Arc<dyn Watcher>,
        path_matches: Box<dyn Fn(&Path) -> bool + Send + Sync>,
        action: Option<Box<dyn FnOnce() + Send>>,
    }

    let mut scenarios: Vec<Scenario> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();

    // --- Headline residual bug: watch path traverses a symlinked ancestor. ---
    {
        let real = root.join("ancestor_real");
        let inner = real.join("inner");
        std::fs::create_dir_all(&inner).expect("create symlinked-ancestor target");
        let link = root.join("ancestor_link");
        match make_dir_symlink(&real, &link) {
            Ok(()) => {
                let (events, watcher) = fs.watch(&link.join("inner"), latency).await;
                let file = inner.join("symlink_ancestor.txt");
                scenarios.push(Scenario {
                    name: "symlink_ancestor",
                    events,
                    _watcher: watcher,
                    path_matches: Box::new(|path| {
                        path.ends_with(Path::new("inner/symlink_ancestor.txt"))
                    }),
                    action: Some(Box::new(move || {
                        std::fs::write(&file, b"x").expect("write symlink-ancestor file");
                    })),
                });
            }
            Err(error) => skipped.push(format!("symlink_ancestor (cannot symlink: {error})")),
        }
    }

    // --- Control: watching a symlinked root IS handled (RealFs::watch follows
    //     the root symlink and also watches the target). ---
    {
        let real = root.join("root_real");
        std::fs::create_dir_all(&real).expect("create symlinked-root target");
        let link = root.join("root_link");
        match make_dir_symlink(&real, &link) {
            Ok(()) => {
                let (events, watcher) = fs.watch(&link, latency).await;
                let file = real.join("symlink_root.txt");
                scenarios.push(Scenario {
                    name: "symlink_root",
                    events,
                    _watcher: watcher,
                    path_matches: Box::new(|path| path.ends_with(Path::new("symlink_root.txt"))),
                    action: Some(Box::new(move || {
                        std::fs::write(&file, b"x").expect("write symlink-root file");
                    })),
                });
            }
            Err(error) => skipped.push(format!("symlink_root (cannot symlink: {error})")),
        }
    }

    // --- Control: wrong-case watch root (the originally-reported bug, which the
    //     real-casing fix already addresses). ---
    if case_insensitive {
        let real = root.join("CaseAlpha");
        std::fs::create_dir_all(&real).expect("create wrong-case root");
        let lower = PathBuf::from(real.to_string_lossy().to_lowercase());
        let (events, watcher) = fs.watch(&lower, latency).await;
        let file = real.join("alpha.txt");
        scenarios.push(Scenario {
            name: "wrong_case_root",
            events,
            _watcher: watcher,
            path_matches: Box::new(|path| path.ends_with(Path::new("alpha.txt"))),
            action: Some(Box::new(move || {
                std::fs::write(&file, b"x").expect("write wrong-case-root file");
            })),
        });
    } else {
        skipped.push("wrong_case_root (case-sensitive fs)".to_owned());
    }

    // --- Control: wrong-case nested watch path. ---
    if case_insensitive {
        let real = root.join("CaseBravo").join("Inner");
        std::fs::create_dir_all(&real).expect("create wrong-case nested dir");
        let lower = PathBuf::from(real.to_string_lossy().to_lowercase());
        let (events, watcher) = fs.watch(&lower, latency).await;
        let file = real.join("bravo.txt");
        scenarios.push(Scenario {
            name: "nested_wrong_case",
            events,
            _watcher: watcher,
            path_matches: Box::new(|path| path.ends_with(Path::new("bravo.txt"))),
            action: Some(Box::new(move || {
                std::fs::write(&file, b"x").expect("write nested-wrong-case file");
            })),
        });
    } else {
        skipped.push("nested_wrong_case (case-sensitive fs)".to_owned());
    }

    // --- Residual bug: the watched root is renamed to a different casing after
    //     the watch is established, so later events arrive under a spelling the
    //     registered (old-case) root no longer matches. ---
    if case_insensitive {
        let real = root.join("CaseEcho");
        std::fs::create_dir_all(&real).expect("create case-rename dir");
        let (events, watcher) = fs.watch(&real, latency).await;
        let renamed = root.join("CASEECHO");
        let file = renamed.join("echo.txt");
        scenarios.push(Scenario {
            name: "case_rename_root",
            events,
            _watcher: watcher,
            path_matches: Box::new(|path| path.ends_with(Path::new("echo.txt"))),
            action: Some(Box::new(move || {
                std::fs::rename(&real, &renamed).expect("case-only rename of watched root");
                std::fs::write(&file, b"x").expect("write case-rename file");
            })),
        });
    } else {
        skipped.push("case_rename_root (case-sensitive fs)".to_owned());
    }

    // Let every watch settle before mutating, then perform all mutations.
    executor.timer(Duration::from_millis(250)).await;
    for scenario in &mut scenarios {
        if let Some(action) = scenario.action.take() {
            action();
        }
    }

    let mut failures = Vec::new();
    for scenario in &mut scenarios {
        let delivered = watcher_delivered_event(
            &mut scenario.events,
            &executor,
            Duration::from_secs(3),
            scenario.path_matches.as_ref(),
        )
        .await;
        eprintln!("scenario {}: delivered={delivered}", scenario.name);
        if !delivered {
            failures.push(scenario.name);
        }
    }

    for name in &skipped {
        eprintln!("scenario skipped: {name}");
    }

    assert!(
        failures.is_empty(),
        "watchers failed to deliver events for {failures:?} (skipped: {skipped:?})"
    );
}

#[gpui::test]
#[ignore = "stress test; run explicitly when needed"]
async fn test_realfs_watch_stress_reports_missed_paths(
    executor: BackgroundExecutor,
    cx: &mut TestAppContext,
) {
    const FILE_COUNT: usize = 32000;
    cx.executor().allow_parking();

    let fs = RealFs::new(None, executor.clone());
    let temp_dir = TempDir::new().expect("create temp dir");
    let root = temp_dir.path();

    let mut file_paths = Vec::with_capacity(FILE_COUNT);
    let mut expected_paths = BTreeSet::new();

    for index in 0..FILE_COUNT {
        let dir_path = root.join(format!("dir-{index:04}"));
        let file_path = dir_path.join("file.txt");
        fs.create_dir(&dir_path).await.expect("create watched dir");
        fs.write(&file_path, b"before")
            .await
            .expect("create initial file");
        expected_paths.insert(file_path.clone());
        file_paths.push(file_path);
    }

    let (mut events, watcher) = fs.watch(root, Duration::from_millis(10)).await;
    let _watcher = watcher;

    for file_path in &expected_paths {
        _watcher
            .add(file_path.parent().expect("file has parent"))
            .expect("add explicit directory watch");
    }

    for (index, file_path) in file_paths.iter().enumerate() {
        let content = format!("after-{index}");
        fs.write(file_path, content.as_bytes())
            .await
            .expect("modify watched file");
    }

    let mut changed_paths = BTreeSet::new();
    let mut rescan_count: u32 = 0;
    let timeout = executor.timer(Duration::from_secs(10)).fuse();

    futures::pin_mut!(timeout);

    let mut ticks = 0;
    while ticks < 1000 {
        if let Some(batch) = events.next().fuse().now_or_never().flatten() {
            for event in batch {
                if event.kind == Some(PathEventKind::Rescan) {
                    rescan_count += 1;
                }
                if expected_paths.contains(&event.path) {
                    changed_paths.insert(event.path);
                }
            }
            if changed_paths.len() == expected_paths.len() {
                break;
            }
            ticks = 0;
        } else {
            ticks += 1;
            executor.timer(Duration::from_millis(10)).await;
        }
    }

    let missed_paths: BTreeSet<_> = expected_paths.difference(&changed_paths).cloned().collect();

    eprintln!(
        "realfs watch stress: expected={}, observed={}, missed={}, rescan={}",
        expected_paths.len(),
        changed_paths.len(),
        missed_paths.len(),
        rescan_count
    );

    assert!(
        missed_paths.is_empty() || rescan_count > 0,
        "missed {} paths without rescan being reported",
        missed_paths.len()
    );
}

#[gpui::test]
async fn restore_can_be_retried_after_collision(cx: &mut TestAppContext) {
    let fs = FakeFs::new(cx.background_executor.clone());
    let path = path!("/root/a.txt");
    let remove_options = RemoveOptions::default();
    fs.insert_tree(path!("/root"), json!({ "a.txt": "original"}))
        .await;

    // We'll first trash the `a.txt` file so we can hold onto its `TrashId`,
    // allowing us to later attempt restoring it again, ensuring that it didn't
    // get removed from the trash state, even if restoring failed.
    let trash_id = fs.trash(path.as_ref(), remove_options).await.unwrap();

    fs.insert_file(path, "conflicting".into()).await;
    let err = fs.restore(trash_id).await.unwrap_err();
    assert!(matches!(err, TrashRestoreError::Collision { .. }));

    fs.remove_file(path.as_ref(), remove_options).await.unwrap();
    let restored_path = fs.restore(trash_id).await.unwrap();
    assert_eq!(fs.load(restored_path.as_path()).await.unwrap(), "original");

    assert!(matches!(
        fs.restore(trash_id).await.unwrap_err(),
        TrashRestoreError::AlreadyRestored
    ));
}

fn verified_write_error(error: &anyhow::Error) -> Option<&VerifiedWriteError> {
    error.downcast_ref::<VerifiedWriteError>()
}

/// ADR-F49: a verified write reaches a new file in new folders and an existing
/// file beneath the root.
#[gpui::test]
async fn test_fake_fs_write_verified_writes_beneath_the_root(executor: BackgroundExecutor) {
    let fs = FakeFs::new(executor);
    fs.insert_tree(path!("/root"), json!({ "existing.txt": "old" }))
        .await;
    let root = Path::new(path!("/root"));

    fs.write_verified(root, Path::new("existing.txt"), b"new".to_vec())
        .await
        .unwrap();
    fs.write_verified(root, Path::new("a/b/new.txt"), b"created".to_vec())
        .await
        .unwrap();

    assert_eq!(
        fs.load(path!("/root/existing.txt").as_ref()).await.unwrap(),
        "new"
    );
    assert_eq!(
        fs.load(path!("/root/a/b/new.txt").as_ref()).await.unwrap(),
        "created"
    );
}

/// ADR-F49: a link anywhere on the way — a folder, the file itself — or a way
/// out of the root is refused, and nothing is written where it leads.
#[gpui::test]
async fn test_fake_fs_write_verified_refuses_links(executor: BackgroundExecutor) {
    let fs = FakeFs::new(executor);
    fs.insert_tree(path!("/root"), json!({ ".git": { "config": "[core]\n" } }))
        .await;
    fs.insert_symlink(
        path!("/root/folder_link"),
        PathBuf::from(path!("/root/.git")),
    )
    .await;
    fs.insert_symlink(
        path!("/root/file_link"),
        PathBuf::from(path!("/root/.git/config")),
    )
    .await;
    let root = Path::new(path!("/root"));

    for relative in [
        "folder_link/config",
        "folder_link/hooks/pre-commit",
        "file_link",
    ] {
        let error = fs
            .write_verified(root, Path::new(relative), b"pwned".to_vec())
            .await
            .expect_err(relative);
        assert!(
            matches!(
                verified_write_error(&error),
                Some(VerifiedWriteError::Redirected(_))
            ),
            "{relative}: {error:#}"
        );
    }
    let error = fs
        .write_verified(root, Path::new("../root/.git/config"), b"pwned".to_vec())
        .await
        .expect_err("a way out of the root");
    assert!(
        matches!(
            verified_write_error(&error),
            Some(VerifiedWriteError::NotBeneath(_))
        ),
        "{error:#}"
    );

    assert_eq!(
        fs.load(path!("/root/.git/config").as_ref()).await.unwrap(),
        "[core]\n"
    );
    assert!(!fs.is_dir(path!("/root/.git/hooks").as_ref()).await);
}

/// A folder link that needs no privilege: a junction on Windows.
#[cfg(windows)]
// One short `mklink` may block this test helper; an async process would have
// the deterministic test executor wait on the outside world.
#[allow(clippy::disallowed_methods)]
fn link_folder(target: &Path, link: &Path) {
    let status = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .status()
        .unwrap();
    assert!(status.success(), "mklink /J failed");
}

#[cfg(unix)]
fn link_folder(target: &Path, link: &Path) {
    std::os::unix::fs::symlink(target, link).unwrap();
}

/// ADR-F49: on the real filesystem, a verified write reaches ordinary files,
/// and refuses a hard link, a folder link on the way (without creating folders
/// through it) and a link as the file itself.
#[gpui::test]
async fn test_real_fs_write_verified(executor: BackgroundExecutor) {
    let fs = RealFs::new(None, executor);
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().join("project");
    let git_directory = root.join(".git");
    std::fs::create_dir_all(&git_directory).unwrap();
    std::fs::write(git_directory.join("config"), "[core]\n").unwrap();
    std::fs::write(root.join("existing.txt"), "old").unwrap();

    fs.write_verified(&root, Path::new("existing.txt"), b"new".to_vec())
        .await
        .unwrap();
    fs.write_verified(&root, Path::new("a/b/new.txt"), b"created".to_vec())
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(root.join("existing.txt")).unwrap(),
        "new"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("a").join("b").join("new.txt")).unwrap(),
        "created"
    );

    std::fs::hard_link(git_directory.join("config"), root.join("hard_link")).unwrap();
    let error = fs
        .write_verified(&root, Path::new("hard_link"), b"pwned".to_vec())
        .await
        .unwrap_err();
    assert!(
        matches!(
            verified_write_error(&error),
            Some(VerifiedWriteError::HardLinked(_))
        ),
        "{error:#}"
    );

    link_folder(&git_directory, &root.join("folder_link"));
    for relative in ["folder_link/config", "folder_link/hooks/pre-commit"] {
        let error = fs
            .write_verified(&root, Path::new(relative), b"pwned".to_vec())
            .await
            .unwrap_err();
        assert!(
            matches!(
                verified_write_error(&error),
                Some(VerifiedWriteError::Redirected(_))
            ),
            "{relative}: {error:#}"
        );
    }
    assert!(!git_directory.join("hooks").exists());

    // The link as the file itself: a directory junction on Windows (opening it
    // for writing fails), a symlink elsewhere.
    #[cfg(windows)]
    link_folder(&git_directory, &root.join("file_link"));
    #[cfg(unix)]
    std::os::unix::fs::symlink(git_directory.join("config"), root.join("file_link")).unwrap();
    let error = fs
        .write_verified(&root, Path::new("file_link"), b"pwned".to_vec())
        .await
        .unwrap_err();
    assert!(verified_write_error(&error).is_some(), "{error:#}");

    assert_eq!(
        std::fs::read_to_string(git_directory.join("config")).unwrap(),
        "[core]\n"
    );
}

/// ADR-F49 decision 11: a verified read reaches the files the folder holds
/// itself, and no link — a folder on the way, the file itself, or one that
/// stays inside the root, since a sandboxed command can point it elsewhere the
/// moment after.
#[gpui::test]
async fn test_fake_fs_verify_read_follows_no_link(executor: BackgroundExecutor) {
    let fs = FakeFs::new(executor);
    fs.insert_tree(
        path!("/root"),
        json!({
            "src": { "main.rs": "fn main() {}\n" },
            ".git": { "config": "[core]\n" },
        }),
    )
    .await;
    fs.insert_symlink(
        path!("/root/key"),
        PathBuf::from(path!("/home/user/.ssh/id_rsa")),
    )
    .await;
    fs.insert_symlink(
        path!("/root/folder_link"),
        PathBuf::from(path!("/root/src")),
    )
    .await;
    fs.insert_symlink(
        path!("/root/inside"),
        PathBuf::from(path!("/root/src/main.rs")),
    )
    .await;
    let root = Path::new(path!("/root"));

    fs.verify_read(root, Path::new("src/main.rs"))
        .await
        .unwrap();
    // Git's own files are read inside the sandbox as well, so a verified read
    // does not refuse what a verified write does.
    fs.verify_read(root, Path::new(".git/config"))
        .await
        .unwrap();

    for relative in ["key", "inside", "folder_link/main.rs"] {
        let error = fs
            .verify_read(root, Path::new(relative))
            .await
            .expect_err(relative);
        assert!(
            matches!(
                verified_write_error(&error),
                Some(VerifiedWriteError::Redirected(_))
            ),
            "{relative}: {error:#}"
        );
    }

    let error = fs
        .verify_read(root, Path::new("../root/src/main.rs"))
        .await
        .expect_err("a way out of the root");
    assert!(
        matches!(
            verified_write_error(&error),
            Some(VerifiedWriteError::NotBeneath(_))
        ),
        "{error:#}"
    );
    let error = fs
        .verify_read(root, Path::new("src"))
        .await
        .expect_err("a folder is not a file to read");
    assert!(
        matches!(
            verified_write_error(&error),
            Some(VerifiedWriteError::NotAFile(_))
        ),
        "{error:#}"
    );
    fs.verify_read(root, Path::new("src/missing.rs"))
        .await
        .expect_err("a file that is not there");
}

/// ADR-F49 decision 11: the same on the real filesystem, where a hard link is
/// a second name for a file the sandboxed agent can open itself, so a read of
/// one is not refused.
#[gpui::test]
async fn test_real_fs_verify_read(executor: BackgroundExecutor) {
    let fs = RealFs::new(None, executor);
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().join("project");
    let outside = temp_dir.path().join("outside");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(root.join("src").join("main.rs"), "fn main() {}\n").unwrap();
    std::fs::write(outside.join("id_rsa"), "-----BEGIN\n").unwrap();

    fs.verify_read(&root, Path::new("src/main.rs"))
        .await
        .unwrap();

    std::fs::hard_link(root.join("src").join("main.rs"), root.join("hard_link")).unwrap();
    fs.verify_read(&root, Path::new("hard_link")).await.unwrap();

    link_folder(&outside, &root.join("folder_link"));
    #[cfg(windows)]
    link_folder(&outside, &root.join("key"));
    #[cfg(unix)]
    std::os::unix::fs::symlink(outside.join("id_rsa"), root.join("key")).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(root.join("src").join("main.rs"), root.join("inside")).unwrap();

    let mut refused = vec!["folder_link/id_rsa", "key"];
    if cfg!(unix) {
        refused.push("inside");
    }
    for relative in refused {
        let error = fs
            .verify_read(&root, Path::new(relative))
            .await
            .expect_err(relative);
        assert!(
            verified_write_error(&error).is_some(),
            "{relative}: {error:#}"
        );
    }
}

/// ADR-F49: a verified write refuses Git metadata by itself, whoever asks for
/// it, in any spelling that reaches it.
#[gpui::test]
async fn test_write_verified_refuses_git_metadata_by_itself(executor: BackgroundExecutor) {
    let fs = FakeFs::new(executor);
    fs.insert_tree(path!("/root"), json!({ ".git": { "config": "[core]\n" } }))
        .await;
    let root = Path::new(path!("/root"));

    for relative in [".git/config", ".GIT/hooks/pre-commit", "sub/.git/config"] {
        let error = fs
            .write_verified(root, Path::new(relative), b"pwned".to_vec())
            .await
            .expect_err(relative);
        assert!(
            matches!(
                verified_write_error(&error),
                Some(VerifiedWriteError::GitMetadata(_))
            ),
            "{relative}: {error:#}"
        );
    }
    assert_eq!(
        fs.load(path!("/root/.git/config").as_ref()).await.unwrap(),
        "[core]\n"
    );
    assert!(!fs.is_dir(path!("/root/.GIT").as_ref()).await);
}

/// ADR-F49: the account's protected files are refused wherever the root is;
/// a project's own agent configuration is left to the client to confirm.
#[test]
fn test_refusal_for_protects_the_home_directory() {
    let home = Path::new(path!("/home/user"));
    for relative in [
        ".gitconfig",
        ".config/git/config",
        ".claude.json",
        ".claude/settings.json",
        ".ssh/authorized_keys",
        ".bashrc",
    ] {
        assert!(
            matches!(
                verified_write::refusal_for(home, Path::new(relative), home),
                Some(VerifiedWriteError::Protected(_))
            ),
            "{relative}"
        );
    }
    assert!(verified_write::refusal_for(home, Path::new("project/src/main.rs"), home).is_none());
    assert!(
        verified_write::refusal_for(
            &home.join("project"),
            Path::new(".claude/settings.json"),
            home
        )
        .is_none()
    );
}

/// Only Windows (and a Windows drive seen from WSL) reads `name:stream` as
/// `name` and drops trailing dots and spaces; case, invisible characters and
/// compatibility forms fold everywhere.
#[test]
fn test_name_rules_fold_windows_spellings_only_on_windows_filesystems() {
    let windows = verified_write::NameRules { windows: true };
    let elsewhere = verified_write::NameRules { windows: false };
    for spelling in [".git:notes", ".git.", ".git ", ".git::$INDEX_ALLOCATION"] {
        assert_eq!(
            windows.comparable_name(spelling),
            windows.comparable_name(".git"),
            "{spelling}"
        );
        assert_ne!(
            elsewhere.comparable_name(spelling),
            elsewhere.comparable_name(".git"),
            "{spelling}"
        );
    }
    for spelling in [".GIT", ".g\u{200C}it", ".gıt"] {
        assert_eq!(
            elsewhere.comparable_name(spelling),
            elsewhere.comparable_name(".git"),
            "{spelling}"
        );
    }
    assert!(verified_write::NameRules::for_path(Path::new("/mnt/c/project")).windows);
    assert!(!verified_write::NameRules::for_path(Path::new("/mnt/data/project")).windows);
    assert!(!verified_write::NameRules::for_path(Path::new("/home/user/project")).windows);
    assert!(verified_write::same_path_ignoring_case(
        Path::new(r"\\?\C:\P\.GIT"),
        Path::new(r"\\?\c:\p\.git")
    ));
    assert!(!verified_write::same_path_ignoring_case(
        Path::new(r"\\?\C:\p\.git"),
        Path::new(r"\\?\C:\p\GIT~1")
    ));
}

/// ADR-F49: a folder's 8.3 alias (such as `GIT~1` for `.git`) is a second name
/// for it, and a verified write refuses to walk through one.
#[cfg(windows)]
#[gpui::test]
async fn test_real_fs_write_verified_refuses_a_short_name_alias(executor: BackgroundExecutor) {
    use std::os::windows::ffi::{OsStrExt as _, OsStringExt as _};
    use windows::Win32::Storage::FileSystem::GetShortPathNameW;
    use windows::core::PCWSTR;

    let fs = RealFs::new(None, executor);
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().join("project");
    let git_directory = root.join(".git");
    std::fs::create_dir_all(&git_directory).unwrap();
    std::fs::write(git_directory.join("config"), "[core]\n").unwrap();

    let long_path = git_directory
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<u16>>();
    let mut buffer = vec![0u16; 1024];
    let length =
        unsafe { GetShortPathNameW(PCWSTR(long_path.as_ptr()), Some(&mut buffer)) } as usize;
    let short_path = PathBuf::from(std::ffi::OsString::from_wide(
        buffer
            .get(..length)
            .expect("the short path fits its buffer"),
    ));
    // A volume without 8.3 names has no alias to try; the comparison itself
    // is covered by `test_name_rules_fold_windows_spellings_only_on_windows_filesystems`.
    let Some(alias) = short_path
        .file_name()
        .filter(|alias| *alias != std::ffi::OsStr::new(".git"))
    else {
        return;
    };

    let error = fs
        .write_verified(&root, &Path::new(alias).join("config"), b"pwned".to_vec())
        .await
        .unwrap_err();
    assert!(
        matches!(
            verified_write_error(&error),
            Some(VerifiedWriteError::Elsewhere { .. })
        ),
        "{error:#}"
    );
    assert_eq!(
        std::fs::read_to_string(git_directory.join("config")).unwrap(),
        "[core]\n"
    );
}
