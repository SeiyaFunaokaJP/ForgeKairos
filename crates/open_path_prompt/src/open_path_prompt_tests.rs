use std::path::PathBuf;
use std::sync::{Arc, atomic};

use futures::channel::oneshot;
use fuzzy::{StringMatch, StringMatchCandidate};
use gpui::{AppContext, BorrowAppContext, Entity, TestAppContext, VisualTestContext};
use picker::{Picker, PickerDelegate};
use project::Project;
use serde_json::json;
use settings::SettingsStore;
use util::path;
use workspace::{AppState, MultiWorkspace};

use crate::{CandidateInfo, DirectoryState, OpenPathDelegate, OpenPathPrompt};

#[gpui::test]
async fn test_open_path_prompt(cx: &mut TestAppContext) {
    let app_state = init_test(cx);
    app_state
        .fs
        .as_fake()
        .insert_tree(
            path!("/root"),
            json!({
                ".a1": ".A1",
                ".b1": ".B1",
                "a1": "A1",
                "a2": "A2",
                "a3": "A3",
                "dir1": {},
                "dir2": {
                    "c": "C",
                    "d1": "D1",
                    "d2": "D2",
                    "d3": "D3",
                    "dir3": {},
                    "dir4": {}
                }
            }),
        )
        .await;

    let project = Project::test(app_state.fs.clone(), [path!("/root").as_ref()], cx).await;

    let (picker, cx) = build_open_path_prompt(project, false, false, cx);

    insert_query(path!("sadjaoislkdjasldj"), &picker, cx).await;
    assert_eq!(collect_match_candidates(&picker, cx), Vec::<String>::new());

    let query = path!("/root");
    insert_query(query, &picker, cx).await;
    assert_eq!(collect_match_candidates(&picker, cx), vec!["root"]);

    #[cfg(not(windows))]
    let (expected_separator, expected_parent) = ("./", "../");
    #[cfg(windows)]
    let (expected_separator, expected_parent) = (".\\", "..\\");

    // If the query ends with a slash, the picker should show the contents of the directory and not show any of the hidden entries.
    // The `..` row is offered by the prompt itself: a directory listing never
    // contains one, so without it there is no way out of the directory the
    // prompt opened in short of retyping the whole path.
    let query = path!("/root/");
    insert_query(query, &picker, cx).await;
    assert_eq!(
        collect_match_candidates(&picker, cx),
        vec![
            expected_separator,
            expected_parent,
            "dir1",
            "dir2",
            "a1",
            "a2",
            "a3"
        ]
    );

    // Show candidates for the query "a".
    let query = path!("/root/a");
    insert_query(query, &picker, cx).await;
    assert_eq!(
        collect_match_candidates(&picker, cx),
        vec!["a1", "a2", "a3"]
    );

    // Show candidates for the query "d".
    let query = path!("/root/d");
    insert_query(query, &picker, cx).await;
    assert_eq!(collect_match_candidates(&picker, cx), vec!["dir1", "dir2"]);

    let query = path!("/root/dir2");
    insert_query(query, &picker, cx).await;
    assert_eq!(collect_match_candidates(&picker, cx), vec!["dir2"]);

    let query = path!("/root/dir2/");
    insert_query(query, &picker, cx).await;
    assert_eq!(
        collect_match_candidates(&picker, cx),
        vec![
            expected_separator,
            expected_parent,
            "dir3",
            "dir4",
            "c",
            "d1",
            "d2",
            "d3"
        ]
    );

    // Show candidates for the query "d".
    let query = path!("/root/dir2/d");
    insert_query(query, &picker, cx).await;
    assert_eq!(
        collect_match_candidates(&picker, cx),
        vec!["dir3", "dir4", "d1", "d2", "d3"]
    );

    let query = path!("/root/dir2/di");
    insert_query(query, &picker, cx).await;
    assert_eq!(collect_match_candidates(&picker, cx), vec!["dir3", "dir4"]);

    // With hidden entries off, the only thing "." matches is the way up.
    let query = path!("/root/.");
    insert_query(query, &picker, cx).await;
    assert_eq!(collect_match_candidates(&picker, cx), vec![expected_parent]);

    // Typing `..` reaches the parent row, which is how the prompt is walked up
    // from the keyboard.
    let query = path!("/root/..");
    insert_query(query, &picker, cx).await;
    assert_eq!(collect_match_candidates(&picker, cx), vec![expected_parent]);

    // Don't show any candidates for the query ".a".
    let query = path!("/root/.a");
    insert_query(query, &picker, cx).await;
    assert_eq!(collect_match_candidates(&picker, cx), Vec::<String>::new());

    // Show candidates for the query "./".
    // Should show current directory and contents.
    let query = path!("/root/./");
    insert_query(query, &picker, cx).await;
    assert_eq!(
        collect_match_candidates(&picker, cx),
        vec![
            expected_separator,
            expected_parent,
            "dir1",
            "dir2",
            "a1",
            "a2",
            "a3"
        ]
    );

    // Show candidates for the query "../". Show parent contents.
    let query = path!("/root/dir1/../");
    insert_query(query, &picker, cx).await;
    assert_eq!(
        collect_match_candidates(&picker, cx),
        vec![
            expected_separator,
            expected_parent,
            "dir1",
            "dir2",
            "a1",
            "a2",
            "a3"
        ]
    );

    // The root has nothing above it to offer.
    let query = path!("/");
    insert_query(query, &picker, cx).await;
    assert!(
        !collect_match_candidates(&picker, cx).contains(&expected_parent.to_string()),
        "the root must not offer a parent row"
    );
}

#[gpui::test]
async fn test_open_path_prompt_completion(cx: &mut TestAppContext) {
    let app_state = init_test(cx);
    app_state
        .fs
        .as_fake()
        .insert_tree(
            path!("/root"),
            json!({
                "a": "A",
                "dir1": {},
                "dir2": {
                    "c": "C",
                    "d": "D",
                    "dir3": {},
                    "dir4": {}
                }
            }),
        )
        .await;

    let project = Project::test(app_state.fs.clone(), [path!("/root").as_ref()], cx).await;

    let (picker, cx) = build_open_path_prompt(project, false, false, cx);

    // Confirm completion for the query "/root", since it's a directory, it should add a trailing slash.
    let query = path!("/root");
    insert_query(query, &picker, cx).await;
    assert_eq!(
        confirm_completion(query, 0, &picker, cx).unwrap(),
        path!("/root/")
    );

    // Confirm completion for the query "/root/", selecting the first candidate "dir1", since it's a directory, it should add a trailing slash.
    let query = path!("/root/");
    insert_query(query, &picker, cx).await;
    assert_eq!(
        confirm_completion(query, 0, &picker, cx),
        None,
        "First entry is `./` and when we confirm completion, it is tabbed below"
    );
    assert_eq!(
        confirm_completion(query, 1, &picker, cx).unwrap(),
        path!("/"),
        "Second entry is `..`, which completes to the parent directory"
    );
    assert_eq!(
        confirm_completion(query, 2, &picker, cx).unwrap(),
        path!("/root/dir1/"),
        "Third entry is the first directory"
    );

    // Confirm completion for the query "/root/", selecting the candidate "a", since it's a file, it should not add a trailing slash.
    let query = path!("/root/");
    insert_query(query, &picker, cx).await;
    assert_eq!(
        confirm_completion(query, 4, &picker, cx).unwrap(),
        path!("/root/a")
    );

    let query = path!("/root/a");
    insert_query(query, &picker, cx).await;
    assert_eq!(
        confirm_completion(query, 0, &picker, cx).unwrap(),
        path!("/root/a")
    );

    let query = path!("/root/d");
    insert_query(query, &picker, cx).await;
    assert_eq!(
        confirm_completion(query, 1, &picker, cx).unwrap(),
        path!("/root/dir2/")
    );

    let query = path!("/root/dir2");
    insert_query(query, &picker, cx).await;
    assert_eq!(
        confirm_completion(query, 0, &picker, cx).unwrap(),
        path!("/root/dir2/")
    );

    let query = path!("/root/dir2/");
    insert_query(query, &picker, cx).await;
    assert_eq!(
        confirm_completion(query, 1, &picker, cx).unwrap(),
        path!("/root/"),
        "`..` completes to the parent, not to a child of the current directory"
    );
    assert_eq!(
        confirm_completion(query, 2, &picker, cx).unwrap(),
        path!("/root/dir2/dir3/")
    );

    let query = path!("/root/dir2/");
    insert_query(query, &picker, cx).await;
    assert_eq!(
        confirm_completion(query, 4, &picker, cx).unwrap(),
        path!("/root/dir2/c")
    );

    let query = path!("/root/dir2/d");
    insert_query(query, &picker, cx).await;
    assert_eq!(
        confirm_completion(query, 0, &picker, cx).unwrap(),
        path!("/root/dir2/dir3/")
    );

    let query = path!("/root/dir2/d");
    insert_query(query, &picker, cx).await;
    assert_eq!(
        confirm_completion(query, 1, &picker, cx).unwrap(),
        path!("/root/dir2/dir4/")
    );

    let query = path!("/root/dir2/di");
    insert_query(query, &picker, cx).await;
    assert_eq!(
        confirm_completion(query, 1, &picker, cx).unwrap(),
        path!("/root/dir2/dir4/")
    );
}

#[gpui::test]
#[cfg_attr(not(target_os = "windows"), ignore)]
async fn test_open_path_prompt_on_windows(cx: &mut TestAppContext) {
    let app_state = init_test(cx);
    app_state
        .fs
        .as_fake()
        .insert_tree(
            path!("/root"),
            json!({
                "a": "A",
                "dir1": {},
                "dir2": {}
            }),
        )
        .await;

    let project = Project::test(app_state.fs.clone(), [path!("/root").as_ref()], cx).await;

    let (picker, cx) = build_open_path_prompt(project, false, false, cx);

    // Support both forward and backward slashes.
    let query = "C:/root/";
    insert_query(query, &picker, cx).await;
    assert_eq!(
        collect_match_candidates(&picker, cx),
        vec![".\\", "..\\", "dir1", "dir2", "a"]
    );
    assert_eq!(
        confirm_completion(query, 0, &picker, cx),
        None,
        "First entry is `.\\` and when we confirm completion, it is tabbed below"
    );
    assert_eq!(
        confirm_completion(query, 1, &picker, cx).unwrap(),
        "C:\\",
        "Second entry is `..`, which completes to the parent directory"
    );
    assert_eq!(
        confirm_completion(query, 2, &picker, cx).unwrap(),
        "C:/root/dir1\\",
        "Third entry is the first directory"
    );

    let query = "C:\\root/";
    insert_query(query, &picker, cx).await;
    assert_eq!(
        collect_match_candidates(&picker, cx),
        vec![".\\", "..\\", "dir1", "dir2", "a"]
    );
    assert_eq!(
        confirm_completion(query, 2, &picker, cx).unwrap(),
        "C:\\root/dir1\\"
    );

    let query = "C:\\root\\";
    insert_query(query, &picker, cx).await;
    assert_eq!(
        collect_match_candidates(&picker, cx),
        vec![".\\", "..\\", "dir1", "dir2", "a"]
    );
    assert_eq!(
        confirm_completion(query, 2, &picker, cx).unwrap(),
        "C:\\root\\dir1\\"
    );

    // Confirm completion for the query "C:/root/d", selecting the second candidate "dir2", since it's a directory, it should add a trailing slash.
    let query = "C:/root/d";
    insert_query(query, &picker, cx).await;
    assert_eq!(collect_match_candidates(&picker, cx), vec!["dir1", "dir2"]);
    assert_eq!(
        confirm_completion(query, 1, &picker, cx).unwrap(),
        "C:/root/dir2\\"
    );

    let query = "C:\\root/d";
    insert_query(query, &picker, cx).await;
    assert_eq!(collect_match_candidates(&picker, cx), vec!["dir1", "dir2"]);
    assert_eq!(
        confirm_completion(query, 0, &picker, cx).unwrap(),
        "C:\\root/dir1\\"
    );

    let query = "C:\\root\\d";
    insert_query(query, &picker, cx).await;
    assert_eq!(collect_match_candidates(&picker, cx), vec!["dir1", "dir2"]);
    assert_eq!(
        confirm_completion(query, 0, &picker, cx).unwrap(),
        "C:\\root\\dir1\\"
    );
}

#[gpui::test]
async fn test_new_path_prompt(cx: &mut TestAppContext) {
    let app_state = init_test(cx);
    app_state
        .fs
        .as_fake()
        .insert_tree(
            path!("/root"),
            json!({
                "a1": "A1",
                "a2": "A2",
                "a3": "A3",
                "dir1": {},
                "dir2": {
                    "c": "C",
                    "d1": "D1",
                    "d2": "D2",
                    "d3": "D3",
                    "dir3": {},
                    "dir4": {}
                }
            }),
        )
        .await;

    let project = Project::test(app_state.fs.clone(), [path!("/root").as_ref()], cx).await;

    let (picker, cx) = build_open_path_prompt(project, true, false, cx);

    insert_query(path!("/root"), &picker, cx).await;
    assert_eq!(collect_match_candidates(&picker, cx), vec!["root"]);

    insert_query(path!("/root/d"), &picker, cx).await;
    assert_eq!(
        collect_match_candidates(&picker, cx),
        vec!["d", "dir1", "dir2"]
    );

    insert_query(path!("/root/dir1"), &picker, cx).await;
    assert_eq!(collect_match_candidates(&picker, cx), vec!["dir1"]);

    insert_query(path!("/root/dir12"), &picker, cx).await;
    assert_eq!(collect_match_candidates(&picker, cx), vec!["dir12"]);

    insert_query(path!("/root/dir1"), &picker, cx).await;
    assert_eq!(collect_match_candidates(&picker, cx), vec!["dir1"]);
}

#[gpui::test]
async fn test_open_path_prompt_panics_with_stale_highlight_positions(cx: &mut TestAppContext) {
    let app_state = init_test(cx);
    app_state
        .fs
        .as_fake()
        .insert_tree(path!("/root"), json!({}))
        .await;

    let project = Project::test(app_state.fs.clone(), [path!("/root").as_ref()], cx).await;
    let (picker, cx) = build_open_path_prompt(project, false, false, cx);

    picker.update_in(cx, |picker, window, cx| {
        picker.delegate.prompt_root = "/".to_string();
        picker.delegate.directory_state = DirectoryState::List {
            parent_path: picker.delegate.prompt_root.clone(),
            entries: vec![CandidateInfo {
                path: StringMatchCandidate::new(0, "éclair"),
                is_dir: false,
            }],
            error: None,
        };
        picker.delegate.string_matches = vec![StringMatch {
            candidate_id: 0,
            score: 0.0,
            positions: vec![1],
            string: "ab".to_string(),
        }];

        picker.delegate.render_match(0, false, window, cx);
    });
}

#[gpui::test]
async fn test_open_path_prompt_with_show_hidden(cx: &mut TestAppContext) {
    let app_state = init_test(cx);
    app_state
        .fs
        .as_fake()
        .insert_tree(
            path!("/root"),
            json!({
                "directory_1": {},
                "directory_2": {},
                ".hidden": {}
            }),
        )
        .await;

    let project = Project::test(app_state.fs.clone(), [path!("/root").as_ref()], cx).await;

    let (picker, cx) = build_open_path_prompt(project, false, true, cx);

    #[cfg(not(windows))]
    let (expected_separator, expected_parent) = ("./", "../");
    #[cfg(windows)]
    let (expected_separator, expected_parent) = (".\\", "..\\");

    insert_query(path!("/root/"), &picker, cx).await;
    assert_eq!(
        collect_match_candidates(&picker, cx),
        vec![
            expected_separator,
            expected_parent,
            ".hidden",
            "directory_1",
            "directory_2"
        ]
    );

    insert_query(path!("/root/."), &picker, cx).await;
    assert_eq!(
        collect_match_candidates(&picker, cx),
        vec![".hidden", expected_parent]
    );
}

/// Confirming the `..` row has to browse upwards. Sending `<dir>/..` back as the
/// chosen path instead would open a worktree at an un-normalized path, and would
/// close the prompt when the user meant to keep looking.
#[gpui::test]
async fn test_confirming_the_parent_row_navigates_up(cx: &mut TestAppContext) {
    let app_state = init_test(cx);
    app_state
        .fs
        .as_fake()
        .insert_tree(
            path!("/root"),
            json!({
                "dir1": {
                    "dir2": {}
                }
            }),
        )
        .await;

    let project = Project::test(app_state.fs.clone(), [path!("/root").as_ref()], cx).await;
    let (picker, cx) = build_open_path_prompt(project, false, false, cx);

    insert_query(path!("/root/dir1/"), &picker, cx).await;
    picker.update_in(cx, |picker, window, cx| {
        picker.delegate.set_selected_index(1, window, cx);
        picker.delegate.confirm(false, window, cx);
    });
    cx.run_until_parked();

    assert_eq!(
        picker.read_with(cx, |picker, cx| picker.query(cx)),
        path!("/root/"),
        "confirming `..` moves the prompt to the parent directory"
    );
    assert!(
        collect_match_candidates(&picker, cx).contains(&"dir1".to_string()),
        "the parent's contents are listed after navigating up"
    );
}

/// Confirming a folder has to browse into it. Handing it back as the chosen
/// path instead makes the prompt useless for reaching anything below the
/// directory it opened in: every folder on the way down would close the prompt.
#[gpui::test]
async fn test_confirming_a_directory_browses_into_it(cx: &mut TestAppContext) {
    let app_state = init_test(cx);
    app_state
        .fs
        .as_fake()
        .insert_tree(
            path!("/root"),
            json!({
                "dir1": {
                    "dir2": {}
                }
            }),
        )
        .await;

    let project = Project::test(app_state.fs.clone(), [path!("/root").as_ref()], cx).await;
    let (picker, mut receiver, cx) =
        build_open_path_prompt_with_receiver(project, false, false, cx);

    insert_query(path!("/root/"), &picker, cx).await;
    assert_eq!(
        collect_match_candidates(&picker, cx)
            .get(2)
            .map(String::as_str),
        Some("dir1"),
        "the third row is the first real entry, after `./` and `../`"
    );
    picker.update_in(cx, |picker, window, cx| {
        picker.delegate.set_selected_index(2, window, cx);
        picker.delegate.confirm(false, window, cx);
    });
    cx.run_until_parked();

    assert_eq!(
        picker.read_with(cx, |picker, cx| picker.query(cx)),
        path!("/root/dir1/"),
        "confirming a folder lists its contents instead of picking it"
    );
    assert!(
        collect_match_candidates(&picker, cx).contains(&"dir2".to_string()),
        "the folder's contents are listed after browsing into it"
    );
    assert_eq!(
        receiver.try_recv().ok().flatten(),
        None,
        "browsing must not answer the prompt"
    );
}

/// `menu::SecondaryConfirm` (`ctrl-enter`) is how the highlighted folder is
/// chosen, now that plain confirm browses into it.
#[gpui::test]
async fn test_secondary_confirm_picks_the_selected_directory(cx: &mut TestAppContext) {
    let app_state = init_test(cx);
    app_state
        .fs
        .as_fake()
        .insert_tree(
            path!("/root"),
            json!({
                "dir1": {
                    "dir2": {}
                }
            }),
        )
        .await;

    let project = Project::test(app_state.fs.clone(), [path!("/root").as_ref()], cx).await;
    let (picker, receiver, cx) = build_open_path_prompt_with_receiver(project, false, false, cx);

    insert_query(path!("/root/"), &picker, cx).await;
    picker.update_in(cx, |picker, window, cx| {
        picker.delegate.set_selected_index(2, window, cx);
        picker.delegate.confirm(true, window, cx);
    });
    cx.run_until_parked();

    assert_eq!(
        receiver.await.ok().flatten(),
        Some(vec![PathBuf::from(path!("/root/dir1"))]),
        "ctrl-enter picks the highlighted folder"
    );
}

/// The `./` row stands for the directory being listed, so it must hand back that
/// directory itself — not `<dir>/./`, which would become the worktree's path.
#[gpui::test]
async fn test_confirming_the_current_dir_row_picks_the_listed_directory(cx: &mut TestAppContext) {
    let app_state = init_test(cx);
    app_state
        .fs
        .as_fake()
        .insert_tree(
            path!("/root"),
            json!({
                "dir1": {
                    "dir2": {}
                }
            }),
        )
        .await;

    let project = Project::test(app_state.fs.clone(), [path!("/root").as_ref()], cx).await;
    let (picker, receiver, cx) = build_open_path_prompt_with_receiver(project, false, false, cx);

    insert_query(path!("/root/dir1/"), &picker, cx).await;
    picker.update_in(cx, |picker, window, cx| {
        picker.delegate.set_selected_index(0, window, cx);
        picker.delegate.confirm(false, window, cx);
    });
    cx.run_until_parked();

    assert_eq!(
        receiver.await.ok().flatten(),
        Some(vec![PathBuf::from(path!("/root/dir1"))]),
        "confirming `./` picks the directory whose contents are listed"
    );
}

/// Files still open on confirm; only folders browse.
#[gpui::test]
async fn test_confirming_a_file_still_picks_it(cx: &mut TestAppContext) {
    let app_state = init_test(cx);
    app_state
        .fs
        .as_fake()
        .insert_tree(
            path!("/root"),
            json!({
                "a1": "A1",
            }),
        )
        .await;

    let project = Project::test(app_state.fs.clone(), [path!("/root").as_ref()], cx).await;
    let (picker, receiver, cx) = build_open_path_prompt_with_receiver(project, false, false, cx);

    insert_query(path!("/root/a1"), &picker, cx).await;
    picker.update_in(cx, |picker, window, cx| {
        picker.delegate.set_selected_index(0, window, cx);
        picker.delegate.confirm(false, window, cx);
    });
    cx.run_until_parked();

    assert_eq!(
        receiver.await.ok().flatten(),
        Some(vec![PathBuf::from(path!("/root/a1"))]),
    );
}

#[gpui::test]
async fn test_dismiss_cancels_in_flight_match(cx: &mut TestAppContext) {
    let app_state = init_test(cx);
    app_state
        .fs
        .as_fake()
        .insert_tree(
            path!("/root"),
            json!({
                "a1": "A1",
                "a2": "A2",
                "a3": "A3",
            }),
        )
        .await;

    let project = Project::test(app_state.fs.clone(), [path!("/root").as_ref()], cx).await;
    let (picker, cx) = build_open_path_prompt(project, false, false, cx);

    insert_query(path!("/root/a"), &picker, cx).await;

    let cancel_flag = picker.read_with(cx, |picker, _| picker.delegate.cancel_flag.clone());
    assert!(
        !cancel_flag.load(atomic::Ordering::Acquire),
        "cancel flag should be clear while the prompt is active"
    );

    picker.update_in(cx, |picker, window, cx| {
        picker.delegate.dismissed(window, cx);
    });

    assert!(
        cancel_flag.load(atomic::Ordering::Acquire),
        "dismissing the prompt must cancel the in-flight fuzzy match"
    );
}

/// The in-app new-path prompt opens where its caller asks, e.g. in the folder a
/// patch file is created from, rather than always in the first worktree.
#[gpui::test]
async fn test_new_path_prompt_opens_in_the_requested_directory(cx: &mut TestAppContext) {
    let app_state = init_test(cx);
    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |settings| {
                settings.workspace.use_system_path_prompts = Some(false);
            });
        });
    });
    app_state
        .fs
        .as_fake()
        .insert_tree(path!("/root"), json!({ "dir1": { "a": "A" } }))
        .await;

    let project = Project::test(app_state.fs.clone(), [path!("/root").as_ref()], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    workspace.update_in(cx, |workspace, window, cx| {
        OpenPathPrompt::register_new_path(workspace, Some(window), cx);
    });

    let cases = [
        (
            Some(PathBuf::from(path!("/root/dir1"))),
            path!("/root/dir1/new.patch"),
        ),
        // A directory that already ends in a separator does not get a second one.
        (
            Some(PathBuf::from(path!("/root/dir1/"))),
            path!("/root/dir1/new.patch"),
        ),
        // Without a directory the prompt still opens in the first worktree.
        (None, path!("/root/new.patch")),
    ];
    for (start_directory, expected_query) in cases {
        let _receiver = workspace.update_in(cx, |workspace, window, cx| {
            workspace.prompt_for_new_path_in_directory(
                project::DirectoryLister::Project(project.clone()),
                start_directory.clone(),
                Some("new.patch".to_string()),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        let query = workspace.update_in(cx, |workspace, window, cx| {
            let query = workspace
                .active_modal::<Picker<OpenPathDelegate>>(cx)
                .expect("the in-app prompt should be open")
                .read(cx)
                .query(cx);
            workspace.hide_modal(window, cx);
            query
        });
        assert_eq!(query, expected_query, "start directory {start_directory:?}");
    }
}

fn init_test(cx: &mut TestAppContext) -> Arc<AppState> {
    cx.update(|cx| {
        let state = AppState::test(cx);
        theme_settings::init(theme::LoadThemes::JustBase, cx);

        editor::init(cx);
        state
    })
}

fn build_open_path_prompt(
    project: Entity<Project>,
    creating_path: bool,
    show_hidden: bool,
    cx: &mut TestAppContext,
) -> (Entity<Picker<OpenPathDelegate>>, &mut VisualTestContext) {
    let (picker, _receiver, cx) =
        build_open_path_prompt_with_receiver(project, creating_path, show_hidden, cx);
    (picker, cx)
}

fn build_open_path_prompt_with_receiver(
    project: Entity<Project>,
    creating_path: bool,
    show_hidden: bool,
    cx: &mut TestAppContext,
) -> (
    Entity<Picker<OpenPathDelegate>>,
    oneshot::Receiver<Option<Vec<PathBuf>>>,
    &mut VisualTestContext,
) {
    let (tx, receiver) = futures::channel::oneshot::channel();
    let lister = project::DirectoryLister::Project(project.clone());

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    (
        workspace.update_in(cx, |_, window, cx| {
            let delegate = OpenPathDelegate::new(tx, lister.clone(), creating_path, cx);
            let delegate = if show_hidden {
                delegate.show_hidden()
            } else {
                delegate
            };
            cx.new(|cx| {
                let picker = Picker::uniform_list(delegate, window, cx).embedded();
                let query = lister.default_query(cx);
                picker.set_query(&query, window, cx);
                picker
            })
        }),
        receiver,
        cx,
    )
}

async fn insert_query(
    query: &str,
    picker: &Entity<Picker<OpenPathDelegate>>,
    cx: &mut VisualTestContext,
) {
    picker
        .update_in(cx, |f, window, cx| {
            f.delegate.update_matches(query.to_string(), window, cx)
        })
        .await;
}

fn confirm_completion(
    query: &str,
    select: usize,
    picker: &Entity<Picker<OpenPathDelegate>>,
    cx: &mut VisualTestContext,
) -> Option<String> {
    picker.update_in(cx, |f, window, cx| {
        if f.delegate.selected_index() != select {
            f.delegate.set_selected_index(select, window, cx);
        }
        f.delegate.confirm_completion(query.to_string(), window, cx)
    })
}

fn collect_match_candidates(
    picker: &Entity<Picker<OpenPathDelegate>>,
    cx: &mut VisualTestContext,
) -> Vec<String> {
    picker.update(cx, |f, _| f.delegate.collect_match_candidates())
}
