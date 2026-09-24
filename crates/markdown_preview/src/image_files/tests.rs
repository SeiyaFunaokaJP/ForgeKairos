use super::*;
use fs::FakeFs;
use gpui::{AppContext as _, TestAppContext};
use serde_json::json;
use util::path;
use util::rel_path::rel_path;

fn init_test(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let settings_store = settings::SettingsStore::test(cx);
        cx.set_global(settings_store);
    });
}

fn bytes(bytes: &[u8], preferred_name: &str) -> (ImageData, Option<String>) {
    (
        ImageData::Bytes {
            bytes: bytes.to_vec(),
            extension: "png".into(),
        },
        Some(preferred_name.to_string()),
    )
}

async fn remote_project(
    server_fs: Arc<FakeFs>,
    local_fs: Arc<FakeFs>,
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) -> (Entity<Project>, Entity<remote_server::HeadlessProject>) {
    cx.update(|cx| release_channel::init(semver::Version::new(0, 0, 0), cx));
    server_cx.update(|cx| release_channel::init(semver::Version::new(0, 0, 0), cx));
    let (options, server_session, connect_guard) = remote::RemoteClient::fake_server(cx, server_cx);
    server_cx.update(remote_server::HeadlessProject::init);
    let server_languages = Arc::new(language::LanguageRegistry::new(server_cx.executor()));
    let headless = server_cx.new(|cx| {
        remote_server::HeadlessProject::new(
            remote_server::HeadlessAppState {
                session: server_session,
                fs: server_fs,
                http_client: Arc::new(http_client::BlockedHttpClient),
                node_runtime: node_runtime::NodeRuntime::unavailable(),
                languages: server_languages,
                extension_host_proxy: Arc::new(extension::ExtensionHostProxy::new()),
                startup_time: std::time::Instant::now(),
            },
            false,
            cx,
        )
    });
    drop(connect_guard);
    let remote_client = remote::RemoteClient::connect_mock(options, cx).await;
    let app_state = cx.update(workspace::AppState::test);
    let project = cx.update(|cx| {
        Project::remote(
            remote_client,
            app_state.client.clone(),
            node_runtime::NodeRuntime::unavailable(),
            app_state.user_store.clone(),
            Arc::new(language::LanguageRegistry::test(
                cx.background_executor().clone(),
            )),
            local_fs,
            false,
            cx,
        )
    });
    project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path!("/project"), true, cx)
        })
        .await
        .unwrap_or_else(|error| panic!("the remote folder should open: {error:#}"));
    cx.run_until_parked();
    (project, headless)
}

/// The image dialog and pasting store a picture next to a document on the
/// host, sent over from this machine; a name the host already has is never
/// written over, even one the worktree does not list.
#[gpui::test]
async fn stores_an_image_next_to_a_remote_document(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let server_fs = FakeFs::new(server_cx.executor());
    server_fs
        .insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                ".gitignore": "docs/images/\n",
                "top.md": "# top",
                "docs": {
                    "readme.md": "# doc",
                    "images": { "shot.png": "taken" },
                },
            }),
        )
        .await;
    let local_fs = FakeFs::new(cx.executor());
    local_fs
        .insert_tree(path!("/local"), json!({ "写真 1.png": "local bytes" }))
        .await;
    // Held to the end: the host side has to outlive the client's project.
    let (project, _headless) =
        remote_project(server_fs.clone(), local_fs.clone(), cx, server_cx).await;
    let worktree_id = project.read_with(cx, |project, cx| {
        project
            .worktrees(cx)
            .next()
            .map(|worktree| worktree.read(cx).id())
            .expect("the remote worktree is open")
    });
    assert!(
        project.read_with(cx, |project, cx| project
            .entry_for_path(
                &ProjectPath {
                    worktree_id,
                    path: rel_path("docs/images/shot.png").into(),
                },
                cx
            )
            .is_none()),
        "the ignored folder is not listed, so only the host knows the name is taken"
    );
    let document = ProjectPath {
        worktree_id,
        path: rel_path("docs/readme.md").into(),
    };

    let (image, name) = bytes(b"pasted", "shot.png");
    let stored = cx
        .update(|cx| store_image_next_to(&project, &document, image, name, cx))
        .await
        .unwrap_or_else(|error| panic!("the picture should be stored on the host: {error:#}"));
    assert_eq!(stored.path.as_ref(), rel_path("docs/images/shot-1.png"));
    let load = |path: &'static str| {
        let server_fs = server_fs.clone();
        async move { server_fs.load(Path::new(path)).await.ok() }
    };
    assert_eq!(
        load(path!("/project/docs/images/shot-1.png"))
            .await
            .as_deref(),
        Some("pasted")
    );
    assert_eq!(
        load(path!("/project/docs/images/shot.png"))
            .await
            .as_deref(),
        Some("taken"),
        "what the host had is not overwritten"
    );
    assert_eq!(
        cx.update(|cx| link_destination(&document, &stored, true, cx))
            .as_deref(),
        Some("images/shot-1.png")
    );

    // A file on this machine is read here and sent over, into an `images`
    // folder the host does not have yet.
    let top = ProjectPath {
        worktree_id,
        path: rel_path("top.md").into(),
    };
    let stored = cx
        .update(|cx| {
            store_image_next_to(
                &project,
                &top,
                ImageData::LocalFile(PathBuf::from(path!("/local/写真 1.png"))),
                None,
                cx,
            )
        })
        .await
        .unwrap_or_else(|error| panic!("the local file should be copied over: {error:#}"));
    assert_eq!(stored.path.as_ref(), rel_path("images/写真 1.png"));
    assert_eq!(
        load(path!("/project/images/写真 1.png")).await.as_deref(),
        Some("local bytes")
    );
    assert_eq!(
        cx.update(|cx| link_destination(&top, &stored, true, cx))
            .as_deref(),
        Some("images/写真%201.png")
    );

    // A link on the host that leads nowhere is never written through. A real
    // host reports the link as there, and its name is passed over; the fake
    // one reports nothing there, so the name looks free and the host refuses
    // the write. Either way the link stays, since what a failed write leaves
    // at the name is not known to be this picture.
    server_fs
        .create_symlink(
            Path::new(path!("/project/images/dangling.png")),
            PathBuf::from(path!("/nowhere/dangling.png")),
        )
        .await
        .expect("the link is made");
    cx.run_until_parked();
    server_cx.run_until_parked();
    let (image, name) = bytes(b"new", "dangling.png");
    let stored = cx
        .update(|cx| store_image_next_to(&project, &top, image, name, cx))
        .await;
    server_cx.run_until_parked();
    cx.run_until_parked();
    if let Ok(stored) = &stored {
        assert_eq!(stored.path.as_ref(), rel_path("images/dangling-1.png"));
    }
    assert!(
        !server_fs
            .is_file(Path::new(path!("/nowhere/dangling.png")))
            .await,
        "nothing is written where the link points"
    );
    assert_eq!(
        server_fs
            .read_link(Path::new(path!("/project/images/dangling.png")))
            .await
            .ok(),
        Some(PathBuf::from(path!("/nowhere/dangling.png"))),
        "the link is left as it was"
    );

    // Given up before it finishes, the store takes away what it wrote.
    let (image, name) = bytes(b"abandoned", "abandoned.png");
    let abandoned = cx.update(|cx| store_image_next_to(&project, &top, image, name, cx));
    drop(abandoned);
    cx.run_until_parked();
    server_cx.run_until_parked();
    cx.run_until_parked();
    assert!(
        !server_fs
            .is_file(Path::new(path!("/project/images/abandoned.png")))
            .await
    );
    assert_eq!(
        load(path!("/project/images/写真 1.png")).await.as_deref(),
        Some("local bytes"),
        "only the abandoned picture is taken away"
    );

    // Taking away a stored picture removes that file and nothing else.
    let (image, name) = bytes(b"refused", "refused.png");
    let refused = cx
        .update(|cx| store_image_next_to(&project, &top, image, name, cx))
        .await
        .unwrap_or_else(|error| panic!("the picture should be stored: {error:#}"));
    cx.run_until_parked();
    cx.update(|cx| remove_stored_image(&project, &top, &refused, cx))
        .await
        .unwrap_or_else(|error| panic!("the picture should be taken away: {error:#}"));
    server_cx.run_until_parked();
    assert!(
        !server_fs
            .is_file(Path::new(path!("/project/images/refused.png")))
            .await
    );
    let images_folder = ProjectPath {
        worktree_id,
        path: rel_path("images").into(),
    };
    let folder_removal = cx
        .update(|cx| remove_stored_image(&project, &top, &images_folder, cx))
        .await;
    assert!(folder_removal.is_err(), "a folder is never taken away");
    assert!(
        server_fs
            .is_file(Path::new(path!("/project/images/写真 1.png")))
            .await
    );

    let (image, name) = bytes(&vec![0; MAXIMUM_IMAGE_BYTES as usize + 1], "large.png");
    let too_large = cx
        .update(|cx| store_image_next_to(&project, &top, image, name, cx))
        .await;
    assert!(too_large.is_err());
    assert!(
        !server_fs
            .is_file(Path::new(path!("/project/images/large.png")))
            .await
    );
}

/// A document opened on its own, with no folder open around it, gets its
/// pictures in the `images` folder beside it all the same.
#[gpui::test]
async fn stores_an_image_beside_a_document_opened_on_its_own(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/notes"),
        json!({
            "memo.md": "# memo",
            "images": { "shot.png": "taken" },
        }),
    )
    .await;
    fs.create_symlink(
        Path::new(path!("/notes/images/dangling.png")),
        PathBuf::from(path!("/nowhere/dangling.png")),
    )
    .await
    .expect("the link is made");
    let project = Project::test(fs.clone(), [path!("/notes/memo.md").as_ref()], cx).await;
    let document = project.read_with(cx, |project, cx| {
        let worktree = project
            .worktrees(cx)
            .next()
            .expect("the document is a worktree of its own");
        ProjectPath {
            worktree_id: worktree.read(cx).id(),
            path: RelPath::empty_arc(),
        }
    });
    assert_eq!(
        cx.update(|cx| storing_refusal(&project, &document, cx)),
        None
    );

    let (image, name) = bytes(b"new", "shot.png");
    let stored = cx
        .update(|cx| store_image_next_to(&project, &document, image, name, cx))
        .await
        .unwrap_or_else(|error| panic!("the picture should be stored: {error:#}"));
    assert_eq!(stored.path.as_ref(), rel_path("images/shot-1.png"));
    assert_eq!(
        cx.update(|cx| link_destination(&document, &stored, true, cx))
            .as_deref(),
        Some("images/shot-1.png")
    );
    assert_eq!(
        fs.load(Path::new(path!("/notes/images/shot-1.png")))
            .await
            .ok()
            .as_deref(),
        Some("new")
    );
    assert_eq!(
        fs.load(Path::new(path!("/notes/images/shot.png")))
            .await
            .ok()
            .as_deref(),
        Some("taken"),
        "what was there is not overwritten"
    );

    let (image, name) = bytes(b"new", "dangling.png");
    let stored = cx
        .update(|cx| store_image_next_to(&project, &document, image, name, cx))
        .await
        .unwrap_or_else(|error| panic!("the picture should be stored: {error:#}"));
    assert_eq!(stored.path.as_ref(), rel_path("images/dangling-1.png"));
    assert!(!fs.is_file(Path::new(path!("/nowhere/dangling.png"))).await);

    cx.update(|cx| remove_stored_image(&project, &document, &stored, cx))
        .await
        .unwrap_or_else(|error| panic!("the picture should be taken away: {error:#}"));
    assert!(
        !fs.is_file(Path::new(path!("/notes/images/dangling-1.png")))
            .await
    );
    assert!(fs.is_file(Path::new(path!("/notes/images/shot.png"))).await);

    let (image, name) = bytes(b"abandoned", "abandoned.png");
    let abandoned = cx.update(|cx| store_image_next_to(&project, &document, image, name, cx));
    drop(abandoned);
    cx.run_until_parked();
    assert!(
        !fs.is_file(Path::new(path!("/notes/images/abandoned.png")))
            .await
    );
}
