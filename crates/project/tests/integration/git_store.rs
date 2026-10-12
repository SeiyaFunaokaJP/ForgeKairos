mod conflict_set_tests {
    use std::sync::mpsc;

    use crate::Project;

    use fs::FakeFs;
    use git::{
        repository::{RepoPath, repo_path},
        status::{UnmergedStatus, UnmergedStatusCode},
    };
    use gpui::{BackgroundExecutor, TestAppContext};
    use project::git_store::*;
    use serde_json::json;
    use text::{Buffer, BufferId, OffsetRangeExt, Point, ReplicaId, ToOffset as _};
    use unindent::Unindent as _;
    use util::{path, rel_path::rel_path};

    #[test]
    fn test_parse_conflicts_in_buffer() {
        // Create a buffer with conflict markers
        let test_content = r#"
            This is some text before the conflict.
            <<<<<<< HEAD
            This is our version
            =======
            This is their version
            >>>>>>> branch-name

            Another conflict:
            <<<<<<< HEAD
            Our second change
            ||||||| merged common ancestors
            Original content
            =======
            Their second change
            >>>>>>> branch-name
        "#
        .unindent();

        let buffer_id = BufferId::new(1).unwrap();
        let buffer = Buffer::new(ReplicaId::LOCAL, buffer_id, test_content);
        let snapshot = buffer.snapshot();

        let conflict_snapshot = ConflictSet::parse(&snapshot);
        assert_eq!(conflict_snapshot.conflicts.len(), 2);

        let first = &conflict_snapshot.conflicts[0];
        assert!(first.base.is_none());
        assert_eq!(first.ours_branch_name.as_ref(), "HEAD");
        assert_eq!(first.theirs_branch_name.as_ref(), "branch-name");
        let our_text = snapshot
            .text_for_range(first.ours.clone())
            .collect::<String>();
        let their_text = snapshot
            .text_for_range(first.theirs.clone())
            .collect::<String>();
        assert_eq!(our_text, "This is our version\n");
        assert_eq!(their_text, "This is their version\n");

        let second = &conflict_snapshot.conflicts[1];
        assert!(second.base.is_some());
        assert_eq!(second.ours_branch_name.as_ref(), "HEAD");
        assert_eq!(second.theirs_branch_name.as_ref(), "branch-name");
        let our_text = snapshot
            .text_for_range(second.ours.clone())
            .collect::<String>();
        let their_text = snapshot
            .text_for_range(second.theirs.clone())
            .collect::<String>();
        let base_text = snapshot
            .text_for_range(second.base.as_ref().unwrap().clone())
            .collect::<String>();
        assert_eq!(our_text, "Our second change\n");
        assert_eq!(their_text, "Their second change\n");
        assert_eq!(base_text, "Original content\n");

        // Test conflicts_in_range
        let range = snapshot.anchor_before(0)..snapshot.anchor_before(snapshot.len());
        let conflicts_in_range = conflict_snapshot.conflicts_in_range(range, &snapshot);
        assert_eq!(conflicts_in_range.len(), 2);

        // Test with a range that includes only the first conflict
        let first_conflict_end = conflict_snapshot.conflicts[0].range.end;
        let range = snapshot.anchor_before(0)..first_conflict_end;
        let conflicts_in_range = conflict_snapshot.conflicts_in_range(range, &snapshot);
        assert_eq!(conflicts_in_range.len(), 1);

        // Test with a range that includes only the second conflict
        let second_conflict_start = conflict_snapshot.conflicts[1].range.start;
        let range = second_conflict_start..snapshot.anchor_before(snapshot.len());
        let conflicts_in_range = conflict_snapshot.conflicts_in_range(range, &snapshot);
        assert_eq!(conflicts_in_range.len(), 1);

        // Test with a range that doesn't include any conflicts
        let range = buffer.anchor_after(first_conflict_end.to_next_offset(&buffer))
            ..buffer.anchor_before(second_conflict_start.to_previous_offset(&buffer));
        let conflicts_in_range = conflict_snapshot.conflicts_in_range(range, &snapshot);
        assert_eq!(conflicts_in_range.len(), 0);
    }

    #[test]
    fn test_parse_conflicts_with_empty_sides() {
        let test_content = r#"
            <<<<<<< HEAD
            =======
            their version
            >>>>>>> branch
            <<<<<<< HEAD
            our version
            =======
            >>>>>>> branch
        "#
        .unindent();

        let mut buffer = Buffer::new(ReplicaId::LOCAL, BufferId::new(1).unwrap(), test_content);
        let conflict_snapshot = ConflictSet::parse(&buffer.snapshot());
        assert_eq!(conflict_snapshot.conflicts.len(), 2);

        for conflict in conflict_snapshot.conflicts.iter() {
            for side in [&conflict.ours, &conflict.theirs] {
                assert!(side.start.cmp(&side.end, &buffer).is_le(), "{side:?}");
            }
        }
        assert_eq!(
            conflict_snapshot.conflicts[0].ours.to_point(&buffer),
            Point::new(1, 0)..Point::new(1, 0)
        );
        assert_eq!(
            conflict_snapshot.conflicts[1].theirs.to_point(&buffer),
            Point::new(7, 0)..Point::new(7, 0)
        );

        // Text typed into an empty side becomes part of that side.
        let ours = conflict_snapshot.conflicts[0].ours.clone();
        buffer.edit([(Point::new(1, 0)..Point::new(1, 0), "resolution\n")]);
        assert_eq!(ours.to_point(&buffer), Point::new(1, 0)..Point::new(2, 0));
    }

    #[test]
    fn test_nested_conflict_markers() {
        // Create a buffer with nested conflict markers
        let test_content = r#"
            This is some text before the conflict.
            <<<<<<< HEAD
            This is our version
            <<<<<<< HEAD
            This is a nested conflict marker
            =======
            This is their version in a nested conflict
            >>>>>>> branch-nested
            =======
            This is their version
            >>>>>>> branch-name
        "#
        .unindent();

        let buffer_id = BufferId::new(1).unwrap();
        let buffer = Buffer::new(ReplicaId::LOCAL, buffer_id, test_content);
        let snapshot = buffer.snapshot();

        let conflict_snapshot = ConflictSet::parse(&snapshot);

        assert_eq!(conflict_snapshot.conflicts.len(), 1);

        // The conflict should have our version, their version, but no base
        let conflict = &conflict_snapshot.conflicts[0];
        assert!(conflict.base.is_none());
        assert_eq!(conflict.ours_branch_name.as_ref(), "HEAD");
        assert_eq!(conflict.theirs_branch_name.as_ref(), "branch-nested");

        // Check that the nested conflict was detected correctly
        let our_text = snapshot
            .text_for_range(conflict.ours.clone())
            .collect::<String>();
        assert_eq!(our_text, "This is a nested conflict marker\n");
        let their_text = snapshot
            .text_for_range(conflict.theirs.clone())
            .collect::<String>();
        assert_eq!(their_text, "This is their version in a nested conflict\n");
    }

    #[test]
    fn test_conflict_markers_at_eof() {
        let test_content = r#"
            <<<<<<< ours
            =======
            This is their version
            >>>>>>> "#
            .unindent();
        let buffer_id = BufferId::new(1).unwrap();
        let buffer = Buffer::new(ReplicaId::LOCAL, buffer_id, test_content);
        let snapshot = buffer.snapshot();

        let conflict_snapshot = ConflictSet::parse(&snapshot);
        assert_eq!(conflict_snapshot.conflicts.len(), 1);
        assert_eq!(
            conflict_snapshot.conflicts[0].ours_branch_name.as_ref(),
            "ours"
        );
        assert_eq!(
            conflict_snapshot.conflicts[0].theirs_branch_name.as_ref(),
            "Origin" // default branch name if there is none
        );
    }

    #[test]
    fn test_conflicts_in_range() {
        // Create a buffer with conflict markers
        let test_content = r#"
            one
            <<<<<<< HEAD1
            two
            =======
            three
            >>>>>>> branch1
            four
            five
            <<<<<<< HEAD2
            six
            =======
            seven
            >>>>>>> branch2
            eight
            nine
            <<<<<<< HEAD3
            ten
            =======
            eleven
            >>>>>>> branch3
            twelve
            <<<<<<< HEAD4
            thirteen
            =======
            fourteen
            >>>>>>> branch4
            fifteen
        "#
        .unindent();

        let buffer_id = BufferId::new(1).unwrap();
        let buffer = Buffer::new(ReplicaId::LOCAL, buffer_id, test_content.clone());
        let snapshot = buffer.snapshot();

        let conflict_snapshot = ConflictSet::parse(&snapshot);
        assert_eq!(conflict_snapshot.conflicts.len(), 4);
        assert_eq!(
            conflict_snapshot.conflicts[0].ours_branch_name.as_ref(),
            "HEAD1"
        );
        assert_eq!(
            conflict_snapshot.conflicts[0].theirs_branch_name.as_ref(),
            "branch1"
        );
        assert_eq!(
            conflict_snapshot.conflicts[1].ours_branch_name.as_ref(),
            "HEAD2"
        );
        assert_eq!(
            conflict_snapshot.conflicts[1].theirs_branch_name.as_ref(),
            "branch2"
        );
        assert_eq!(
            conflict_snapshot.conflicts[2].ours_branch_name.as_ref(),
            "HEAD3"
        );
        assert_eq!(
            conflict_snapshot.conflicts[2].theirs_branch_name.as_ref(),
            "branch3"
        );
        assert_eq!(
            conflict_snapshot.conflicts[3].ours_branch_name.as_ref(),
            "HEAD4"
        );
        assert_eq!(
            conflict_snapshot.conflicts[3].theirs_branch_name.as_ref(),
            "branch4"
        );

        let range = test_content.find("seven").unwrap()..test_content.find("eleven").unwrap();
        let range = buffer.anchor_before(range.start)..buffer.anchor_after(range.end);
        assert_eq!(
            conflict_snapshot.conflicts_in_range(range, &snapshot),
            &conflict_snapshot.conflicts[1..=2]
        );

        let range = test_content.find("one").unwrap()..test_content.find("<<<<<<< HEAD2").unwrap();
        let range = buffer.anchor_before(range.start)..buffer.anchor_after(range.end);
        assert_eq!(
            conflict_snapshot.conflicts_in_range(range, &snapshot),
            &conflict_snapshot.conflicts[0..=1]
        );

        let range =
            test_content.find("eight").unwrap() - 1..test_content.find(">>>>>>> branch3").unwrap();
        let range = buffer.anchor_before(range.start)..buffer.anchor_after(range.end);
        assert_eq!(
            conflict_snapshot.conflicts_in_range(range, &snapshot),
            &conflict_snapshot.conflicts[1..=2]
        );

        let range = test_content.find("thirteen").unwrap() - 1..test_content.len();
        let range = buffer.anchor_before(range.start)..buffer.anchor_after(range.end);
        assert_eq!(
            conflict_snapshot.conflicts_in_range(range, &snapshot),
            &conflict_snapshot.conflicts[3..=3]
        );
    }

    #[gpui::test]
    async fn test_conflict_updates(executor: BackgroundExecutor, cx: &mut TestAppContext) {
        zlog::init_test();
        cx.update(|cx| {
            settings::init(cx);
        });
        let initial_text = "
            one
            two
            three
            four
            five
        "
        .unindent();
        let fs = FakeFs::new(executor);
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "a.txt": initial_text,
            }),
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        let (git_store, buffer) = project.update(cx, |project, cx| {
            (
                project.git_store().clone(),
                project.open_local_buffer(path!("/project/a.txt"), cx),
            )
        });
        let buffer = buffer.await.unwrap();
        let conflict_set = git_store
            .update(cx, |git_store, cx| {
                git_store.open_conflict_set(buffer.clone(), cx)
            })
            .await;
        let (events_tx, events_rx) = mpsc::channel::<ConflictSetUpdate>();
        let _conflict_set_subscription = cx.update(|cx| {
            cx.subscribe(&conflict_set, move |_, event, _| {
                events_tx.send(event.clone()).ok();
            })
        });
        let conflicts_snapshot =
            conflict_set.read_with(cx, |conflict_set, _| conflict_set.snapshot());
        assert!(conflicts_snapshot.conflicts.is_empty());

        buffer.update(cx, |buffer, cx| {
            buffer.edit(
                [
                    (4..4, "<<<<<<< HEAD\n"),
                    (14..14, "=======\nTWO\n>>>>>>> branch\n"),
                ],
                None,
                cx,
            );
        });

        cx.run_until_parked();
        events_rx.try_recv().expect_err(
            "no conflicts should be registered as long as the file's status is unchanged",
        );

        fs.with_git_state(path!("/project/.git").as_ref(), true, |state| {
            state.unmerged_paths.insert(
                repo_path("a.txt"),
                UnmergedStatus {
                    first_head: UnmergedStatusCode::Updated,
                    second_head: UnmergedStatusCode::Updated,
                },
            );
            // Cause the repository to update cached conflicts
            state.refs.insert("MERGE_HEAD".into(), "123".into())
        })
        .unwrap();

        cx.run_until_parked();
        let update = events_rx
            .try_recv()
            .expect("status change should trigger conflict parsing");
        assert_eq!(update.old_range, 0..0);
        assert_eq!(update.new_range, 0..1);

        let conflict = conflict_set.read_with(cx, |conflict_set, _| {
            conflict_set.snapshot().conflicts[0].clone()
        });
        cx.update(|cx| {
            conflict.resolve(buffer.clone(), std::slice::from_ref(&conflict.theirs), cx);
        });

        cx.run_until_parked();
        let update = events_rx
            .try_recv()
            .expect("conflicts should be removed after resolution");
        assert_eq!(update.old_range, 0..1);
        assert_eq!(update.new_range, 0..0);
    }

    #[gpui::test]
    async fn test_conflict_updates_without_merge_head(
        executor: BackgroundExecutor,
        cx: &mut TestAppContext,
    ) {
        zlog::init_test();
        cx.update(|cx| {
            settings::init(cx);
        });

        let initial_text = "
            zero
            <<<<<<< HEAD
            one
            =======
            two
            >>>>>>> Stashed Changes
            three
        "
        .unindent();

        let fs = FakeFs::new(executor);
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "a.txt": initial_text,
            }),
        )
        .await;

        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        let (git_store, buffer) = project.update(cx, |project, cx| {
            (
                project.git_store().clone(),
                project.open_local_buffer(path!("/project/a.txt"), cx),
            )
        });

        cx.run_until_parked();
        fs.with_git_state(path!("/project/.git").as_ref(), true, |state| {
            state.unmerged_paths.insert(
                RepoPath::from_rel_path(rel_path("a.txt")),
                UnmergedStatus {
                    first_head: UnmergedStatusCode::Updated,
                    second_head: UnmergedStatusCode::Updated,
                },
            )
        })
        .unwrap();

        let buffer = buffer.await.unwrap();

        // Open the conflict set for a file that currently has conflicts.
        let conflict_set = git_store
            .update(cx, |git_store, cx| {
                git_store.open_conflict_set(buffer.clone(), cx)
            })
            .await;

        cx.run_until_parked();
        conflict_set.update(cx, |conflict_set, cx| {
            let conflict_range = conflict_set.snapshot().conflicts[0]
                .range
                .to_point(buffer.read(cx));
            assert_eq!(conflict_range, Point::new(1, 0)..Point::new(6, 0));
        });

        // Simulate the conflict being removed by e.g. staging the file.
        fs.with_git_state(path!("/project/.git").as_ref(), true, |state| {
            state.unmerged_paths.remove(&repo_path("a.txt"))
        })
        .unwrap();

        cx.run_until_parked();
        conflict_set.update(cx, |conflict_set, _| {
            assert!(!conflict_set.has_conflict);
            assert_eq!(conflict_set.snapshot.conflicts.len(), 0);
        });

        // Simulate the conflict being re-added.
        fs.with_git_state(path!("/project/.git").as_ref(), true, |state| {
            state.unmerged_paths.insert(
                repo_path("a.txt"),
                UnmergedStatus {
                    first_head: UnmergedStatusCode::Updated,
                    second_head: UnmergedStatusCode::Updated,
                },
            )
        })
        .unwrap();

        cx.run_until_parked();
        conflict_set.update(cx, |conflict_set, cx| {
            let conflict_range = conflict_set.snapshot().conflicts[0]
                .range
                .to_point(buffer.read(cx));
            assert_eq!(conflict_range, Point::new(1, 0)..Point::new(6, 0));
        });
    }

    #[gpui::test]
    async fn test_conflict_updates_with_delayed_merge_head_conflicts(
        executor: BackgroundExecutor,
        cx: &mut TestAppContext,
    ) {
        zlog::init_test();
        cx.update(|cx| {
            settings::init(cx);
        });

        let initial_text = "
            one
            two
            three
            four
        "
        .unindent();

        let conflicted_text = "
            one
            <<<<<<< HEAD
            two
            =======
            TWO
            >>>>>>> branch
            three
            four
        "
        .unindent();

        let resolved_text = "
            one
            TWO
            three
            four
        "
        .unindent();

        let fs = FakeFs::new(executor);
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "a.txt": initial_text,
            }),
        )
        .await;

        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        let (git_store, buffer) = project.update(cx, |project, cx| {
            (
                project.git_store().clone(),
                project.open_local_buffer(path!("/project/a.txt"), cx),
            )
        });
        let buffer = buffer.await.unwrap();
        let conflict_set = git_store
            .update(cx, |git_store, cx| {
                git_store.open_conflict_set(buffer.clone(), cx)
            })
            .await;

        let (events_tx, events_rx) = mpsc::channel::<ConflictSetUpdate>();
        let _conflict_set_subscription = cx.update(|cx| {
            cx.subscribe(&conflict_set, move |_, event, _| {
                events_tx.send(event.clone()).ok();
            })
        });

        cx.run_until_parked();
        events_rx
            .try_recv()
            .expect_err("conflict set should start empty");

        fs.with_git_state(path!("/project/.git").as_ref(), true, |state| {
            state.refs.insert("MERGE_HEAD".into(), "123".into())
        })
        .unwrap();

        cx.run_until_parked();
        events_rx
            .try_recv()
            .expect_err("merge head without conflicted paths should not publish conflicts");
        conflict_set.update(cx, |conflict_set, _| {
            assert!(!conflict_set.has_conflict);
            assert_eq!(conflict_set.snapshot.conflicts.len(), 0);
        });

        buffer.update(cx, |buffer, cx| {
            buffer.set_text(conflicted_text.clone(), cx);
        });
        fs.with_git_state(path!("/project/.git").as_ref(), true, |state| {
            state.unmerged_paths.insert(
                repo_path("a.txt"),
                UnmergedStatus {
                    first_head: UnmergedStatusCode::Updated,
                    second_head: UnmergedStatusCode::Updated,
                },
            );
        })
        .unwrap();

        cx.run_until_parked();
        let update = events_rx
            .try_recv()
            .expect("conflicts should appear once conflicted paths are visible");
        assert_eq!(update.old_range, 0..0);
        assert_eq!(update.new_range, 0..1);
        conflict_set.update(cx, |conflict_set, cx| {
            assert!(conflict_set.has_conflict);
            let conflict_range = conflict_set.snapshot().conflicts[0]
                .range
                .to_point(buffer.read(cx));
            assert_eq!(conflict_range, Point::new(1, 0)..Point::new(6, 0));
        });

        buffer.update(cx, |buffer, cx| {
            buffer.set_text(resolved_text.clone(), cx);
        });

        cx.run_until_parked();
        let update = events_rx
            .try_recv()
            .expect("resolved buffer text should clear visible conflict markers");
        assert_eq!(update.old_range, 0..1);
        assert_eq!(update.new_range, 0..0);
        conflict_set.update(cx, |conflict_set, _| {
            assert!(conflict_set.has_conflict);
            assert_eq!(conflict_set.snapshot.conflicts.len(), 0);
        });

        fs.with_git_state(path!("/project/.git").as_ref(), true, |state| {
            state.refs.insert("MERGE_HEAD".into(), "456".into());
        })
        .unwrap();

        cx.run_until_parked();
        events_rx.try_recv().expect_err(
            "merge-head change without unmerged-path changes should not emit marker updates",
        );
        conflict_set.update(cx, |conflict_set, _| {
            assert!(conflict_set.has_conflict);
            assert_eq!(conflict_set.snapshot.conflicts.len(), 0);
        });

        fs.with_git_state(path!("/project/.git").as_ref(), true, |state| {
            state.unmerged_paths.remove(&repo_path("a.txt"));
            state.refs.remove("MERGE_HEAD");
        })
        .unwrap();

        cx.run_until_parked();
        let update = events_rx.try_recv().expect(
            "status catch-up should emit a no-op update when clearing stale conflict state",
        );
        assert_eq!(update.old_range, 0..0);
        assert_eq!(update.new_range, 0..0);
        assert!(update.buffer_range.is_none());
        conflict_set.update(cx, |conflict_set, _| {
            assert!(!conflict_set.has_conflict);
            assert_eq!(conflict_set.snapshot.conflicts.len(), 0);
        });
    }
}

mod git_traversal {
    use std::{path::Path, time::Duration};

    use collections::HashMap;
    use project::{
        Project,
        git_store::{RepositoryId, RepositorySnapshot},
    };

    use fs::FakeFs;
    use git::status::{
        FileStatus, GitSummary, StatusCode, TrackedSummary, UnmergedStatus, UnmergedStatusCode,
    };
    use gpui::TestAppContext;
    use project::GitTraversal;

    use serde_json::json;
    use settings::SettingsStore;
    use util::{
        path,
        rel_path::{RelPath, rel_path},
    };

    const CONFLICT: FileStatus = FileStatus::Unmerged(UnmergedStatus {
        first_head: UnmergedStatusCode::Updated,
        second_head: UnmergedStatusCode::Updated,
    });
    const ADDED: GitSummary = GitSummary {
        index: TrackedSummary::ADDED,
        count: 1,
        ..GitSummary::UNCHANGED
    };
    const MODIFIED: GitSummary = GitSummary {
        index: TrackedSummary::MODIFIED,
        count: 1,
        ..GitSummary::UNCHANGED
    };

    #[gpui::test]
    async fn test_git_traversal_with_one_repo(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.background_executor.clone());
        fs.insert_tree(
            path!("/root"),
            json!({
                "x": {
                    ".git": {},
                    "x1.txt": "foo",
                    "x2.txt": "bar",
                    "y": {
                        ".git": {},
                        "y1.txt": "baz",
                        "y2.txt": "qux"
                    },
                    "z.txt": "sneaky..."
                },
                "z": {
                    ".git": {},
                    "z1.txt": "quux",
                    "z2.txt": "quuux"
                }
            }),
        )
        .await;

        fs.set_status_for_repo(
            Path::new(path!("/root/x/.git")),
            &[
                ("x2.txt", StatusCode::Modified.index()),
                ("z.txt", StatusCode::Added.index()),
            ],
        );
        fs.set_status_for_repo(Path::new(path!("/root/x/y/.git")), &[("y1.txt", CONFLICT)]);
        fs.set_status_for_repo(
            Path::new(path!("/root/z/.git")),
            &[("z2.txt", StatusCode::Added.index())],
        );

        let project = Project::test(fs, [path!("/root").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let (repo_snapshots, worktree_snapshot) = project.read_with(cx, |project, cx| {
            (
                project.git_store().read(cx).repo_snapshots(cx),
                project.worktrees(cx).next().unwrap().read(cx).snapshot(),
            )
        });

        let traversal = GitTraversal::new(
            &repo_snapshots,
            worktree_snapshot.traverse_from_path(
                true,
                false,
                true,
                RelPath::from_unix_str("x").unwrap(),
            ),
        );
        let entries = traversal
            .map(|entry| (entry.path.clone(), entry.git_summary))
            .collect::<Vec<_>>();
        pretty_assertions::assert_eq!(
            entries,
            [
                (rel_path("x/x1.txt").into(), GitSummary::UNCHANGED),
                (rel_path("x/x2.txt").into(), MODIFIED),
                (rel_path("x/y/y1.txt").into(), GitSummary::CONFLICT),
                (rel_path("x/y/y2.txt").into(), GitSummary::UNCHANGED),
                (rel_path("x/z.txt").into(), ADDED),
                (rel_path("z/z1.txt").into(), GitSummary::UNCHANGED),
                (rel_path("z/z2.txt").into(), ADDED),
            ]
        )
    }

    #[gpui::test]
    async fn test_git_traversal_with_nested_repos(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.background_executor.clone());
        fs.insert_tree(
            path!("/root"),
            json!({
                "x": {
                    ".git": {},
                    "x1.txt": "foo",
                    "x2.txt": "bar",
                    "y": {
                        ".git": {},
                        "y1.txt": "baz",
                        "y2.txt": "qux"
                    },
                    "z.txt": "sneaky..."
                },
                "z": {
                    ".git": {},
                    "z1.txt": "quux",
                    "z2.txt": "quuux"
                }
            }),
        )
        .await;

        fs.set_status_for_repo(
            Path::new(path!("/root/x/.git")),
            &[
                ("x2.txt", StatusCode::Modified.index()),
                ("z.txt", StatusCode::Added.index()),
            ],
        );
        fs.set_status_for_repo(Path::new(path!("/root/x/y/.git")), &[("y1.txt", CONFLICT)]);

        fs.set_status_for_repo(
            Path::new(path!("/root/z/.git")),
            &[("z2.txt", StatusCode::Added.index())],
        );

        let project = Project::test(fs, [path!("/root").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let (repo_snapshots, worktree_snapshot) = project.read_with(cx, |project, cx| {
            (
                project.git_store().read(cx).repo_snapshots(cx),
                project.worktrees(cx).next().unwrap().read(cx).snapshot(),
            )
        });

        // Sanity check the propagation for x/y and z
        check_git_statuses(
            &repo_snapshots,
            &worktree_snapshot,
            &[
                ("x/y", GitSummary::CONFLICT),
                ("x/y/y1.txt", GitSummary::CONFLICT),
                ("x/y/y2.txt", GitSummary::UNCHANGED),
            ],
        );
        check_git_statuses(
            &repo_snapshots,
            &worktree_snapshot,
            &[
                ("z", ADDED),
                ("z/z1.txt", GitSummary::UNCHANGED),
                ("z/z2.txt", ADDED),
            ],
        );

        // Test one of the fundamental cases of propagation blocking, the transition from one git repository to another
        check_git_statuses(
            &repo_snapshots,
            &worktree_snapshot,
            &[
                ("x", MODIFIED + ADDED),
                ("x/y", GitSummary::CONFLICT),
                ("x/y/y1.txt", GitSummary::CONFLICT),
            ],
        );

        // Sanity check everything around it
        check_git_statuses(
            &repo_snapshots,
            &worktree_snapshot,
            &[
                ("x", MODIFIED + ADDED),
                ("x/x1.txt", GitSummary::UNCHANGED),
                ("x/x2.txt", MODIFIED),
                ("x/y", GitSummary::CONFLICT),
                ("x/y/y1.txt", GitSummary::CONFLICT),
                ("x/y/y2.txt", GitSummary::UNCHANGED),
                ("x/z.txt", ADDED),
            ],
        );

        // Test the other fundamental case, transitioning from git repository to non-git repository
        check_git_statuses(
            &repo_snapshots,
            &worktree_snapshot,
            &[
                ("", GitSummary::UNCHANGED),
                ("x", MODIFIED + ADDED),
                ("x/x1.txt", GitSummary::UNCHANGED),
            ],
        );

        // And all together now
        check_git_statuses(
            &repo_snapshots,
            &worktree_snapshot,
            &[
                ("", GitSummary::UNCHANGED),
                ("x", MODIFIED + ADDED),
                ("x/x1.txt", GitSummary::UNCHANGED),
                ("x/x2.txt", MODIFIED),
                ("x/y", GitSummary::CONFLICT),
                ("x/y/y1.txt", GitSummary::CONFLICT),
                ("x/y/y2.txt", GitSummary::UNCHANGED),
                ("x/z.txt", ADDED),
                ("z", ADDED),
                ("z/z1.txt", GitSummary::UNCHANGED),
                ("z/z2.txt", ADDED),
            ],
        );
    }

    #[gpui::test]
    async fn test_git_traversal_simple(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.background_executor.clone());
        fs.insert_tree(
            path!("/root"),
            json!({
                ".git": {},
                "a": {
                    "b": {
                        "c1.txt": "",
                        "c2.txt": "",
                    },
                    "d": {
                        "e1.txt": "",
                        "e2.txt": "",
                        "e3.txt": "",
                    }
                },
                "f": {
                    "no-status.txt": ""
                },
                "g": {
                    "h1.txt": "",
                    "h2.txt": ""
                },
            }),
        )
        .await;

        fs.set_status_for_repo(
            Path::new(path!("/root/.git")),
            &[
                ("a/b/c1.txt", StatusCode::Added.index()),
                ("a/d/e2.txt", StatusCode::Modified.index()),
                ("g/h2.txt", CONFLICT),
            ],
        );

        let project = Project::test(fs, [path!("/root").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let (repo_snapshots, worktree_snapshot) = project.read_with(cx, |project, cx| {
            (
                project.git_store().read(cx).repo_snapshots(cx),
                project.worktrees(cx).next().unwrap().read(cx).snapshot(),
            )
        });

        check_git_statuses(
            &repo_snapshots,
            &worktree_snapshot,
            &[
                ("", GitSummary::CONFLICT + MODIFIED + ADDED),
                ("g", GitSummary::CONFLICT),
                ("g/h2.txt", GitSummary::CONFLICT),
            ],
        );

        check_git_statuses(
            &repo_snapshots,
            &worktree_snapshot,
            &[
                ("", GitSummary::CONFLICT + ADDED + MODIFIED),
                ("a", ADDED + MODIFIED),
                ("a/b", ADDED),
                ("a/b/c1.txt", ADDED),
                ("a/b/c2.txt", GitSummary::UNCHANGED),
                ("a/d", MODIFIED),
                ("a/d/e2.txt", MODIFIED),
                ("f", GitSummary::UNCHANGED),
                ("f/no-status.txt", GitSummary::UNCHANGED),
                ("g", GitSummary::CONFLICT),
                ("g/h2.txt", GitSummary::CONFLICT),
            ],
        );

        check_git_statuses(
            &repo_snapshots,
            &worktree_snapshot,
            &[
                ("a/b", ADDED),
                ("a/b/c1.txt", ADDED),
                ("a/b/c2.txt", GitSummary::UNCHANGED),
                ("a/d", MODIFIED),
                ("a/d/e1.txt", GitSummary::UNCHANGED),
                ("a/d/e2.txt", MODIFIED),
                ("f", GitSummary::UNCHANGED),
                ("f/no-status.txt", GitSummary::UNCHANGED),
                ("g", GitSummary::CONFLICT),
            ],
        );

        check_git_statuses(
            &repo_snapshots,
            &worktree_snapshot,
            &[
                ("a/b/c1.txt", ADDED),
                ("a/b/c2.txt", GitSummary::UNCHANGED),
                ("a/d/e1.txt", GitSummary::UNCHANGED),
                ("a/d/e2.txt", MODIFIED),
                ("f/no-status.txt", GitSummary::UNCHANGED),
            ],
        );
    }

    #[gpui::test]
    async fn test_git_traversal_with_repos_under_project(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.background_executor.clone());
        fs.insert_tree(
            path!("/root"),
            json!({
                "x": {
                    ".git": {},
                    "x1.txt": "foo",
                    "x2.txt": "bar"
                },
                "y": {
                    ".git": {},
                    "y1.txt": "baz",
                    "y2.txt": "qux"
                },
                "z": {
                    ".git": {},
                    "z1.txt": "quux",
                    "z2.txt": "quuux"
                }
            }),
        )
        .await;

        fs.set_status_for_repo(
            Path::new(path!("/root/x/.git")),
            &[("x1.txt", StatusCode::Added.index())],
        );
        fs.set_status_for_repo(
            Path::new(path!("/root/y/.git")),
            &[
                ("y1.txt", CONFLICT),
                ("y2.txt", StatusCode::Modified.index()),
            ],
        );
        fs.set_status_for_repo(
            Path::new(path!("/root/z/.git")),
            &[("z2.txt", StatusCode::Modified.index())],
        );

        let project = Project::test(fs, [path!("/root").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let (repo_snapshots, worktree_snapshot) = project.read_with(cx, |project, cx| {
            (
                project.git_store().read(cx).repo_snapshots(cx),
                project.worktrees(cx).next().unwrap().read(cx).snapshot(),
            )
        });

        check_git_statuses(
            &repo_snapshots,
            &worktree_snapshot,
            &[("x", ADDED), ("x/x1.txt", ADDED)],
        );

        check_git_statuses(
            &repo_snapshots,
            &worktree_snapshot,
            &[
                ("y", GitSummary::CONFLICT + MODIFIED),
                ("y/y1.txt", GitSummary::CONFLICT),
                ("y/y2.txt", MODIFIED),
            ],
        );

        check_git_statuses(
            &repo_snapshots,
            &worktree_snapshot,
            &[("z", MODIFIED), ("z/z2.txt", MODIFIED)],
        );

        check_git_statuses(
            &repo_snapshots,
            &worktree_snapshot,
            &[("x", ADDED), ("x/x1.txt", ADDED)],
        );

        check_git_statuses(
            &repo_snapshots,
            &worktree_snapshot,
            &[
                ("x", ADDED),
                ("x/x1.txt", ADDED),
                ("x/x2.txt", GitSummary::UNCHANGED),
                ("y", GitSummary::CONFLICT + MODIFIED),
                ("y/y1.txt", GitSummary::CONFLICT),
                ("y/y2.txt", MODIFIED),
                ("z", MODIFIED),
                ("z/z1.txt", GitSummary::UNCHANGED),
                ("z/z2.txt", MODIFIED),
            ],
        );
    }

    fn init_test(cx: &mut gpui::TestAppContext) {
        zlog::init_test();

        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
    }

    #[gpui::test]
    async fn test_bump_mtime_of_git_repo_workdir(cx: &mut TestAppContext) {
        init_test(cx);

        // Create a worktree with a git directory.
        let fs = FakeFs::new(cx.background_executor.clone());
        fs.insert_tree(
            path!("/root"),
            json!({
                ".git": {},
                "a.txt": "",
                "b": {
                    "c.txt": "",
                },
            }),
        )
        .await;
        fs.set_head_and_index_for_repo(
            path!("/root/.git").as_ref(),
            &[("a.txt", "".into()), ("b/c.txt", "".into())],
        );
        cx.run_until_parked();

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let (old_entry_ids, old_mtimes) = project.read_with(cx, |project, cx| {
            let tree = project.worktrees(cx).next().unwrap().read(cx);
            (
                tree.entries(true, 0).map(|e| e.id).collect::<Vec<_>>(),
                tree.entries(true, 0).map(|e| e.mtime).collect::<Vec<_>>(),
            )
        });

        // Regression test: after the directory is scanned, touch the git repo's
        // working directory, bumping its mtime. That directory keeps its project
        // entry id after the directories are re-scanned.
        fs.touch_path(path!("/root")).await;
        cx.executor().run_until_parked();

        let (new_entry_ids, new_mtimes) = project.read_with(cx, |project, cx| {
            let tree = project.worktrees(cx).next().unwrap().read(cx);
            (
                tree.entries(true, 0).map(|e| e.id).collect::<Vec<_>>(),
                tree.entries(true, 0).map(|e| e.mtime).collect::<Vec<_>>(),
            )
        });
        assert_eq!(new_entry_ids, old_entry_ids);
        assert_ne!(new_mtimes, old_mtimes);

        // Regression test: changes to the git repository should still be
        // detected.
        fs.set_head_for_repo(
            path!("/root/.git").as_ref(),
            &[("a.txt", "".into()), ("b/c.txt", "something-else".into())],
            "deadbeef",
        );
        cx.executor().run_until_parked();
        cx.executor().advance_clock(Duration::from_secs(1));

        let (repo_snapshots, worktree_snapshot) = project.read_with(cx, |project, cx| {
            (
                project.git_store().read(cx).repo_snapshots(cx),
                project.worktrees(cx).next().unwrap().read(cx).snapshot(),
            )
        });

        check_git_statuses(
            &repo_snapshots,
            &worktree_snapshot,
            &[
                ("", MODIFIED),
                ("a.txt", GitSummary::UNCHANGED),
                ("b/c.txt", MODIFIED),
            ],
        );
    }

    #[track_caller]
    fn check_git_statuses(
        repo_snapshots: &HashMap<RepositoryId, RepositorySnapshot>,
        worktree_snapshot: &worktree::Snapshot,
        expected_statuses: &[(&str, GitSummary)],
    ) {
        let mut traversal = GitTraversal::new(
            repo_snapshots,
            worktree_snapshot.traverse_from_path(true, true, false, RelPath::empty()),
        );
        let found_statuses = expected_statuses
            .iter()
            .map(|&(path, _)| {
                let git_entry = traversal
                    .find(|git_entry| git_entry.path.as_ref() == rel_path(path))
                    .unwrap_or_else(|| panic!("Traversal has no entry for {path:?}"));
                (path, git_entry.git_summary)
            })
            .collect::<Vec<_>>();
        pretty_assertions::assert_eq!(found_statuses, expected_statuses);
    }
}

mod git_worktrees {
    use fs::{FakeFs, Fs};
    use gpui::TestAppContext;
    use project::worktrees_directory_for_repo;
    use serde_json::json;
    use settings::SettingsStore;
    use std::path::{Path, PathBuf};
    use util::{path, paths::PathStyle};

    fn init_test(cx: &mut gpui::TestAppContext) {
        zlog::init_test();

        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
    }

    #[test]
    fn test_validate_worktree_directory() {
        let work_dir = Path::new("/code/my-project");

        // Valid: sibling
        assert!(worktrees_directory_for_repo(work_dir, "../worktrees", PathStyle::Unix).is_ok());

        // Valid: subdirectory
        assert!(
            worktrees_directory_for_repo(work_dir, ".git/zed-worktrees", PathStyle::Unix).is_ok()
        );
        assert!(worktrees_directory_for_repo(work_dir, "my-worktrees", PathStyle::Unix).is_ok());

        // Invalid: just ".." would resolve back to the working directory itself
        let err = worktrees_directory_for_repo(work_dir, "..", PathStyle::Unix).unwrap_err();
        assert!(err.to_string().contains("must not be \"..\""));

        // Invalid: ".." with trailing separators
        let err = worktrees_directory_for_repo(work_dir, "..\\", PathStyle::Unix).unwrap_err();
        assert!(err.to_string().contains("must not be \"..\""));
        let err = worktrees_directory_for_repo(work_dir, "../", PathStyle::Unix).unwrap_err();
        assert!(err.to_string().contains("must not be \"..\""));

        // Invalid: empty string would resolve to the working directory itself
        let err = worktrees_directory_for_repo(work_dir, "", PathStyle::Unix).unwrap_err();
        assert!(err.to_string().contains("must not be empty"));

        // Invalid: absolute path
        let err =
            worktrees_directory_for_repo(work_dir, "/tmp/worktrees", PathStyle::Unix).unwrap_err();
        assert!(err.to_string().contains("relative path"));

        // Invalid: "/" is absolute on Unix
        let err = worktrees_directory_for_repo(work_dir, "/", PathStyle::Unix).unwrap_err();
        assert!(err.to_string().contains("relative path"));

        // Invalid: "///" is absolute
        let err = worktrees_directory_for_repo(work_dir, "///", PathStyle::Unix).unwrap_err();
        assert!(err.to_string().contains("relative path"));

        // Invalid: escapes too far up
        let err = worktrees_directory_for_repo(work_dir, "../../other-project/wt", PathStyle::Unix)
            .unwrap_err();
        assert!(err.to_string().contains("outside"));
    }

    #[test]
    fn test_worktree_directory_uses_remote_path_style() {
        let work_dir = Path::new("/home/user/dev/lsp-tests");

        let directory =
            worktrees_directory_for_repo(work_dir, "../worktrees", PathStyle::Unix).unwrap();

        assert_eq!(
            directory,
            PathBuf::from("/home/user/dev/worktrees/lsp-tests")
        );
    }

    #[gpui::test]
    async fn test_new_worktree_paths_use_bare_repository_identity(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.background_executor.clone());
        fs.insert_tree(
            path!("/zed"),
            json!({
                ".bare": {
                    "worktrees": {
                        "main": {
                            "commondir": "../..",
                        },
                    },
                },
                "main": {
                    ".git": "gitdir: /zed/.bare/worktrees/main",
                    "file.txt": "content",
                },
            }),
        )
        .await;

        let project = Project::test(fs, [path!("/zed/main").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project.repositories(cx).values().next().unwrap().clone()
        });
        let default_path = repository.read_with(cx, |repository, _| {
            repository
                .path_for_new_linked_worktree("plum-warbler", "../worktrees")
                .unwrap()
        });
        let repository_relative_path = repository.read_with(cx, |repository, _| {
            repository
                .path_for_new_linked_worktree("plum-warbler", "worktrees")
                .unwrap()
        });

        assert_eq!(
            default_path,
            PathBuf::from(path!("/worktrees/zed/plum-warbler/zed"))
        );
        assert_eq!(
            repository_relative_path,
            PathBuf::from(path!("/zed/worktrees/plum-warbler/zed"))
        );
    }

    #[gpui::test]
    async fn test_git_worktrees_list_and_create(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.background_executor.clone());
        fs.insert_tree(
            path!("/root"),
            json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project.repositories(cx).values().next().unwrap().clone()
        });

        let worktrees = cx
            .update(|cx| repository.update(cx, |repository, _| repository.worktrees()))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(worktrees.len(), 1);
        assert_eq!(worktrees[0].path, PathBuf::from(path!("/root")));

        let worktrees_directory = PathBuf::from(path!("/root"));
        let worktree_1_directory = worktrees_directory.join("feature-branch");
        cx.update(|cx| {
            repository.update(cx, |repository, _| {
                repository.create_worktree(
                    git::repository::CreateWorktreeTarget::NewBranch {
                        branch_name: "feature-branch".to_string(),
                        base_sha: Some("abc123".to_string()),
                    },
                    worktree_1_directory.clone(),
                )
            })
        })
        .await
        .unwrap()
        .unwrap();

        cx.executor().run_until_parked();

        let worktrees = cx
            .update(|cx| repository.update(cx, |repository, _| repository.worktrees()))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(worktrees.len(), 2);
        assert_eq!(worktrees[0].path, PathBuf::from(path!("/root")));
        assert_eq!(worktrees[1].path, worktree_1_directory);
        assert_eq!(
            worktrees[1].ref_name,
            Some("refs/heads/feature-branch".into())
        );
        assert_eq!(worktrees[1].sha.as_ref(), "abc123");

        let worktree_2_directory = worktrees_directory.join("bugfix-branch");
        cx.update(|cx| {
            repository.update(cx, |repository, _| {
                repository.create_worktree(
                    git::repository::CreateWorktreeTarget::NewBranch {
                        branch_name: "bugfix-branch".to_string(),
                        base_sha: None,
                    },
                    worktree_2_directory.clone(),
                )
            })
        })
        .await
        .unwrap()
        .unwrap();

        cx.executor().run_until_parked();

        // List worktrees — should now have main + two created
        let worktrees = cx
            .update(|cx| repository.update(cx, |repository, _| repository.worktrees()))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(worktrees.len(), 3);

        let worktree_1 = worktrees
            .iter()
            .find(|worktree| worktree.ref_name == Some("refs/heads/feature-branch".into()))
            .expect("should find feature-branch worktree");
        assert_eq!(worktree_1.path, worktree_1_directory);

        let worktree_2 = worktrees
            .iter()
            .find(|worktree| worktree.ref_name == Some("refs/heads/bugfix-branch".into()))
            .expect("should find bugfix-branch worktree");
        assert_eq!(worktree_2.path, worktree_2_directory);
        assert_eq!(worktree_2.sha.as_ref(), "fake-sha");
    }

    #[gpui::test]
    async fn test_remove_worktree_removes_managed_parent_directories(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.background_executor.clone());
        fs.insert_tree(
            path!("/root"),
            json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project.repositories(cx).values().next().unwrap().clone()
        });

        let worktree_path = PathBuf::from(path!("/worktrees/root/feature/nested/root"));
        let worktree_parent = PathBuf::from(path!("/worktrees/root/feature/nested"));
        let worktree_intermediate_parent = PathBuf::from(path!("/worktrees/root/feature"));
        let worktree_base = PathBuf::from(path!("/worktrees/root"));

        cx.update(|cx| {
            repository.update(cx, |repository, _| {
                repository.create_worktree(
                    git::repository::CreateWorktreeTarget::NewBranch {
                        branch_name: "feature/nested".to_string(),
                        base_sha: Some("abc123".to_string()),
                    },
                    worktree_path.clone(),
                )
            })
        })
        .await
        .unwrap()
        .unwrap();

        assert!(Fs::is_dir(fs.as_ref(), &worktree_path).await);
        assert!(Fs::is_dir(fs.as_ref(), &worktree_parent).await);
        assert!(Fs::is_dir(fs.as_ref(), &worktree_intermediate_parent).await);
        assert!(Fs::is_dir(fs.as_ref(), &worktree_base).await);

        cx.update(|cx| {
            repository.update(cx, |repository, _| {
                repository.remove_worktree(worktree_path.clone(), false)
            })
        })
        .await
        .unwrap()
        .unwrap();

        cx.executor().run_until_parked();

        assert!(!Fs::is_dir(fs.as_ref(), &worktree_path).await);
        assert!(!Fs::is_dir(fs.as_ref(), &worktree_parent).await);
        assert!(!Fs::is_dir(fs.as_ref(), &worktree_intermediate_parent).await);
        assert!(Fs::is_dir(fs.as_ref(), &worktree_base).await);
    }

    #[gpui::test]
    async fn test_remove_worktree_uses_bare_repository_identity_for_managed_parent_directories(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let fs = FakeFs::new(cx.background_executor.clone());
        fs.insert_tree(
            path!("/zed"),
            json!({
                ".bare": {
                    "worktrees": {
                        "main": {
                            "commondir": "../..",
                        },
                    },
                },
                "main": {
                    ".git": "gitdir: /zed/.bare/worktrees/main",
                    "file.txt": "content",
                },
            }),
        )
        .await;

        let project = Project::test(fs.clone(), [path!("/zed/main").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project.repositories(cx).values().next().unwrap().clone()
        });
        let worktree_path = repository.read_with(cx, |repository, _| {
            repository
                .path_for_new_linked_worktree("feature/nested", "../worktrees")
                .unwrap()
        });
        let worktree_parent = PathBuf::from(path!("/worktrees/zed/feature/nested"));
        let worktree_intermediate_parent = PathBuf::from(path!("/worktrees/zed/feature"));
        let worktree_base = PathBuf::from(path!("/worktrees/zed"));

        assert_eq!(
            worktree_path,
            PathBuf::from(path!("/worktrees/zed/feature/nested/zed"))
        );

        cx.update(|cx| {
            repository.update(cx, |repository, _| {
                repository.create_worktree(
                    git::repository::CreateWorktreeTarget::NewBranch {
                        branch_name: "feature/nested".to_string(),
                        base_sha: Some("abc123".to_string()),
                    },
                    worktree_path.clone(),
                )
            })
        })
        .await
        .unwrap()
        .unwrap();

        assert!(Fs::is_dir(fs.as_ref(), &worktree_path).await);
        assert!(Fs::is_dir(fs.as_ref(), &worktree_parent).await);
        assert!(Fs::is_dir(fs.as_ref(), &worktree_intermediate_parent).await);
        assert!(Fs::is_dir(fs.as_ref(), &worktree_base).await);

        cx.update(|cx| {
            repository.update(cx, |repository, _| {
                repository.remove_worktree(worktree_path.clone(), false)
            })
        })
        .await
        .unwrap()
        .unwrap();

        cx.executor().run_until_parked();

        assert!(!Fs::is_dir(fs.as_ref(), &worktree_path).await);
        assert!(!Fs::is_dir(fs.as_ref(), &worktree_parent).await);
        assert!(!Fs::is_dir(fs.as_ref(), &worktree_intermediate_parent).await);
        assert!(Fs::is_dir(fs.as_ref(), &worktree_base).await);
    }

    use crate::Project;
}

mod trust_tests {
    use collections::HashSet;
    use fs::FakeFs;
    use gpui::TestAppContext;
    use project::trusted_worktrees::*;

    use serde_json::json;
    use settings::SettingsStore;
    use util::path;

    use crate::Project;

    fn init_test(cx: &mut TestAppContext) {
        zlog::init_test();

        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
    }

    #[gpui::test]
    async fn test_repository_defaults_to_untrusted_without_trust_system(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.background_executor.clone());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "a.txt": "hello",
            }),
        )
        .await;

        // Create project without trust system — repos should default to untrusted.
        let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project.repositories(cx).values().next().unwrap().clone()
        });

        repository.read_with(cx, |repo, _| {
            assert!(
                !repo.is_trusted(),
                "repository should default to untrusted when no trust system is initialized"
            );
        });
    }

    #[gpui::test]
    async fn test_multiple_repos_trust_with_single_worktree(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.background_executor.clone());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "a.txt": "hello",
                "sub": {
                    ".git": {},
                    "b.txt": "world",
                },
            }),
        )
        .await;

        cx.update(|cx| {
            init(DbTrustedPaths::default(), cx);
        });

        let project =
            Project::test_with_worktree_trust(fs.clone(), [path!("/project").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let worktree_store = project.read_with(cx, |project, _| project.worktree_store());
        let worktree_id = worktree_store.read_with(cx, |store, cx| {
            store.worktrees().next().unwrap().read(cx).id()
        });

        let repos = project.read_with(cx, |project, cx| {
            project
                .repositories(cx)
                .values()
                .cloned()
                .collect::<Vec<_>>()
        });
        assert_eq!(repos.len(), 2, "should have two repositories");
        for repo in &repos {
            repo.read_with(cx, |repo, _| {
                assert!(
                    !repo.is_trusted(),
                    "all repos should be untrusted initially"
                );
            });
        }

        let trusted_worktrees = cx
            .update(|cx| TrustedWorktrees::try_get_global(cx).expect("trust global should be set"));
        trusted_worktrees.update(cx, |store, cx| {
            store.trust(
                &worktree_store,
                HashSet::from_iter([PathTrust::Worktree(worktree_id)]),
                cx,
            );
        });
        cx.executor().run_until_parked();

        for repo in &repos {
            repo.read_with(cx, |repo, _| {
                assert!(
                    repo.is_trusted(),
                    "all repos should be trusted after worktree is trusted"
                );
            });
        }
    }

    #[gpui::test]
    async fn test_repository_trust_restrict_trust_cycle(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.background_executor.clone());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "a.txt": "hello",
            }),
        )
        .await;

        cx.update(|cx| {
            project::trusted_worktrees::init(DbTrustedPaths::default(), cx);
        });

        let project =
            Project::test_with_worktree_trust(fs.clone(), [path!("/project").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let worktree_store = project.read_with(cx, |project, _| project.worktree_store());
        let worktree_id = worktree_store.read_with(cx, |store, cx| {
            store.worktrees().next().unwrap().read(cx).id()
        });

        let repository = project.read_with(cx, |project, cx| {
            project.repositories(cx).values().next().unwrap().clone()
        });

        repository.read_with(cx, |repo, _| {
            assert!(!repo.is_trusted(), "repository should start untrusted");
        });

        let trusted_worktrees = cx
            .update(|cx| TrustedWorktrees::try_get_global(cx).expect("trust global should be set"));

        trusted_worktrees.update(cx, |store, cx| {
            store.trust(
                &worktree_store,
                HashSet::from_iter([PathTrust::Worktree(worktree_id)]),
                cx,
            );
        });
        cx.executor().run_until_parked();

        repository.read_with(cx, |repo, _| {
            assert!(
                repo.is_trusted(),
                "repository should be trusted after worktree is trusted"
            );
        });

        trusted_worktrees.update(cx, |store, cx| {
            store.restrict(
                worktree_store.downgrade(),
                HashSet::from_iter([PathTrust::Worktree(worktree_id)]),
                cx,
            );
        });
        cx.executor().run_until_parked();

        repository.read_with(cx, |repo, _| {
            assert!(
                !repo.is_trusted(),
                "repository should be untrusted after worktree is restricted"
            );
        });

        trusted_worktrees.update(cx, |store, cx| {
            store.trust(
                &worktree_store,
                HashSet::from_iter([PathTrust::Worktree(worktree_id)]),
                cx,
            );
        });
        cx.executor().run_until_parked();

        repository.read_with(cx, |repo, _| {
            assert!(
                repo.is_trusted(),
                "repository should be trusted again after second trust"
            );
        });
    }
}

/// Folding changes into an older commit (fixup), against a real git: the plumbing only proves
/// itself when `commit --fixup` and `rebase -i --autosquash` actually run.
mod fixup_tests {
    use std::{
        path::{Path, PathBuf},
        process::Command,
    };

    use collections::HashSet;
    use fs::RealFs;
    use git::repository::{FixupChanges, FixupOutcome, repo_path};
    use gpui::{Entity, TestAppContext};
    use project::{
        Project,
        git_store::Repository,
        trusted_worktrees::{self, DbTrustedPaths, PathTrust, TrustedWorktrees},
    };
    use serde_json::json;
    use settings::SettingsStore;
    use util::test::TempTree;
    use worktree::WorktreeModelHandle as _;

    fn init_test(cx: &mut TestAppContext) {
        zlog::init_test();
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
        cx.executor().allow_parking();
    }

    #[allow(clippy::disallowed_methods)]
    #[track_caller]
    fn git_output(work_dir: &Path, args: &[&str], author: Option<(&str, &str)>) -> String {
        let mut command = Command::new("git");
        command.current_dir(work_dir).args(args);
        if let Some((name, date)) = author {
            command
                .env("GIT_AUTHOR_NAME", name)
                .env("GIT_AUTHOR_EMAIL", format!("{name}@example.com"))
                .env("GIT_AUTHOR_DATE", date);
        }
        let output = command.output().expect("git should run");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_string()
    }

    #[track_caller]
    fn git(work_dir: &Path, args: &[&str]) -> String {
        git_output(work_dir, args, None)
    }

    /// A repository whose behaviour does not depend on the machine's git configuration: the
    /// same identity, line endings and signing for the commits the test makes and the ones the
    /// fixup makes.
    fn init_repository(work_dir: &Path) {
        git(work_dir, &["init", "-b", "main"]);
        for (key, value) in [
            ("user.name", "Committer"),
            ("user.email", "committer@example.com"),
            ("core.autocrlf", "false"),
            ("commit.gpgsign", "false"),
        ] {
            git(work_dir, &["config", key, value]);
        }
    }

    #[track_caller]
    fn commit_file(
        work_dir: &Path,
        path: &str,
        contents: &str,
        message: &str,
        author: (&str, &str),
    ) -> String {
        std::fs::write(work_dir.join(path), contents).expect("the file should be written");
        git(work_dir, &["add", "--", path]);
        git_output(work_dir, &["commit", "-q", "-m", message], Some(author));
        git(work_dir, &["rev-parse", "HEAD"])
    }

    fn write(work_dir: &Path, path: &str, contents: &str) {
        std::fs::write(work_dir.join(path), contents).expect("the file should be written");
    }

    fn read(work_dir: &Path, path: &str) -> String {
        std::fs::read_to_string(work_dir.join(path)).expect("the file should be readable")
    }

    async fn open_repository(
        work_dir: &Path,
        cx: &mut TestAppContext,
    ) -> (Entity<Project>, Entity<Repository>) {
        let project = Project::test(RealFs::new(None, cx.executor()), [work_dir], cx).await;
        let tree = project.read_with(cx, |project, cx| {
            project
                .worktrees(cx)
                .next()
                .expect("the project has a worktree")
        });
        tree.flush_fs_events(cx).await;
        project
            .update(cx, |project, cx| project.git_scans_complete(cx))
            .await;
        cx.executor().run_until_parked();
        let repository = project.read_with(cx, |project, cx| {
            project
                .repositories(cx)
                .values()
                .next()
                .expect("the project has a repository")
                .clone()
        });
        (project, repository)
    }

    async fn fixup(
        repository: &Entity<Repository>,
        sha: &str,
        changes: FixupChanges,
        cx: &mut TestAppContext,
    ) -> anyhow::Result<FixupOutcome> {
        repository
            .update(cx, |repository, cx| {
                repository.fixup_commit(sha.to_string(), changes, cx)
            })
            .await
            .expect("the git job should not be dropped")
    }

    const ANN: (&str, &str) = ("Ann", "1700000000 +0900");
    const BOB: (&str, &str) = ("Bob", "1700000100 +0900");
    const CAROL: (&str, &str) = ("Carol", "1700000200 +0900");

    /// base (base.txt, keep.txt) ← first (a.txt, the target) ← second (b.txt) ← third (c.txt),
    /// returned as (base, first, third).
    fn four_commits(work_dir: &Path) -> (String, String, String) {
        init_repository(work_dir);
        write(work_dir, "keep.txt", "keep\n");
        git(work_dir, &["add", "keep.txt"]);
        let base = commit_file(work_dir, "base.txt", "base\n", "base", ANN);
        let first = commit_file(work_dir, "a.txt", "a\n", "first", ANN);
        commit_file(work_dir, "b.txt", "b\n", "second", BOB);
        let third = commit_file(work_dir, "c.txt", "c\n", "third", CAROL);
        (base, first, third)
    }

    /// A fresh repository made by `setup`, which leaves the changes to fold in place and returns
    /// the commit to fold them into and HEAD, and the fixup of `changes` into that commit.
    async fn fixup_in_fresh_repository(
        cx: &mut TestAppContext,
        setup: impl Fn(&Path) -> (String, String),
        changes: FixupChanges,
    ) -> (TempTree, PathBuf, (String, String), FixupOutcome) {
        let root = TempTree::new(json!({ "project": {} }));
        let work_dir = root.path().join("project");
        let shas = setup(&work_dir);
        let (_project, repository) = open_repository(&work_dir, cx).await;
        let outcome = fixup(&repository, &shas.0, changes, cx)
            .await
            .expect("the fixup should run");
        (root, work_dir, shas, outcome)
    }

    /// The staged changes go into the older commit; the commits after it keep their authors,
    /// dates, messages and changes; and what was not staged stays where it was.
    #[gpui::test]
    async fn test_fixup_folds_the_staged_changes_into_an_older_commit(cx: &mut TestAppContext) {
        init_test(cx);
        let (_root, work_dir, (first, old_head), outcome) = fixup_in_fresh_repository(
            cx,
            |work_dir| {
                let (_base, first, third) = four_commits(work_dir);
                write(work_dir, "a.txt", "a\nfixed\n");
                git(work_dir, &["add", "a.txt"]);
                write(work_dir, "b.txt", "b\nnot staged\n");
                write(work_dir, "untracked.txt", "untracked\n");
                (first, third)
            },
            FixupChanges::Staged,
        )
        .await;

        let FixupOutcome::Folded {
            old_head: reported_old_head,
            new_head,
        } = outcome
        else {
            panic!("expected the changes to be folded in, got {outcome:?}");
        };
        assert_eq!(reported_old_head, old_head);
        assert_eq!(new_head, git(&work_dir, &["rev-parse", "HEAD"]));
        assert_ne!(new_head, old_head);

        // (a) The target carries the change, and nothing else moved into or out of history.
        assert_eq!(git(&work_dir, &["show", "HEAD~2:a.txt"]), "a\nfixed");
        assert_eq!(
            git(&work_dir, &["log", "--format=%s"]),
            "third\nsecond\nfirst\nbase"
        );
        assert_eq!(
            git(&work_dir, &["rev-parse", "HEAD~3"]),
            git(&work_dir, &["rev-parse", &format!("{first}^")]),
            "the commit before the target keeps its SHA"
        );

        // (b) The commits after it are the same commits with new SHAs.
        assert_eq!(
            git(
                &work_dir,
                &["log", "-3", "--format=%an|%ad|%s", "--date=raw"]
            ),
            "Carol|1700000200 +0900|third\nBob|1700000100 +0900|second\nAnn|1700000000 +0900|first"
        );
        assert_eq!(
            git(&work_dir, &["diff", "--name-only", "HEAD~2", "HEAD~1"]),
            "b.txt"
        );
        assert_eq!(
            git(&work_dir, &["diff", "--name-only", "HEAD~1", "HEAD"]),
            "c.txt"
        );
        assert_eq!(git(&work_dir, &["show", "HEAD~1:b.txt"]), "b");

        // (c) The unstaged edit of another file is still in the working tree, still unstaged.
        assert_eq!(read(&work_dir, "b.txt"), "b\nnot staged\n");
        assert_eq!(git(&work_dir, &["diff", "--name-only"]), "b.txt");
        assert_eq!(git(&work_dir, &["diff", "--cached", "--name-only"]), "");

        // (d) The untracked file is untouched, and the autostash left nothing behind.
        assert_eq!(read(&work_dir, "untracked.txt"), "untracked\n");
        assert_eq!(
            git(
                &work_dir,
                &["status", "--porcelain", "--untracked-files=all"]
            ),
            " M b.txt\n?? untracked.txt"
        );
        assert_eq!(git(&work_dir, &["stash", "list"]), "");
    }

    /// Picked files go in exactly as the working tree has them, an untracked one and a deletion
    /// included, and nothing else does: not an unpicked untracked file a glob would have matched,
    /// not another staged change, not another unstaged one.
    #[gpui::test]
    async fn test_fixup_of_picked_files_takes_them_and_nothing_else(cx: &mut TestAppContext) {
        init_test(cx);
        let (_root, work_dir, _shas, outcome) = fixup_in_fresh_repository(
            cx,
            |work_dir| {
                let (_base, first, third) = four_commits(work_dir);
                std::fs::remove_file(work_dir.join("keep.txt"))
                    .expect("keep.txt should be removable");
                write(work_dir, "n[1].txt", "picked\n");
                write(work_dir, "n1.txt", "not picked\n");
                write(work_dir, "c.txt", "c\nstaged elsewhere\n");
                git(work_dir, &["add", "c.txt"]);
                write(work_dir, "a.txt", "a\nnot picked\n");
                (first, third)
            },
            FixupChanges::Paths(vec![repo_path("keep.txt"), repo_path("n[1].txt")]),
        )
        .await;
        assert!(
            matches!(outcome, FixupOutcome::Folded { .. }),
            "expected the files to be folded in, got {outcome:?}"
        );

        assert_eq!(
            git(&work_dir, &["diff", "--name-status", "HEAD~3", "HEAD~2"]),
            "A\ta.txt\nD\tkeep.txt\nA\tn[1].txt"
        );
        assert_eq!(git(&work_dir, &["show", "HEAD~2:n[1].txt"]), "picked");
        assert_eq!(
            git(&work_dir, &["log", "--format=%s"]),
            "third\nsecond\nfirst\nbase"
        );

        assert_eq!(read(&work_dir, "n1.txt"), "not picked\n");
        assert_eq!(read(&work_dir, "c.txt"), "c\nstaged elsewhere\n");
        assert_eq!(read(&work_dir, "a.txt"), "a\nnot picked\n");
        // The rebase's autostash hands the other staged change back unstaged, which is what the
        // confirmation warns about.
        assert_eq!(
            git(
                &work_dir,
                &["status", "--porcelain", "--untracked-files=all"]
            ),
            " M a.txt\n M c.txt\n?? n1.txt"
        );
    }

    /// The repository's first commit has no parent to rebase from, so the rebase starts at the
    /// root.
    #[gpui::test]
    async fn test_fixup_of_the_first_commit(cx: &mut TestAppContext) {
        init_test(cx);
        let (_root, work_dir, _shas, outcome) = fixup_in_fresh_repository(
            cx,
            |work_dir| {
                init_repository(work_dir);
                let first = commit_file(work_dir, "a.txt", "a\n", "first", ANN);
                let second = commit_file(work_dir, "b.txt", "b\n", "second", BOB);
                write(work_dir, "a.txt", "a\nfixed\n");
                (first, second)
            },
            FixupChanges::AllTracked,
        )
        .await;
        assert!(
            matches!(outcome, FixupOutcome::Folded { .. }),
            "expected the changes to be folded in, got {outcome:?}"
        );
        assert_eq!(git(&work_dir, &["show", "HEAD~1:a.txt"]), "a\nfixed");
        assert_eq!(git(&work_dir, &["log", "--format=%s"]), "second\nfirst");
        assert_eq!(git(&work_dir, &["status", "--porcelain"]), "");
    }

    /// A conflict stops the rebase for the banner to deal with, and aborting it leaves the
    /// changes in the `fixup!` commit on top of the branch rather than losing them.
    #[gpui::test]
    async fn test_fixup_stopped_by_a_conflict_keeps_the_changes(cx: &mut TestAppContext) {
        init_test(cx);
        let (_root, work_dir, (_first, old_head), outcome) = fixup_in_fresh_repository(
            cx,
            |work_dir| {
                init_repository(work_dir);
                commit_file(work_dir, "base.txt", "base\n", "base", ANN);
                let first = commit_file(work_dir, "a.txt", "1\n", "first", ANN);
                let second = commit_file(work_dir, "a.txt", "1\n2\n", "second", BOB);
                write(work_dir, "a.txt", "1\n2\n3\n");
                git(work_dir, &["add", "a.txt"]);
                (first, second)
            },
            FixupChanges::Staged,
        )
        .await;
        let FixupOutcome::RebaseStopped {
            old_head: reported_old_head,
            fixup_commit,
            ..
        } = outcome
        else {
            panic!("expected the rebase to stop on the conflict, got {outcome:?}");
        };
        assert_eq!(reported_old_head, old_head);
        git(&work_dir, &["rev-parse", "--verify", "REBASE_HEAD"]);

        git(&work_dir, &["rebase", "--abort"]);
        assert_eq!(
            git(&work_dir, &["log", "--format=%s"]),
            "fixup! first\nsecond\nfirst\nbase"
        );
        // The SHA the toast names for getting the changes back after skipping the pick.
        assert_eq!(git(&work_dir, &["rev-parse", "HEAD"]), fixup_commit);
        assert_eq!(git(&work_dir, &["rev-parse", "HEAD~1"]), old_head);
        assert_eq!(git(&work_dir, &["show", "HEAD:a.txt"]), "1\n2\n3");
        assert_eq!(read(&work_dir, "a.txt"), "1\n2\n3\n");
    }

    /// A rebase that cannot even start leaves nothing replayed, so the `fixup!` commit comes
    /// back off the branch and its changes stay staged: the state before the fixup.
    #[gpui::test]
    async fn test_fixup_whose_rebase_cannot_start_is_taken_back(cx: &mut TestAppContext) {
        init_test(cx);
        let (_root, work_dir, (_first, old_head), outcome) = fixup_in_fresh_repository(
            cx,
            |work_dir| {
                let (_base, first, third) = four_commits(work_dir);
                write(work_dir, "a.txt", "a\nfixed\n");
                git(work_dir, &["add", "a.txt"]);
                // What an interrupted rebase leaves behind; git refuses to start another one
                // over it.
                let rebase_merge = git(work_dir, &["rev-parse", "--git-path", "rebase-merge"]);
                std::fs::create_dir_all(work_dir.join(&rebase_merge))
                    .expect("the directory should be created");
                (first, third)
            },
            FixupChanges::Staged,
        )
        .await;
        assert!(
            matches!(
                outcome,
                FixupOutcome::RebaseRefused {
                    rolled_back: true,
                    ..
                }
            ),
            "expected the fixup to be taken back, got {outcome:?}"
        );
        assert_eq!(git(&work_dir, &["rev-parse", "HEAD"]), old_head);
        assert_eq!(
            git(&work_dir, &["diff", "--cached", "--name-only"]),
            "a.txt"
        );
        assert_eq!(read(&work_dir, "a.txt"), "a\nfixed\n");
    }

    /// Like [`open_repository`], with the worktree trusted so that the repository's hooks run.
    async fn open_trusted_repository(
        work_dir: &Path,
        cx: &mut TestAppContext,
    ) -> (Entity<Project>, Entity<Repository>) {
        let project =
            Project::test_with_worktree_trust(RealFs::new(None, cx.executor()), [work_dir], cx)
                .await;
        let tree = project.read_with(cx, |project, cx| {
            project
                .worktrees(cx)
                .next()
                .expect("the project has a worktree")
        });
        tree.flush_fs_events(cx).await;
        project
            .update(cx, |project, cx| project.git_scans_complete(cx))
            .await;
        let worktree_store = project.read_with(cx, |project, _| project.worktree_store());
        let worktree_id = tree.read_with(cx, |tree, _| tree.id());
        let trusted_worktrees = cx.update(|cx| {
            TrustedWorktrees::try_get_global(cx).expect("the trust store is initialized")
        });
        trusted_worktrees.update(cx, |store, cx| {
            store.trust(
                &worktree_store,
                HashSet::from_iter([PathTrust::Worktree(worktree_id)]),
                cx,
            );
        });
        cx.executor().run_until_parked();
        let repository = project.read_with(cx, |project, cx| {
            project
                .repositories(cx)
                .values()
                .next()
                .expect("the project has a repository")
                .clone()
        });
        assert!(repository.read_with(cx, |repository, _| repository.is_trusted()));
        (project, repository)
    }

    /// git reports a rebase whose `--autostash` could not be put back as a success, keeping the
    /// changes as a stash entry; the fixup says so instead of reporting a clean success.
    #[gpui::test]
    async fn test_fixup_whose_autostash_is_left_in_the_stash_says_so(cx: &mut TestAppContext) {
        init_test(cx);
        cx.update(|cx| trusted_worktrees::init(DbTrustedPaths::default(), cx));
        let root = TempTree::new(json!({ "project": {} }));
        let work_dir = root.path().join("project");
        let (_base, first, _third) = four_commits(&work_dir);
        // Rewrites base.txt, which no commit touches, after the rebase and before git puts the
        // autostash back, which then refuses to overwrite it.
        install_post_rewrite_hook(&work_dir);
        write(&work_dir, "a.txt", "a\nfixed\n");
        git(&work_dir, &["add", "a.txt"]);
        write(&work_dir, "base.txt", "base\nnot staged\n");
        let (_project, repository) = open_trusted_repository(&work_dir, cx).await;

        let outcome = fixup(&repository, &first, FixupChanges::Staged, cx)
            .await
            .expect("the fixup should run");
        assert!(
            matches!(outcome, FixupOutcome::ChangesLeftInStash { .. }),
            "expected the autostash to be reported, got {outcome:?}"
        );
        assert_eq!(git(&work_dir, &["show", "HEAD~2:a.txt"]), "a\nfixed");
        assert_eq!(
            git(&work_dir, &["stash", "list", "--format=%gs"]),
            "autostash"
        );
        assert_eq!(
            git(&work_dir, &["show", "stash@{0}:base.txt"]),
            "base\nnot staged"
        );
    }

    fn install_post_rewrite_hook(work_dir: &Path) {
        let hook = work_dir.join(git(
            work_dir,
            &["rev-parse", "--git-path", "hooks/post-rewrite"],
        ));
        let hooks = hook.parent().expect("the hook has a directory");
        std::fs::create_dir_all(hooks).expect("the hooks directory should be created");
        write(hooks, "post-rewrite", "#!/bin/sh\necho hooked > base.txt\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755))
                .expect("the hook should be made executable");
        }
    }

    /// The reword's `rebase --autostash` is held to the same standard as the fixup's.
    #[gpui::test]
    async fn test_reword_whose_autostash_is_left_in_the_stash_says_so(cx: &mut TestAppContext) {
        init_test(cx);
        cx.update(|cx| trusted_worktrees::init(DbTrustedPaths::default(), cx));
        let root = TempTree::new(json!({ "project": {} }));
        let work_dir = root.path().join("project");
        let (_base, first, _third) = four_commits(&work_dir);
        install_post_rewrite_hook(&work_dir);
        write(&work_dir, "base.txt", "base\nnot staged\n");
        let (_project, repository) = open_trusted_repository(&work_dir, cx).await;

        let result = repository
            .update(cx, |repository, cx| {
                repository.reword_commit(first.clone(), "first, reworded".to_string(), cx)
            })
            .await
            .expect("the git job should not be dropped");
        let error = result.expect_err("the stashed changes did not come back");
        assert!(
            error
                .downcast_ref::<project::git_store::ChangesLeftInStash>()
                .is_some(),
            "unexpected error: {error:#}"
        );
        assert_eq!(
            git(&work_dir, &["log", "--format=%s", "-3"]),
            "third\nsecond\nfirst, reworded"
        );
        assert_eq!(
            git(&work_dir, &["show", "stash@{0}:base.txt"]),
            "base\nnot staged"
        );
    }

    /// Dropping, squashing and reordering use the same `rebase --autostash` and are held to the
    /// same standard: the rewrite is done, and the error says the changes are in the stash.
    #[gpui::test]
    async fn test_other_rewrites_whose_autostash_is_left_in_the_stash_say_so(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        cx.update(|cx| trusted_worktrees::init(DbTrustedPaths::default(), cx));
        for rewrite in ["drop", "squash", "reorder"] {
            let root = TempTree::new(json!({ "project": {} }));
            let work_dir = root.path().join("project");
            let (_base, first, _third) = four_commits(&work_dir);
            let second = git(&work_dir, &["rev-parse", "HEAD~1"]);
            install_post_rewrite_hook(&work_dir);
            write(&work_dir, "base.txt", "base\nnot staged\n");
            let (_project, repository) = open_trusted_repository(&work_dir, cx).await;

            let result = repository
                .update(cx, |repository, cx| match rewrite {
                    "drop" => repository.drop_commit(first.clone(), cx),
                    "squash" => repository.squash_range(
                        first.clone(),
                        second.clone(),
                        "first and second".to_string(),
                        cx,
                    ),
                    _ => repository.rewrite_range(
                        first.clone(),
                        second.clone(),
                        [&second, &first]
                            .map(|sha| git::repository::RewriteGroup {
                                shas: vec![sha.clone()],
                                message: None,
                            })
                            .to_vec(),
                        vec![first.clone(), second.clone()],
                        cx,
                    ),
                })
                .await
                .expect("the git job should not be dropped");
            let error = result.expect_err("the stashed changes did not come back");
            assert!(
                error
                    .downcast_ref::<project::git_store::ChangesLeftInStash>()
                    .is_some(),
                "{rewrite}: unexpected error: {error:#}"
            );
            assert_eq!(
                git(&work_dir, &["log", "--format=%s", "-3"]),
                match rewrite {
                    "drop" => "third\nsecond\nbase",
                    "squash" => "third\nfirst and second\nbase",
                    _ => "third\nfirst\nsecond",
                },
                "{rewrite}: the rewrite itself went through"
            );
            assert_eq!(
                git(&work_dir, &["show", "stash@{0}:base.txt"]),
                "base\nnot staged",
                "{rewrite}"
            );
        }
    }

    /// git refusing the commit itself changes nothing: HEAD stays, and the untracked file added
    /// for the commit goes back to being untracked.
    #[gpui::test]
    async fn test_fixup_whose_commit_is_refused_changes_nothing(cx: &mut TestAppContext) {
        init_test(cx);
        let root = TempTree::new(json!({ "project": {} }));
        let work_dir = root.path().join("project");
        let (_base, first, old_head) = four_commits(&work_dir);
        write(&work_dir, "a.txt", "a\nfixed\n");
        write(&work_dir, "new.txt", "new\n");
        let (_project, repository) = open_repository(&work_dir, cx).await;

        // A picked file that is gone by the time the fixup runs: `add` takes the untracked one,
        // and then `commit --only` refuses the path it cannot find.
        let outcome = fixup(
            &repository,
            &first,
            FixupChanges::Paths(vec![
                repo_path("a.txt"),
                repo_path("new.txt"),
                repo_path("gone.txt"),
            ]),
            cx,
        )
        .await
        .expect("the fixup should run");

        assert!(
            matches!(
                outcome,
                FixupOutcome::NotCommitted {
                    head_moved: false,
                    ..
                }
            ),
            "expected the commit to be refused, got {outcome:?}"
        );
        assert_eq!(git(&work_dir, &["rev-parse", "HEAD"]), old_head);
        assert_eq!(
            git(
                &work_dir,
                &["status", "--porcelain", "--untracked-files=all"]
            ),
            " M a.txt\n?? new.txt"
        );
    }

    #[track_caller]
    fn assert_refused(result: anyhow::Result<FixupOutcome>, reason: &str) {
        match result {
            Err(error) => assert!(
                format!("{error:#}").contains(reason),
                "expected a refusal mentioning {reason:?}, got {error:#}"
            ),
            Ok(outcome) => panic!("expected a refusal mentioning {reason:?}, got {outcome:?}"),
        }
    }

    /// A change to base.txt, which every commit of [`four_commits`] carries unchanged, staged so
    /// that a refusal can only come from the situation under test.
    fn stage_a_change(work_dir: &Path) {
        write(work_dir, "base.txt", "base\nfixed\n");
        git(work_dir, &["add", "base.txt"]);
    }

    /// A refused fixup wrote nothing: the branch is where it was and the change is still staged.
    #[track_caller]
    fn assert_nothing_written(work_dir: &Path, head: &str) {
        assert_eq!(git(work_dir, &["rev-parse", "HEAD"]), head);
        assert_eq!(
            git(work_dir, &["diff", "--cached", "--name-only"]),
            "base.txt"
        );
    }

    /// Nothing to fold in: nothing staged, only untracked files, or a SHA that is not one.
    #[gpui::test]
    async fn test_fixup_refuses_a_fixup_with_nothing_in_it(cx: &mut TestAppContext) {
        init_test(cx);
        let root = TempTree::new(json!({ "project": {} }));
        let work_dir = root.path().join("project");
        let (_base, first, head) = four_commits(&work_dir);
        write(&work_dir, "untracked.txt", "untracked\n");
        let (_project, repository) = open_repository(&work_dir, cx).await;

        assert_refused(
            fixup(&repository, &first, FixupChanges::Staged, cx).await,
            "There are no staged changes",
        );
        assert_refused(
            fixup(&repository, &first, FixupChanges::AllTracked, cx).await,
            "There are no changes to put in the commit",
        );
        assert_refused(
            fixup(&repository, "--all", FixupChanges::AllTracked, cx).await,
            "Cannot read this as a commit SHA",
        );
        assert_eq!(git(&work_dir, &["rev-parse", "HEAD"]), head);
    }

    /// A commit the current branch does not contain, which "すべてのブランチ" shows alongside.
    #[gpui::test]
    async fn test_fixup_refuses_a_commit_off_the_current_branch(cx: &mut TestAppContext) {
        init_test(cx);
        let root = TempTree::new(json!({ "project": {} }));
        let work_dir = root.path().join("project");
        let (base, _first, head) = four_commits(&work_dir);
        git(&work_dir, &["switch", "-q", "-c", "other", &base]);
        let elsewhere = commit_file(&work_dir, "other.txt", "other\n", "elsewhere", BOB);
        git(&work_dir, &["switch", "-q", "main"]);
        stage_a_change(&work_dir);
        let (_project, repository) = open_repository(&work_dir, cx).await;

        assert_refused(
            fixup(&repository, &elsewhere, FixupChanges::Staged, cx).await,
            "not in the history of the current branch",
        );
        assert_nothing_written(&work_dir, &head);
    }

    /// A merge in the range would be flattened by the rebase.
    #[gpui::test]
    async fn test_fixup_refuses_a_range_with_a_merge(cx: &mut TestAppContext) {
        init_test(cx);
        let root = TempTree::new(json!({ "project": {} }));
        let work_dir = root.path().join("project");
        let (_base, first, _head) = four_commits(&work_dir);
        git(&work_dir, &["switch", "-q", "-c", "side", &first]);
        commit_file(&work_dir, "side.txt", "side\n", "side", BOB);
        git(&work_dir, &["switch", "-q", "main"]);
        git(&work_dir, &["merge", "-q", "--no-ff", "--no-edit", "side"]);
        let head = git(&work_dir, &["rev-parse", "HEAD"]);
        stage_a_change(&work_dir);
        let (_project, repository) = open_repository(&work_dir, cx).await;

        assert_refused(
            fixup(&repository, &first, FixupChanges::Staged, cx).await,
            "merge commit",
        );
        assert_nothing_written(&work_dir, &head);
    }

    /// A `fixup!` commit already waiting in the range would be folded in as well, without the
    /// user having asked for it.
    #[gpui::test]
    async fn test_fixup_refuses_a_range_with_a_fixup_commit(cx: &mut TestAppContext) {
        init_test(cx);
        let root = TempTree::new(json!({ "project": {} }));
        let work_dir = root.path().join("project");
        let (_base, first, _head) = four_commits(&work_dir);
        let head = commit_file(&work_dir, "d.txt", "d\n", "fixup! second", BOB);
        stage_a_change(&work_dir);
        let (_project, repository) = open_repository(&work_dir, cx).await;

        assert_refused(
            fixup(&repository, &first, FixupChanges::Staged, cx).await,
            "fixup! / squash!",
        );
        assert_nothing_written(&work_dir, &head);
    }

    /// A cherry-pick stopped on a conflict: a commit now would conclude it instead.
    #[gpui::test]
    async fn test_fixup_refuses_while_a_cherry_pick_is_stopped(cx: &mut TestAppContext) {
        init_test(cx);
        let root = TempTree::new(json!({ "project": {} }));
        let work_dir = root.path().join("project");
        let (base, first, head) = four_commits(&work_dir);
        git(&work_dir, &["switch", "-q", "-c", "clash", &base]);
        let clash = commit_file(&work_dir, "a.txt", "clash\n", "clash", BOB);
        git(&work_dir, &["switch", "-q", "main"]);
        #[allow(clippy::disallowed_methods)]
        let cherry_pick = Command::new("git")
            .current_dir(&work_dir)
            .args(["cherry-pick", &clash])
            .output()
            .expect("git should run");
        assert!(
            !cherry_pick.status.success(),
            "the cherry-pick should stop on the conflict"
        );
        let (_project, repository) = open_repository(&work_dir, cx).await;

        assert_refused(
            fixup(&repository, &first, FixupChanges::AllTracked, cx).await,
            "stopped partway",
        );
        assert_eq!(git(&work_dir, &["rev-parse", "HEAD"]), head);
        git(&work_dir, &["rev-parse", "--verify", "CHERRY_PICK_HEAD"]);
    }

    /// A detached HEAD has no branch for the rebase to move.
    #[gpui::test]
    async fn test_fixup_refuses_a_detached_head(cx: &mut TestAppContext) {
        init_test(cx);
        let root = TempTree::new(json!({ "project": {} }));
        let work_dir = root.path().join("project");
        let (_base, first, head) = four_commits(&work_dir);
        git(&work_dir, &["switch", "-q", "--detach"]);
        stage_a_change(&work_dir);
        let (_project, repository) = open_repository(&work_dir, cx).await;

        assert_refused(
            fixup(&repository, &first, FixupChanges::Staged, cx).await,
            "detached HEAD",
        );
        assert_nothing_written(&work_dir, &head);
    }

    /// Only a commit the upstream already has is "pushed"; a branch without an upstream has
    /// pushed nothing.
    #[gpui::test]
    async fn test_commit_is_pushed_follows_the_upstream(cx: &mut TestAppContext) {
        init_test(cx);
        let root = TempTree::new(json!({ "project": {}, "remote.git": {} }));
        let work_dir = root.path().join("project");
        let remote = root.path().join("remote.git");
        init_repository(&work_dir);
        commit_file(&work_dir, "base.txt", "base\n", "base", ANN);
        let first = commit_file(&work_dir, "a.txt", "a\n", "first", ANN);
        git(&remote, &["init", "-q", "--bare"]);
        // Relative, because git does not take the temporary directory's verbatim (`\\?\`) path
        // for a remote on Windows.
        git(&work_dir, &["remote", "add", "origin", "../remote.git"]);
        let (_project, repository) = open_repository(&work_dir, cx).await;

        let is_pushed = |sha: &str, cx: &mut TestAppContext| {
            repository.update(cx, |repository, _| {
                repository.commit_is_pushed(sha.to_string())
            })
        };
        assert!(
            !is_pushed(&first, cx).await.unwrap().unwrap(),
            "without an upstream nothing counts as pushed"
        );

        git(&work_dir, &["push", "-q", "-u", "origin", "main"]);
        let second = commit_file(&work_dir, "b.txt", "b\n", "second", BOB);
        assert!(is_pushed(&first, cx).await.unwrap().unwrap());
        assert!(!is_pushed(&second, cx).await.unwrap().unwrap());

        let target = repository
            .update(cx, |repository, _| repository.fixup_target(first.clone()))
            .await
            .unwrap()
            .expect("first is a commit changes can be folded into");
        assert_eq!(target.sha, first);
        assert_eq!(target.subject, "first");
        assert!(target.pushed);
    }
}

mod resolve_worktree_tests {
    use fs::FakeFs;
    use gpui::TestAppContext;
    use path::PathStyle;
    use project::{
        git_store::resolve_git_worktree_to_main_repo, linked_worktree_short_name,
        repo_identity_path,
    };
    use serde_json::json;
    use std::path::{Path, PathBuf};

    #[gpui::test]
    async fn test_resolve_git_worktree_to_main_repo(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        // Set up a main repo with a worktree entry
        fs.insert_tree(
            "/main-repo",
            json!({
                ".git": {
                    "worktrees": {
                        "feature": {
                            "commondir": "../../",
                            "HEAD": "ref: refs/heads/feature"
                        }
                    }
                },
                "src": { "main.rs": "" }
            }),
        )
        .await;
        // Set up a worktree checkout pointing back to the main repo
        fs.insert_tree(
            "/worktree-checkout",
            json!({
                ".git": "gitdir: /main-repo/.git/worktrees/feature",
                "src": { "main.rs": "" }
            }),
        )
        .await;

        let result =
            resolve_git_worktree_to_main_repo(fs.as_ref(), Path::new("/worktree-checkout")).await;
        assert_eq!(result, Some(PathBuf::from("/main-repo")));
    }

    #[gpui::test]
    async fn test_resolve_git_worktree_normal_repo_returns_none(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/repo",
            json!({
                ".git": {},
                "src": { "main.rs": "" }
            }),
        )
        .await;

        let result = resolve_git_worktree_to_main_repo(fs.as_ref(), Path::new("/repo")).await;
        assert_eq!(result, None);
    }

    #[gpui::test]
    async fn test_resolve_git_worktree_bare_repo_identity_path(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/monty/.bare",
            json!({
                "worktrees": {
                    "feature-a": {
                        "commondir": "../../",
                        "HEAD": "ref: refs/heads/feature-a"
                    }
                }
            }),
        )
        .await;
        fs.insert_tree(
            "/monty/feature-a",
            json!({
                ".git": "gitdir: /monty/.bare/worktrees/feature-a",
                "src": { "main.rs": "" }
            }),
        )
        .await;

        let result =
            resolve_git_worktree_to_main_repo(fs.as_ref(), Path::new("/monty/feature-a")).await;
        assert_eq!(result, Some(PathBuf::from("/monty")));
    }

    #[gpui::test]
    async fn test_resolve_git_worktree_no_git_returns_none(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/plain",
            json!({
                "src": { "main.rs": "" }
            }),
        )
        .await;

        let result = resolve_git_worktree_to_main_repo(fs.as_ref(), Path::new("/plain")).await;
        assert_eq!(result, None);
    }

    #[gpui::test]
    async fn test_resolve_git_worktree_nonexistent_returns_none(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());

        let result =
            resolve_git_worktree_to_main_repo(fs.as_ref(), Path::new("/does-not-exist")).await;
        assert_eq!(result, None);
    }

    #[test]
    fn test_repo_identity_path() {
        let examples = [
            // Normal checkout: `.git` starts with `.`, so parent is the worktree
            ("/home/bob/zed/.git", "/home/bob/zed"),
            // Bare clone named `.bare`: starts with `.`, so parent is the project dir
            ("/repos/project/.bare", "/repos/project"),
            // Bare clone with `.git` extension: does not start with `.`, kept as-is
            ("/repos/zed.git", "/repos/zed.git"),
            // Bare clone with arbitrary plain name: kept as-is
            ("/repos/project", "/repos/project"),
        ];
        for (common_dir, expected) in examples {
            assert_eq!(
                repo_identity_path(Path::new(common_dir), PathStyle::local()),
                Path::new(expected),
                "identity path for common_dir {common_dir:?} should be {expected:?}"
            );
        }
    }

    #[test]
    fn test_windows_remote_identity_path() {
        let examples = [
            (r"C:\Users\zed\.git", r"C:\Users\zed"),
            (r"C:\Users\project\.bare", r"C:\Users\project"),
            (r"C:\Users\zed.git", r"C:\Users\zed.git"),
            (r"C:\Users\zed", r"C:\Users\zed"),
        ];
        for (common_dir, expected) in examples {
            assert_eq!(
                repo_identity_path(Path::new(common_dir), PathStyle::Windows),
                Path::new(expected),
                "identity path for common_dir {common_dir:?} should be {expected:?}"
            );
        }
    }

    #[test]
    fn test_linked_worktree_short_name() {
        let examples = [
            (
                "/home/bob/zed",
                "/home/bob/worktrees/olivetti/zed",
                Some("olivetti".into()),
            ),
            ("/home/bob/zed", "/home/bob/zed2", Some("zed2".into())),
            (
                "/home/bob/zed",
                "/home/bob/worktrees/zed/selectric",
                Some("selectric".into()),
            ),
            ("/home/bob/zed", "/home/bob/zed", None),
        ];
        for (main_worktree_path, linked_worktree_path, expected) in examples {
            let short_name = linked_worktree_short_name(
                Path::new(main_worktree_path),
                Path::new(linked_worktree_path),
            );
            assert_eq!(
                short_name, expected,
                "short name for {linked_worktree_path:?}, linked worktree of {main_worktree_path:?}, should be {expected:?}"
            );
        }
    }
}

mod repository_activation_tests {
    use std::{
        path::{Path, PathBuf},
        sync::Arc,
    };

    use fs::{FakeFs, Fs, RemoveOptions};
    use gpui::{BorrowAppContext, Entity, TestAppContext};
    use serde_json::json;
    use settings::{LocalSettingsKind, LocalSettingsPath, SettingsStore};
    use util::{
        path,
        rel_path::{RelPath, rel_path},
    };

    use crate::{Project, ProjectPath};

    #[gpui::test]
    async fn test_deep_repositories_park_in_unmanaged_root(cx: &mut TestAppContext) {
        init_test(cx);
        let (_fs, project) = build_project(
            cx,
            json!({
                "repo1": {
                    ".git": {},
                    "a.txt": ""
                },
                "nested": {
                    "deep": {
                        "repo2": {
                            ".git": {},
                            "src": {
                                "b.txt": ""
                            }
                        }
                    }
                }
            }),
        )
        .await;

        assert_repositories(
            &project,
            cx,
            &[path!("/root/repo1")],
            &[path!("/root/nested/deep/repo2")],
        );
        project.read_with(cx, |project, cx| {
            let worktree = project.worktrees(cx).next().unwrap().read(cx);
            assert_eq!(
                worktree
                    .entry_for_path(rel_path("nested/deep/repo2/src/b.txt"))
                    .map(|entry| entry.path.as_ref()),
                Some(rel_path("nested/deep/repo2/src/b.txt"))
            );
        });
    }

    #[gpui::test]
    async fn test_managed_root_never_parks(cx: &mut TestAppContext) {
        init_test(cx);
        let (_fs, project) = build_project(
            cx,
            json!({
                ".git": {},
                "vendor": {
                    "x": {
                        ".git": {},
                        "a.txt": ""
                    }
                }
            }),
        )
        .await;

        assert_repositories(
            &project,
            cx,
            &[path!("/root"), path!("/root/vendor/x")],
            &[],
        );
    }

    #[gpui::test]
    async fn test_parked_repository_activates_on_buffer_open(cx: &mut TestAppContext) {
        init_test(cx);
        let (_fs, project) = build_project(
            cx,
            json!({
                "nested": {
                    "outer": {
                        ".git": {},
                        "inner": {
                            ".git": {},
                            "a.txt": ""
                        },
                        "b.txt": ""
                    }
                }
            }),
        )
        .await;

        assert_repositories(
            &project,
            cx,
            &[],
            &[
                path!("/root/nested/outer"),
                path!("/root/nested/outer/inner"),
            ],
        );

        let _buffer = project
            .update(cx, |project, cx| {
                project.open_local_buffer(path!("/root/nested/outer/inner/a.txt"), cx)
            })
            .await
            .unwrap();
        cx.executor().run_until_parked();

        assert_repositories(
            &project,
            cx,
            &[
                path!("/root/nested/outer"),
                path!("/root/nested/outer/inner"),
            ],
            &[],
        );
    }

    #[gpui::test]
    async fn test_parked_repository_activates_on_save_as(cx: &mut TestAppContext) {
        init_test(cx);
        let (_fs, project) = build_project(
            cx,
            json!({
                "a": {
                    "repo": {
                        ".git": {},
                        "x.txt": ""
                    }
                }
            }),
        )
        .await;

        assert_repositories(&project, cx, &[], &[path!("/root/a/repo")]);

        let buffer = project
            .update(cx, |project, cx| project.create_buffer(None, false, cx))
            .await
            .unwrap();
        let worktree_id = project.read_with(cx, |project, cx| {
            project.worktrees(cx).next().unwrap().read(cx).id()
        });
        project
            .update(cx, |project, cx| {
                project.save_buffer_as(
                    buffer,
                    ProjectPath {
                        worktree_id,
                        path: rel_path("a/repo/new.txt").into(),
                    },
                    cx,
                )
            })
            .await
            .unwrap();
        cx.executor().run_until_parked();

        assert_repositories(&project, cx, &[path!("/root/a/repo")], &[]);
    }

    #[gpui::test]
    async fn test_buffer_open_beyond_scan_horizon_activates_repository(cx: &mut TestAppContext) {
        init_test(cx);
        set_file_scan_depth(cx, 2);
        let (_fs, project) = build_project(
            cx,
            json!({
                "a": {
                    "b": {
                        "repo": {
                            ".git": {},
                            "src": {
                                "deep": {
                                    "f.txt": ""
                                },
                                "g.txt": ""
                            },
                            "docs": {
                                "h.txt": ""
                            }
                        }
                    }
                }
            }),
        )
        .await;

        assert_repositories(&project, cx, &[], &[]);
        project.read_with(cx, |project, cx| {
            let worktree = project.worktrees(cx).next().unwrap().read(cx);
            assert_eq!(worktree.entry_for_path(rel_path("a/b/repo")), None);
        });

        let _buffer = project
            .update(cx, |project, cx| {
                project.open_local_buffer(path!("/root/a/b/repo/src/deep/f.txt"), cx)
            })
            .await
            .unwrap();
        cx.executor().run_until_parked();

        assert_repositories(&project, cx, &[path!("/root/a/b/repo")], &[]);
        project.read_with(cx, |project, cx| {
            let worktree = project.worktrees(cx).next().unwrap().read(cx);
            for path in [
                "a/b/repo/src/deep/f.txt",
                "a/b/repo/src/g.txt",
                "a/b/repo/docs/h.txt",
            ] {
                assert_eq!(
                    worktree
                        .entry_for_path(rel_path(path))
                        .map(|entry| entry.path.as_ref()),
                    Some(rel_path(path))
                );
            }
        });
    }

    #[gpui::test]
    async fn test_project_settings_disable_parking(cx: &mut TestAppContext) {
        init_test(cx);
        let (_fs, project) = build_project(
            cx,
            json!({
                "a": {
                    "repo": {
                        ".git": {},
                        "x.txt": ""
                    }
                }
            }),
        )
        .await;

        assert_repositories(&project, cx, &[], &[path!("/root/a/repo")]);

        let worktree_id = project.read_with(cx, |project, cx| {
            project.worktrees(cx).next().unwrap().read(cx).id()
        });
        cx.update(|cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store
                    .set_local_settings(
                        worktree_id,
                        LocalSettingsPath::InWorktree(Arc::from(RelPath::empty())),
                        LocalSettingsKind::Settings,
                        Some(r#"{ "file_scan_depth": 0 }"#),
                        cx,
                    )
                    .unwrap();
            });
        });
        cx.executor().run_until_parked();

        assert_repositories(&project, cx, &[path!("/root/a/repo")], &[]);
    }

    #[gpui::test]
    async fn test_open_buffer_prevents_parking_late_discovered_repository(cx: &mut TestAppContext) {
        init_test(cx);
        let (fs, project) = build_project(
            cx,
            json!({
                "nested": {
                    "repo": {
                        "a.txt": ""
                    }
                }
            }),
        )
        .await;

        let _buffer = project
            .update(cx, |project, cx| {
                project.open_local_buffer(path!("/root/nested/repo/a.txt"), cx)
            })
            .await
            .unwrap();
        cx.executor().run_until_parked();

        fs.create_dir(Path::new(path!("/root/nested/repo/.git")))
            .await
            .unwrap();
        cx.executor().run_until_parked();

        assert_repositories(&project, cx, &[path!("/root/nested/repo")], &[]);
    }

    #[gpui::test]
    async fn test_parked_repository_dropped_when_dot_git_removed(cx: &mut TestAppContext) {
        init_test(cx);
        let (fs, project) = build_project(
            cx,
            json!({
                "nested": {
                    "repo": {
                        ".git": {},
                        "a.txt": ""
                    }
                }
            }),
        )
        .await;

        assert_repositories(&project, cx, &[], &[path!("/root/nested/repo")]);

        fs.remove_dir(
            Path::new(path!("/root/nested/repo/.git")),
            RemoveOptions {
                recursive: true,
                ignore_if_not_exists: false,
            },
        )
        .await
        .unwrap();
        cx.executor().run_until_parked();

        assert_repositories(&project, cx, &[], &[]);
    }

    #[gpui::test]
    async fn test_repository_shared_between_two_worktrees(cx: &mut TestAppContext) {
        init_test(cx);
        let (_fs, project) = build_project(
            cx,
            json!({
                "nested": {
                    "repo": {
                        ".git": {},
                        "a.txt": ""
                    }
                }
            }),
        )
        .await;

        assert_repositories(&project, cx, &[], &[path!("/root/nested/repo")]);

        let worktree = project
            .update(cx, |project, cx| {
                project.worktree_store().update(cx, |worktree_store, cx| {
                    worktree_store.create_worktree(path!("/root/nested/repo"), true, cx)
                })
            })
            .await
            .unwrap();
        worktree
            .read_with(cx, |worktree, _| {
                worktree.as_local().unwrap().scan_complete()
            })
            .await;
        cx.executor().run_until_parked();

        assert_repositories(&project, cx, &[path!("/root/nested/repo")], &[]);

        project.update(cx, |project, cx| {
            project.git_store().update(cx, |git_store, cx| {
                git_store.activate_all_parked_repositories(cx);
            });
        });
        cx.executor().run_until_parked();

        assert_repositories(&project, cx, &[path!("/root/nested/repo")], &[]);

        let second_worktree_id = worktree.read_with(cx, |worktree, _| worktree.id());
        project.update(cx, |project, cx| {
            project.worktree_store().update(cx, |worktree_store, cx| {
                worktree_store.remove_worktree(second_worktree_id, cx);
            })
        });
        drop(worktree);
        cx.executor().run_until_parked();

        assert_repositories(&project, cx, &[path!("/root/nested/repo")], &[]);
    }

    #[gpui::test]
    async fn test_nested_repository_inside_active_repository_not_parked(cx: &mut TestAppContext) {
        init_test(cx);
        let (_fs, project) = build_project(
            cx,
            json!({
                "x": {
                    ".git": {},
                    "vendor": {
                        "sub": {
                            ".git": {},
                            "f.txt": ""
                        }
                    }
                }
            }),
        )
        .await;

        assert_repositories(
            &project,
            cx,
            &[path!("/root/x"), path!("/root/x/vendor/sub")],
            &[],
        );
    }

    #[gpui::test]
    async fn test_active_repository_survives_file_scan_depth_tightening(cx: &mut TestAppContext) {
        init_test(cx);
        set_file_scan_depth(cx, 0);
        let (_fs, project) = build_project(
            cx,
            json!({
                "a": {
                    "repo": {
                        ".git": {},
                        "x.txt": ""
                    }
                }
            }),
        )
        .await;

        assert_repositories(&project, cx, &[path!("/root/a/repo")], &[]);

        set_file_scan_depth(cx, 1);
        cx.executor().run_until_parked();

        assert_repositories(&project, cx, &[path!("/root/a/repo")], &[]);
        project.read_with(cx, |project, cx| {
            let worktree = project.worktrees(cx).next().unwrap().read(cx);
            assert_eq!(worktree.entry_for_path(rel_path("a/repo/x.txt")), None);
        });
    }

    #[gpui::test]
    async fn test_open_buffer_survives_file_scan_depth_tightening(cx: &mut TestAppContext) {
        init_test(cx);
        let (_fs, project) = build_project(
            cx,
            json!({
                "junk": {
                    "a": {
                        "b": {
                            "deep.txt": "content",
                            "sibling.txt": ""
                        }
                    }
                }
            }),
        )
        .await;

        let buffer = project
            .update(cx, |project, cx| {
                project.open_local_buffer(path!("/root/junk/a/b/deep.txt"), cx)
            })
            .await
            .unwrap();
        cx.executor().run_until_parked();

        let disk_state_before =
            buffer.read_with(cx, |buffer, _| buffer.file().unwrap().disk_state());

        set_file_scan_depth(cx, 1);
        cx.executor().run_until_parked();

        buffer.read_with(cx, |buffer, _| {
            assert_eq!(buffer.file().unwrap().disk_state(), disk_state_before);
        });
        project.read_with(cx, |project, cx| {
            let worktree = project.worktrees(cx).next().unwrap().read(cx);
            assert_eq!(
                worktree
                    .entry_for_path(rel_path("junk/a/b/deep.txt"))
                    .map(|entry| entry.path.as_ref()),
                Some(rel_path("junk/a/b/deep.txt"))
            );
        });
    }

    #[gpui::test]
    async fn test_git_init_at_root_activates_parked_repositories(cx: &mut TestAppContext) {
        init_test(cx);
        let (fs, project) = build_project(
            cx,
            json!({
                "nested": {
                    "repo": {
                        ".git": {},
                        "a.txt": ""
                    }
                }
            }),
        )
        .await;

        assert_repositories(&project, cx, &[], &[path!("/root/nested/repo")]);

        fs.create_dir(Path::new(path!("/root/.git"))).await.unwrap();
        cx.executor().run_until_parked();

        assert_repositories(
            &project,
            cx,
            &[path!("/root"), path!("/root/nested/repo")],
            &[],
        );
    }

    fn init_test(cx: &mut TestAppContext) {
        zlog::init_test();

        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
    }

    async fn build_project(
        cx: &mut TestAppContext,
        tree: serde_json::Value,
    ) -> (Arc<FakeFs>, Entity<Project>) {
        let fs = FakeFs::new(cx.background_executor.clone());
        fs.insert_tree(path!("/root"), tree).await;
        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        cx.executor().run_until_parked();
        (fs, project)
    }

    #[track_caller]
    fn assert_repositories(
        project: &Entity<Project>,
        cx: &mut TestAppContext,
        expected_active: &[&str],
        expected_parked: &[&str],
    ) {
        let (mut active, mut parked) = project.read_with(cx, |project, cx| {
            (
                project
                    .repositories(cx)
                    .values()
                    .map(|repository| repository.read(cx).work_directory_abs_path.to_path_buf())
                    .collect::<Vec<_>>(),
                project
                    .git_store()
                    .read(cx)
                    .parked_repositories()
                    .iter()
                    .map(|parked| parked.work_directory_abs_path().to_path_buf())
                    .collect::<Vec<_>>(),
            )
        });
        active.sort();
        parked.sort();
        assert_eq!(
            active,
            expected_active
                .iter()
                .map(PathBuf::from)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            parked,
            expected_parked
                .iter()
                .map(PathBuf::from)
                .collect::<Vec<_>>()
        );
    }

    fn set_file_scan_depth(cx: &mut TestAppContext, depth: u32) {
        cx.update(|cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.project.worktree.file_scan_depth = Some(depth);
                });
            });
        });
    }
}

mod status_error_tests {
    use std::{path::Path, sync::Arc};

    use fs::FakeFs;
    use gpui::{Entity, TestAppContext};
    use project::git_store::Repository;
    use serde_json::json;
    use settings::SettingsStore;
    use util::path;

    use crate::Project;

    const STATUS_ERROR: &str =
        "git status failed: error: unknown option `no-renames'\nusage: git status [<options>]";

    #[gpui::test]
    async fn test_status_error_is_kept_until_the_next_successful_status(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.background_executor.clone());
        fs.insert_tree(path!("/project"), json!({ ".git": {}, "a.txt": "a" }))
            .await;
        let dot_git = Path::new(path!("/project/.git"));
        fs.set_status_error(dot_git, Some(STATUS_ERROR.to_owned()));

        let (_project, repository) = open_project(&fs, cx).await;
        assert_eq!(status_error(&repository, cx).as_deref(), Some(STATUS_ERROR));
        assert_eq!(status_paths(&repository, cx), Vec::<String>::new());

        fs.set_status_error(dot_git, None);
        cx.executor().run_until_parked();
        assert_eq!(status_error(&repository, cx), None);
        assert_eq!(status_paths(&repository, cx), ["a.txt"]);
    }

    #[gpui::test]
    async fn test_failed_refresh_of_changed_paths_waits_for_a_full_status(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.background_executor.clone());
        fs.insert_tree(path!("/project"), json!({ ".git": {}, "a.txt": "a" }))
            .await;
        let dot_git = Path::new(path!("/project/.git"));

        let (_project, repository) = open_project(&fs, cx).await;
        assert_eq!(status_error(&repository, cx), None);
        assert_eq!(status_paths(&repository, cx), ["a.txt"]);

        // Without a git event only the changed path is refreshed.
        set_status_error_without_git_event(&fs, dot_git, Some(STATUS_ERROR));
        fs.insert_file(path!("/project/b.txt"), b"b".to_vec()).await;
        cx.executor().run_until_parked();
        assert_eq!(status_error(&repository, cx).as_deref(), Some(STATUS_ERROR));
        assert_eq!(status_paths(&repository, cx), ["a.txt"]);

        // Refreshing another path cannot vouch for the one that was dropped.
        set_status_error_without_git_event(&fs, dot_git, None);
        fs.insert_file(path!("/project/c.txt"), b"c".to_vec()).await;
        cx.executor().run_until_parked();
        assert_eq!(status_error(&repository, cx).as_deref(), Some(STATUS_ERROR));
        assert_eq!(status_paths(&repository, cx), ["a.txt", "c.txt"]);

        fs.set_status_error(dot_git, None);
        cx.executor().run_until_parked();
        assert_eq!(status_error(&repository, cx), None);
        assert_eq!(status_paths(&repository, cx), ["a.txt", "b.txt", "c.txt"]);
    }

    #[gpui::test]
    async fn test_repository_open_error_is_reported_as_status_error(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.background_executor.clone());
        fs.insert_tree(path!("/project"), json!({ ".git": {}, "a.txt": "a" }))
            .await;
        fs.with_git_state(Path::new(path!("/project/.git")), false, |state| {
            state.simulated_open_error = Some("no git binary available".to_owned());
        })
        .unwrap();

        let (_project, repository) = open_project(&fs, cx).await;
        let error = status_error(&repository, cx).expect("the open failure should be reported");
        assert!(
            error.contains("no git binary available"),
            "unexpected error: {error}"
        );
    }

    fn init_test(cx: &mut TestAppContext) {
        zlog::init_test();

        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
    }

    async fn open_project(
        fs: &Arc<FakeFs>,
        cx: &mut TestAppContext,
    ) -> (Entity<Project>, Entity<Repository>) {
        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        cx.executor().run_until_parked();
        let repository = project.read_with(cx, |project, cx| {
            project.repositories(cx).values().next().unwrap().clone()
        });
        (project, repository)
    }

    fn set_status_error_without_git_event(fs: &FakeFs, dot_git: &Path, message: Option<&str>) {
        fs.with_git_state(dot_git, false, |state| {
            state.simulated_status_error = message.map(ToOwned::to_owned);
        })
        .unwrap();
    }

    fn status_error(repository: &Entity<Repository>, cx: &mut TestAppContext) -> Option<String> {
        repository.read_with(cx, |repository, _| {
            repository.status_error.as_ref().map(ToString::to_string)
        })
    }

    fn status_paths(repository: &Entity<Repository>, cx: &mut TestAppContext) -> Vec<String> {
        repository.read_with(cx, |repository, _| {
            repository
                .statuses_by_path
                .iter()
                .map(|entry| entry.repo_path.as_unix_str().to_owned())
                .collect()
        })
    }
}

/// ForgeKairos (ADR-F93): the account picked for a repository in the user's settings is who its
/// commits are made as, from the moment the settings say so.
mod git_account_tests {
    use std::{path::Path, process::Command};

    use askpass::AskPassDelegate;
    use fs::RealFs;
    use git::repository::CommitOptions;
    use gpui::{BorrowAppContext as _, Entity, TestAppContext};
    use project::{
        Project,
        git_accounts::{GitAccountChoice, account_choice, project_account_key},
        git_store::Repository,
        project_settings::ProjectSettings,
    };
    use serde_json::json;
    use settings::{GitAccountContent, Settings as _, SettingsStore};
    use util::test::TempTree;
    use worktree::WorktreeModelHandle as _;

    #[allow(clippy::disallowed_methods)]
    #[track_caller]
    fn git(work_dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(work_dir)
            .args(args)
            .output()
            .expect("git should run");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_string()
    }

    fn set_project_account(work_dir: &Path, account_id: Option<&str>, cx: &mut TestAppContext) {
        let key = project_account_key(work_dir);
        let account_id = account_id.map(str::to_string);
        cx.update(|cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    let git = settings.git.get_or_insert_default();
                    git.accounts = Some(vec![GitAccountContent {
                        id: "work".to_string(),
                        name: "Ada Work".to_string(),
                        email: "ada@work.example".to_string(),
                        host: Some("github.com".to_string()),
                        login: Some("ada-work".to_string()),
                        signing_key: None,
                    }]);
                    let project_accounts = git.project_accounts.get_or_insert_default();
                    match account_id {
                        Some(account_id) => {
                            project_accounts.insert(key, account_id);
                        }
                        None => {
                            project_accounts.remove(&key);
                        }
                    }
                })
            })
        });
        cx.executor().run_until_parked();
    }

    async fn commit(repository: &Entity<Repository>, message: &str, cx: &mut TestAppContext) {
        let askpass = AskPassDelegate::new(&mut cx.to_async(), |_, _, _| {});
        repository
            .update(cx, |repository, cx| {
                repository.commit(
                    message.to_string().into(),
                    None,
                    CommitOptions {
                        allow_empty: true,
                        ..CommitOptions::default()
                    },
                    askpass,
                    cx,
                )
            })
            .await
            .expect("the git job should not be dropped")
            .expect("the commit should be made");
    }

    async fn open_repository(
        work_dir: &Path,
        cx: &mut TestAppContext,
    ) -> (Entity<Project>, Entity<Repository>) {
        let project = Project::test(RealFs::new(None, cx.executor()), [work_dir], cx).await;
        let tree = project.read_with(cx, |project, cx| {
            project
                .worktrees(cx)
                .next()
                .expect("the project has a worktree")
        });
        tree.flush_fs_events(cx).await;
        project
            .update(cx, |project, cx| project.git_scans_complete(cx))
            .await;
        cx.executor().run_until_parked();
        let repository = project.read_with(cx, |project, cx| {
            project
                .repositories(cx)
                .values()
                .next()
                .expect("the project has a repository")
                .clone()
        });
        (project, repository)
    }

    #[gpui::test]
    async fn test_commits_are_made_as_the_picked_account(cx: &mut TestAppContext) {
        zlog::init_test();
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
        cx.executor().allow_parking();

        let root = TempTree::new(json!({ "project": {} }));
        let work_dir = root.path().join("project");
        git(&work_dir, &["init"]);
        for (key, value) in [
            ("user.name", "Committer"),
            ("user.email", "committer@example.com"),
            ("commit.gpgsign", "false"),
        ] {
            git(&work_dir, &["config", key, value]);
        }

        let (_project, repository) = open_repository(&work_dir, cx).await;
        let repository_path = repository.read_with(cx, |repository, _| {
            repository.work_directory_abs_path.clone()
        });

        commit(&repository, "before picking", cx).await;
        assert_eq!(
            git(&work_dir, &["log", "-1", "--format=%an <%ae>"]),
            "Committer <committer@example.com>"
        );

        set_project_account(&repository_path, Some("work"), cx);
        assert!(cx.read(|cx| matches!(
            account_choice(&ProjectSettings::get_global(cx).git, &repository_path),
            GitAccountChoice::Account(account) if account.id == "work"
        )));
        commit(&repository, "as the account", cx).await;
        assert_eq!(
            git(&work_dir, &["log", "-1", "--format=%an <%ae>|%cn <%ce>"]),
            "Ada Work <ada@work.example>|Ada Work <ada@work.example>"
        );

        set_project_account(&repository_path, None, cx);
        commit(&repository, "back to git's configuration", cx).await;
        assert_eq!(
            git(&work_dir, &["log", "-1", "--format=%an <%ae>"]),
            "Committer <committer@example.com>"
        );

        // A repository opened while an account is already picked starts out as that account.
        set_project_account(&repository_path, Some("work"), cx);
        let (_reopened_project, reopened) = open_repository(&work_dir, cx).await;
        commit(&reopened, "as the account from the start", cx).await;
        assert_eq!(
            git(&work_dir, &["log", "-1", "--format=%an <%ae>"]),
            "Ada Work <ada@work.example>"
        );
    }
}
