use fs::FakeFs;
use gpui::TestAppContext;
use project::Project;
use project::terminals::{provided_terminal_environment, register_terminal_environment_provider};
use serde_json::json;
use settings::SettingsStore;
use util::path;

fn init_test(cx: &mut TestAppContext) {
    zlog::init_test();
    cx.update(|cx| {
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
        release_channel::init(semver::Version::new(0, 0, 0), cx);
    });
}

/// ADR-F26: 内蔵 IDE サーバーはこの仕組みで `CLAUDE_CODE_SSE_PORT` を渡す。値が指すのは
/// Zed が動いている PC の loopback なので、リモート越しのターミナルには渡さない。
#[gpui::test]
async fn provides_the_registered_environment_only_to_local_terminals(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/test"), json!({ "main.rs": "" }))
        .await;
    let project = Project::test(fs, [path!("/test").as_ref()], cx).await;

    let observed_projects = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    cx.update(|cx| {
        register_terminal_environment_provider(
            {
                let observed_projects = observed_projects.clone();
                move |project, _cx| {
                    if let Ok(mut observed_projects) = observed_projects.lock() {
                        observed_projects.push(project.entity_id());
                    }
                    vec![(
                        "CLAUDE_CODE_SSE_PORT".to_string(),
                        project.entity_id().as_u64().to_string(),
                    )]
                }
            },
            cx,
        );
    });

    let (local, remote) = cx.update(|cx| {
        (
            provided_terminal_environment(&project, false, cx),
            provided_terminal_environment(&project, true, cx),
        )
    });

    assert_eq!(
        local,
        vec![(
            "CLAUDE_CODE_SSE_PORT".to_string(),
            project.entity_id().as_u64().to_string()
        )]
    );
    assert!(
        remote.is_empty(),
        "a remote terminal must not receive the loopback port, got {remote:?}"
    );
    let observed_projects = observed_projects
        .lock()
        .expect("the provider never panics while holding the lock");
    assert_eq!(
        observed_projects.as_slice(),
        &[project.entity_id()],
        "the provider should be asked once, about the terminal's own project"
    );
}
