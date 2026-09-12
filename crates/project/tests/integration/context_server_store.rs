use anyhow::Result;
use context_server::test::create_fake_transport;
use context_server::{ContextServer, ContextServerId};
use gpui::{
    AppContext, AsyncApp, Entity, SharedString, Subscription, Task, TestAppContext,
    UpdateGlobal as _,
};
use http_client::{FakeHttpClient, Response};
use project::context_server_store::registry::ContextServerDescriptorRegistry;
use project::context_server_store::*;
use project::project_launch_approvals::{
    CHANGED_SINCE_SHOWN_MESSAGE, ExecutionHost, LaunchApprovalKey, LaunchApprovalState,
    LaunchDefinitionFile, LaunchHost, LaunchKind, ProjectLaunchApprovals,
    ProjectLaunchApprovalsEvent, UnboundWorktreeContent,
};
use project::project_settings::ContextServerSettings;
use project::worktree_store::WorktreeStore;
use project::{
    DisableAiSettings, FakeFs, Project, context_server_store::registry::ContextServerDescriptor,
    project_settings::ProjectSettings,
};
use serde_json::json;
use settings::settings_content::SaturatingBool;
use settings::{ContextServerCommand, Settings, SettingsStore};
use std::sync::Arc;
use std::{cell::RefCell, path::PathBuf, rc::Rc};
use util::path;

#[gpui::test]
async fn test_context_server_status(cx: &mut TestAppContext) {
    const SERVER_1_ID: &str = "mcp-1";
    const SERVER_2_ID: &str = "mcp-2";

    let (_fs, project) = setup_context_server_test(cx, json!({"code.rs": ""}), vec![]).await;

    let registry = cx.new(|_| ContextServerDescriptorRegistry::new());
    let store = cx.new(|cx| {
        ContextServerStore::test(
            registry.clone(),
            project.read(cx).worktree_store(),
            Some(project.downgrade()),
            cx,
        )
    });

    let server_1_id = ContextServerId(SERVER_1_ID.into());
    let server_2_id = ContextServerId(SERVER_2_ID.into());

    let server_1 = Arc::new(ContextServer::new(
        server_1_id.clone(),
        Arc::new(create_fake_transport(SERVER_1_ID, cx.executor())),
    ));
    let server_2 = Arc::new(ContextServer::new(
        server_2_id.clone(),
        Arc::new(create_fake_transport(SERVER_2_ID, cx.executor())),
    ));

    store.update(cx, |store, cx| store.test_start_server(server_1, cx));

    cx.run_until_parked();

    cx.update(|cx| {
        assert_eq!(
            store.read(cx).status_for_server(&server_1_id),
            Some(ContextServerStatus::Running)
        );
        assert_eq!(store.read(cx).status_for_server(&server_2_id), None);
    });

    store.update(cx, |store, cx| {
        store.test_start_server(server_2.clone(), cx)
    });

    cx.run_until_parked();

    cx.update(|cx| {
        assert_eq!(
            store.read(cx).status_for_server(&server_1_id),
            Some(ContextServerStatus::Running)
        );
        assert_eq!(
            store.read(cx).status_for_server(&server_2_id),
            Some(ContextServerStatus::Running)
        );
    });

    store
        .update(cx, |store, cx| store.stop_server(&server_2_id, cx))
        .unwrap();

    cx.update(|cx| {
        assert_eq!(
            store.read(cx).status_for_server(&server_1_id),
            Some(ContextServerStatus::Running)
        );
        assert_eq!(
            store.read(cx).status_for_server(&server_2_id),
            Some(ContextServerStatus::Stopped)
        );
    });
}

#[gpui::test]
async fn test_context_server_status_events(cx: &mut TestAppContext) {
    const SERVER_1_ID: &str = "mcp-1";
    const SERVER_2_ID: &str = "mcp-2";

    let (_fs, project) = setup_context_server_test(cx, json!({"code.rs": ""}), vec![]).await;

    let registry = cx.new(|_| ContextServerDescriptorRegistry::new());
    let store = cx.new(|cx| {
        ContextServerStore::test(
            registry.clone(),
            project.read(cx).worktree_store(),
            Some(project.downgrade()),
            cx,
        )
    });

    let server_1_id = ContextServerId(SERVER_1_ID.into());
    let server_2_id = ContextServerId(SERVER_2_ID.into());

    let server_1 = Arc::new(ContextServer::new(
        server_1_id.clone(),
        Arc::new(create_fake_transport(SERVER_1_ID, cx.executor())),
    ));
    let server_2 = Arc::new(ContextServer::new(
        server_2_id.clone(),
        Arc::new(create_fake_transport(SERVER_2_ID, cx.executor())),
    ));

    let _server_events = assert_server_events(
        &store,
        vec![
            (server_1_id.clone(), ContextServerStatus::Starting),
            (server_1_id, ContextServerStatus::Running),
            (server_2_id.clone(), ContextServerStatus::Starting),
            (server_2_id.clone(), ContextServerStatus::Running),
            (server_2_id.clone(), ContextServerStatus::Stopped),
        ],
        cx,
    );

    store.update(cx, |store, cx| store.test_start_server(server_1, cx));

    cx.run_until_parked();

    store.update(cx, |store, cx| {
        store.test_start_server(server_2.clone(), cx)
    });

    cx.run_until_parked();

    store
        .update(cx, |store, cx| store.stop_server(&server_2_id, cx))
        .unwrap();
}

#[gpui::test(iterations = 25)]
async fn test_context_server_concurrent_starts(cx: &mut TestAppContext) {
    const SERVER_1_ID: &str = "mcp-1";

    let (_fs, project) = setup_context_server_test(cx, json!({"code.rs": ""}), vec![]).await;

    let registry = cx.new(|_| ContextServerDescriptorRegistry::new());
    let store = cx.new(|cx| {
        ContextServerStore::test(
            registry.clone(),
            project.read(cx).worktree_store(),
            Some(project.downgrade()),
            cx,
        )
    });

    let server_id = ContextServerId(SERVER_1_ID.into());

    let server_with_same_id_1 = Arc::new(ContextServer::new(
        server_id.clone(),
        Arc::new(create_fake_transport(SERVER_1_ID, cx.executor())),
    ));
    let server_with_same_id_2 = Arc::new(ContextServer::new(
        server_id.clone(),
        Arc::new(create_fake_transport(SERVER_1_ID, cx.executor())),
    ));

    // If we start another server with the same id, we should report that we stopped the previous one
    let _server_events = assert_server_events(
        &store,
        vec![
            (server_id.clone(), ContextServerStatus::Starting),
            (server_id.clone(), ContextServerStatus::Stopped),
            (server_id.clone(), ContextServerStatus::Starting),
            (server_id.clone(), ContextServerStatus::Running),
        ],
        cx,
    );

    store.update(cx, |store, cx| {
        store.test_start_server(server_with_same_id_1.clone(), cx)
    });
    store.update(cx, |store, cx| {
        store.test_start_server(server_with_same_id_2.clone(), cx)
    });

    cx.run_until_parked();

    cx.update(|cx| {
        assert_eq!(
            store.read(cx).status_for_server(&server_id),
            Some(ContextServerStatus::Running)
        );
    });
}

#[gpui::test]
async fn test_context_server_maintain_servers_loop(cx: &mut TestAppContext) {
    const SERVER_1_ID: &str = "mcp-1";
    const SERVER_2_ID: &str = "mcp-2";

    let server_1_id = ContextServerId(SERVER_1_ID.into());
    let server_2_id = ContextServerId(SERVER_2_ID.into());

    let fake_descriptor_1 = Arc::new(FakeContextServerDescriptor::new(SERVER_1_ID));

    let (_fs, project) = setup_context_server_test(cx, json!({"code.rs": ""}), vec![]).await;

    let executor = cx.executor();
    let store = project.read_with(cx, |project, _| project.context_server_store());
    store.update(cx, |store, cx| {
        store.set_context_server_factory(Box::new(move |id, _| {
            Arc::new(ContextServer::new(
                id.clone(),
                Arc::new(create_fake_transport(id.0.to_string(), executor.clone())),
            ))
        }));
        store.registry().update(cx, |registry, cx| {
            registry.register_context_server_descriptor(SERVER_1_ID.into(), fake_descriptor_1, cx);
        });
    });

    set_context_server_configuration(
        vec![(
            server_1_id.0.clone(),
            settings::ContextServerSettingsContent::Extension {
                enabled: true,
                remote: false,
                settings: json!({
                    "somevalue": true
                }),
            },
        )],
        cx,
    );

    // Ensure that mcp-1 starts up
    {
        let _server_events = assert_server_events(
            &store,
            vec![
                (server_1_id.clone(), ContextServerStatus::Starting),
                (server_1_id.clone(), ContextServerStatus::Running),
            ],
            cx,
        );
        cx.run_until_parked();
    }

    // Ensure that mcp-1 is restarted when the configuration was changed
    {
        let _server_events = assert_server_events(
            &store,
            vec![
                (server_1_id.clone(), ContextServerStatus::Stopped),
                (server_1_id.clone(), ContextServerStatus::Starting),
                (server_1_id.clone(), ContextServerStatus::Running),
            ],
            cx,
        );
        set_context_server_configuration(
            vec![(
                server_1_id.0.clone(),
                settings::ContextServerSettingsContent::Extension {
                    enabled: true,
                    remote: false,
                    settings: json!({
                        "somevalue": false
                    }),
                },
            )],
            cx,
        );

        cx.run_until_parked();
    }

    // Ensure that mcp-1 is not restarted when the configuration was not changed
    {
        let _server_events = assert_server_events(&store, vec![], cx);
        set_context_server_configuration(
            vec![(
                server_1_id.0.clone(),
                settings::ContextServerSettingsContent::Extension {
                    enabled: true,
                    remote: false,
                    settings: json!({
                        "somevalue": false
                    }),
                },
            )],
            cx,
        );

        cx.run_until_parked();
    }

    // Ensure that mcp-2 is started once it is added to the settings
    {
        let _server_events = assert_server_events(
            &store,
            vec![
                (server_2_id.clone(), ContextServerStatus::Starting),
                (server_2_id.clone(), ContextServerStatus::Running),
            ],
            cx,
        );
        set_context_server_configuration(
            vec![
                (
                    server_1_id.0.clone(),
                    settings::ContextServerSettingsContent::Extension {
                        enabled: true,
                        remote: false,
                        settings: json!({
                            "somevalue": false
                        }),
                    },
                ),
                (
                    server_2_id.0.clone(),
                    settings::ContextServerSettingsContent::Stdio {
                        enabled: true,
                        remote: false,
                        command: ContextServerCommand {
                            path: "somebinary".into(),
                            args: vec!["arg".to_string()],
                            env: None,
                            timeout: None,
                        },
                    },
                ),
            ],
            cx,
        );

        cx.run_until_parked();
    }

    // Ensure that mcp-2 is restarted once the args have changed
    {
        let _server_events = assert_server_events(
            &store,
            vec![
                (server_2_id.clone(), ContextServerStatus::Stopped),
                (server_2_id.clone(), ContextServerStatus::Starting),
                (server_2_id.clone(), ContextServerStatus::Running),
            ],
            cx,
        );
        set_context_server_configuration(
            vec![
                (
                    server_1_id.0.clone(),
                    settings::ContextServerSettingsContent::Extension {
                        enabled: true,
                        remote: false,
                        settings: json!({
                            "somevalue": false
                        }),
                    },
                ),
                (
                    server_2_id.0.clone(),
                    settings::ContextServerSettingsContent::Stdio {
                        enabled: true,
                        remote: false,
                        command: ContextServerCommand {
                            path: "somebinary".into(),
                            args: vec!["anotherArg".to_string()],
                            env: None,
                            timeout: None,
                        },
                    },
                ),
            ],
            cx,
        );

        cx.run_until_parked();
    }

    // Ensure that mcp-2 is removed once it is removed from the settings
    {
        let _server_events = assert_server_events(
            &store,
            vec![(server_2_id.clone(), ContextServerStatus::Stopped)],
            cx,
        );
        set_context_server_configuration(
            vec![(
                server_1_id.0.clone(),
                settings::ContextServerSettingsContent::Extension {
                    enabled: true,
                    remote: false,
                    settings: json!({
                        "somevalue": false
                    }),
                },
            )],
            cx,
        );

        cx.run_until_parked();

        cx.update(|cx| {
            assert_eq!(store.read(cx).status_for_server(&server_2_id), None);
        });
    }

    // Ensure that nothing happens if the settings do not change
    {
        let _server_events = assert_server_events(&store, vec![], cx);
        set_context_server_configuration(
            vec![(
                server_1_id.0.clone(),
                settings::ContextServerSettingsContent::Extension {
                    enabled: true,
                    remote: false,
                    settings: json!({
                        "somevalue": false
                    }),
                },
            )],
            cx,
        );

        cx.run_until_parked();

        cx.update(|cx| {
            assert_eq!(
                store.read(cx).status_for_server(&server_1_id),
                Some(ContextServerStatus::Running)
            );
            assert_eq!(store.read(cx).status_for_server(&server_2_id), None);
        });
    }
}

#[gpui::test]
async fn test_context_server_enabled_disabled(cx: &mut TestAppContext) {
    const SERVER_1_ID: &str = "mcp-1";

    let server_1_id = ContextServerId(SERVER_1_ID.into());

    let (_fs, project) = setup_context_server_test(cx, json!({"code.rs": ""}), vec![]).await;

    let executor = cx.executor();
    let store = project.read_with(cx, |project, _| project.context_server_store());
    store.update(cx, |store, _| {
        store.set_context_server_factory(Box::new(move |id, _| {
            Arc::new(ContextServer::new(
                id.clone(),
                Arc::new(create_fake_transport(id.0.to_string(), executor.clone())),
            ))
        }));
    });

    set_context_server_configuration(
        vec![(
            server_1_id.0.clone(),
            settings::ContextServerSettingsContent::Stdio {
                enabled: true,
                remote: false,
                command: ContextServerCommand {
                    path: "somebinary".into(),
                    args: vec!["arg".to_string()],
                    env: None,
                    timeout: None,
                },
            },
        )],
        cx,
    );

    // Ensure that mcp-1 starts up
    {
        let _server_events = assert_server_events(
            &store,
            vec![
                (server_1_id.clone(), ContextServerStatus::Starting),
                (server_1_id.clone(), ContextServerStatus::Running),
            ],
            cx,
        );
        cx.run_until_parked();
    }

    // Ensure that mcp-1 is stopped once it is disabled.
    {
        let _server_events = assert_server_events(
            &store,
            vec![(server_1_id.clone(), ContextServerStatus::Stopped)],
            cx,
        );
        set_context_server_configuration(
            vec![(
                server_1_id.0.clone(),
                settings::ContextServerSettingsContent::Stdio {
                    enabled: false,
                    remote: false,
                    command: ContextServerCommand {
                        path: "somebinary".into(),
                        args: vec!["arg".to_string()],
                        env: None,
                        timeout: None,
                    },
                },
            )],
            cx,
        );

        cx.run_until_parked();
    }

    // Ensure that mcp-1 is started once it is enabled again.
    {
        let _server_events = assert_server_events(
            &store,
            vec![
                (server_1_id.clone(), ContextServerStatus::Starting),
                (server_1_id.clone(), ContextServerStatus::Running),
            ],
            cx,
        );
        set_context_server_configuration(
            vec![(
                server_1_id.0.clone(),
                settings::ContextServerSettingsContent::Stdio {
                    enabled: true,
                    remote: false,
                    command: ContextServerCommand {
                        path: "somebinary".into(),
                        args: vec!["arg".to_string()],
                        timeout: None,
                        env: None,
                    },
                },
            )],
            cx,
        );

        cx.run_until_parked();
    }
}

#[gpui::test]
async fn test_context_server_respects_disable_ai(cx: &mut TestAppContext) {
    const SERVER_1_ID: &str = "mcp-1";

    let server_1_id = ContextServerId(SERVER_1_ID.into());

    // Set up SettingsStore with disable_ai: true in user settings BEFORE creating project
    cx.update(|cx| {
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
        DisableAiSettings::register(cx);
        // Set disable_ai via user settings (not override_global) so it persists through recompute_values
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |content| {
                content.project.disable_ai = Some(SaturatingBool(true));
            });
        });
    });

    // Now create the project (ContextServerStore will see disable_ai = true)
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/test"), json!({"code.rs": ""})).await;
    let project = Project::test(fs.clone(), [path!("/test").as_ref()], cx).await;

    let executor = cx.executor();
    let store = project.read_with(cx, |project, _| project.context_server_store());
    store.update(cx, |store, _| {
        store.set_context_server_factory(Box::new(move |id, _| {
            Arc::new(ContextServer::new(
                id.clone(),
                Arc::new(create_fake_transport(id.0.to_string(), executor.clone())),
            ))
        }));
    });

    set_context_server_configuration(
        vec![(
            server_1_id.0.clone(),
            settings::ContextServerSettingsContent::Stdio {
                enabled: true,
                remote: false,
                command: ContextServerCommand {
                    path: "somebinary".into(),
                    args: vec!["arg".to_string()],
                    env: None,
                    timeout: None,
                },
            },
        )],
        cx,
    );

    cx.run_until_parked();

    // Verify that no server started because AI is disabled
    cx.update(|cx| {
        assert_eq!(
            store.read(cx).status_for_server(&server_1_id),
            None,
            "Server should not start when disable_ai is true"
        );
    });

    // Enable AI and verify server starts
    {
        let _server_events = assert_server_events(
            &store,
            vec![
                (server_1_id.clone(), ContextServerStatus::Starting),
                (server_1_id.clone(), ContextServerStatus::Running),
            ],
            cx,
        );
        cx.update(|cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |content| {
                    content.project.disable_ai = Some(SaturatingBool(false));
                });
            });
        });
        cx.run_until_parked();
    }

    // Disable AI again and verify server stops
    {
        let _server_events = assert_server_events(
            &store,
            vec![(server_1_id.clone(), ContextServerStatus::Stopped)],
            cx,
        );
        cx.update(|cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |content| {
                    content.project.disable_ai = Some(SaturatingBool(true));
                });
            });
        });
        cx.run_until_parked();
    }

    // Verify server is stopped
    cx.update(|cx| {
        assert_eq!(
            store.read(cx).status_for_server(&server_1_id),
            Some(ContextServerStatus::Stopped),
            "Server should be stopped when disable_ai is true"
        );
    });
}

#[gpui::test]
async fn test_context_server_loaded_when_first_worktree_added(cx: &mut TestAppContext) {
    const SERVER_ID: &str = "mcp-1";
    let server_id = ContextServerId(SERVER_ID.into());

    cx.update(|cx| {
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
    });
    set_context_server_configuration(
        vec![(
            server_id.0.clone(),
            settings::ContextServerSettingsContent::Stdio {
                enabled: true,
                remote: false,
                command: ContextServerCommand {
                    path: "somebinary".into(),
                    args: vec!["arg".to_string()],
                    env: None,
                    timeout: None,
                },
            },
        )],
        cx,
    );

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/test"), json!({"code.rs": ""})).await;
    let project = Project::test(fs, std::iter::empty::<&std::path::Path>(), cx).await;
    let store = project.read_with(cx, |project, _| project.context_server_store());
    let executor = cx.executor();
    store.update(cx, |store, _| {
        store.set_context_server_factory(Box::new(move |id, _| {
            Arc::new(ContextServer::new(
                id.clone(),
                Arc::new(create_fake_transport(id.0.to_string(), executor.clone())),
            ))
        }));
    });

    project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path!("/test"), true, cx)
        })
        .await
        .expect("failed to add worktree");
    cx.run_until_parked();

    cx.update(|cx| {
        let store = store.read(cx);
        assert_eq!(
            (
                store.server_ids().to_vec(),
                store.status_for_server(&server_id),
            ),
            (vec![server_id], Some(ContextServerStatus::Running))
        );
    });
}

// A server from the user's settings starts in the neutral directory: what an
// interpreter or package runner loads from its working directory is part of
// no approval, so that must not be a directory an agent can write (ADR-F42).
// Adding a worktree therefore does not restart it either.
#[gpui::test]
async fn test_user_stdio_server_runs_in_the_neutral_directory(cx: &mut TestAppContext) {
    const SERVER_ID: &str = "mcp-1";
    let server_id = ContextServerId(SERVER_ID.into());

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/"),
        json!({"lonely.rs": "", "project": {"code.rs": ""}}),
    )
    .await;

    cx.update(|cx| {
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
    });

    let project = Project::test(fs.clone(), [path!("/lonely.rs").as_ref()], cx).await;

    let executor = cx.executor();
    let store = project.read_with(cx, |project, _| project.context_server_store());
    store.update(cx, |store, _| {
        store.set_context_server_factory(Box::new(move |id, _| {
            Arc::new(ContextServer::new(
                id.clone(),
                Arc::new(create_fake_transport(id.0.to_string(), executor.clone())),
            ))
        }));
    });

    // Configure the server globally so it starts against the file worktree.
    {
        let _server_events = assert_server_events(
            &store,
            vec![
                (server_id.clone(), ContextServerStatus::Starting),
                (server_id.clone(), ContextServerStatus::Running),
            ],
            cx,
        );
        set_context_server_configuration(
            vec![(
                server_id.0.clone(),
                settings::ContextServerSettingsContent::Stdio {
                    enabled: true,
                    remote: false,
                    command: ContextServerCommand {
                        path: "somebinary".into(),
                        args: vec!["arg".to_string()],
                        env: None,
                        timeout: None,
                    },
                },
            )],
            cx,
        );
        cx.run_until_parked();
    }

    {
        let _server_events = assert_server_events(&store, vec![], cx);
        project
            .update(cx, |project, cx| {
                project.find_or_create_worktree(path!("/project"), true, cx)
            })
            .await
            .expect("Failed to add worktree");
        cx.run_until_parked();
    }

    cx.update(|cx| {
        let store = store.read(cx);
        assert_eq!(
            store.status_for_server(&server_id),
            Some(ContextServerStatus::Running)
        );
        assert_eq!(
            store.working_directory_for_test(&server_id),
            Some(Some(neutral_working_directory())),
            "a server from the user's settings starts in the neutral directory"
        );
        assert_eq!(
            &*neutral_working_directory(),
            paths::data_dir().join("mcp-neutral-cwd").as_path()
        );
    });
}

#[gpui::test]
async fn test_server_ids_includes_disabled_servers(cx: &mut TestAppContext) {
    const ENABLED_SERVER_ID: &str = "enabled-server";
    const DISABLED_SERVER_ID: &str = "disabled-server";

    let enabled_server_id = ContextServerId(ENABLED_SERVER_ID.into());
    let disabled_server_id = ContextServerId(DISABLED_SERVER_ID.into());

    let (_fs, project) = setup_context_server_test(cx, json!({"code.rs": ""}), vec![]).await;

    let executor = cx.executor();
    let store = project.read_with(cx, |project, _| project.context_server_store());
    store.update(cx, |store, _| {
        store.set_context_server_factory(Box::new(move |id, _| {
            Arc::new(ContextServer::new(
                id.clone(),
                Arc::new(create_fake_transport(id.0.to_string(), executor.clone())),
            ))
        }));
    });

    // Configure one enabled and one disabled server
    set_context_server_configuration(
        vec![
            (
                enabled_server_id.0.clone(),
                settings::ContextServerSettingsContent::Stdio {
                    enabled: true,
                    remote: false,
                    command: ContextServerCommand {
                        path: "somebinary".into(),
                        args: vec![],
                        env: None,
                        timeout: None,
                    },
                },
            ),
            (
                disabled_server_id.0.clone(),
                settings::ContextServerSettingsContent::Stdio {
                    enabled: false,
                    remote: false,
                    command: ContextServerCommand {
                        path: "somebinary".into(),
                        args: vec![],
                        env: None,
                        timeout: None,
                    },
                },
            ),
        ],
        cx,
    );

    cx.run_until_parked();

    // Verify that server_ids includes both enabled and disabled servers
    cx.update(|cx| {
        let server_ids = store.read(cx).server_ids().to_vec();
        assert!(
            server_ids.contains(&enabled_server_id),
            "server_ids should include enabled server"
        );
        assert!(
            server_ids.contains(&disabled_server_id),
            "server_ids should include disabled server"
        );
    });

    // Verify that the enabled server is running and the disabled server is not
    cx.read(|cx| {
        assert_eq!(
            store.read(cx).status_for_server(&enabled_server_id),
            Some(ContextServerStatus::Running),
            "enabled server should be running"
        );
        // Disabled server should not be in the servers map (status returns None)
        // but should still be in server_ids
        assert_eq!(
            store.read(cx).status_for_server(&disabled_server_id),
            None,
            "disabled server should not have a status (not in servers map)"
        );
    });
}

fn set_context_server_configuration(
    context_servers: Vec<(Arc<str>, settings::ContextServerSettingsContent)>,
    cx: &mut TestAppContext,
) {
    cx.update(|cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |content| {
                content.project.context_servers.clear();
                for (id, config) in context_servers {
                    content.project.context_servers.insert(id, config);
                }
            });
        })
    });
}

#[gpui::test]
async fn test_remote_context_server(cx: &mut TestAppContext) {
    const SERVER_ID: &str = "remote-server";
    let server_id = ContextServerId(SERVER_ID.into());
    let server_url = "http://example.com/api";

    let client = FakeHttpClient::create(|_| async move {
        use http_client::AsyncBody;

        let response = Response::builder()
            .status(200)
            .header("Content-Type", "application/json")
            .body(AsyncBody::from(
                serde_json::to_string(&json!({
                    "jsonrpc": "2.0",
                    "id": 0,
                    "result": {
                        "protocolVersion": "2024-11-05",
                        "capabilities": {},
                        "serverInfo": {
                            "name": "test-server",
                            "version": "1.0.0"
                        }
                    }
                }))
                .unwrap(),
            ))
            .unwrap();
        Ok(response)
    });
    cx.update(|cx| cx.set_http_client(client));

    let (_fs, project) = setup_context_server_test(cx, json!({ "code.rs": "" }), vec![]).await;

    let store = project.read_with(cx, |project, _| project.context_server_store());

    set_context_server_configuration(
        vec![(
            server_id.0.clone(),
            settings::ContextServerSettingsContent::Http {
                enabled: true,
                url: server_url.to_string(),
                headers: Default::default(),
                timeout: None,
                oauth: None,
            },
        )],
        cx,
    );

    let _server_events = assert_server_events(
        &store,
        vec![
            (server_id.clone(), ContextServerStatus::Starting),
            (server_id.clone(), ContextServerStatus::Running),
        ],
        cx,
    );
    cx.run_until_parked();
}

// A server may accept `initialize` unauthenticated (returns 200) yet only send
// `WWW-Authenticate` on a later request such as `tools/list` / `tools/call`.
// That post-initialize 401 must initiate the OAuth flow instead of surfacing as
// an opaque request failure while the server stays "Running".
#[gpui::test]
async fn test_http_server_authenticates_on_post_init_401(cx: &mut TestAppContext) {
    use context_server::transport::TransportError;

    const SERVER_ID: &str = "auth-server";
    let server_id = ContextServerId(SERVER_ID.into());

    set_fake_mcp_http_client(cx, |message| {
        if message.contains("\"method\":\"initialize\"") {
            Ok(initialize_response())
        } else if message.contains("notifications/initialized") {
            Ok(notification_accepted_response())
        } else {
            Ok(unauthorized_response())
        }
    });

    let (_fs, project) = setup_context_server_test(cx, json!({ "code.rs": "" }), vec![]).await;
    let store = project.read_with(cx, |project, _| project.context_server_store());

    set_http_context_server_configuration(&server_id, cx);

    {
        let _server_events = assert_server_events(
            &store,
            vec![
                (server_id.clone(), ContextServerStatus::Starting),
                (server_id.clone(), ContextServerStatus::Running),
            ],
            cx,
        );
        cx.run_until_parked();
    }

    let client = store.read_with(cx, |store, _| {
        store
            .get_running_server(&server_id)
            .expect("server should be running")
            .client()
            .expect("running server should have a client")
    });

    {
        let _server_events = assert_server_events(
            &store,
            vec![
                (server_id.clone(), ContextServerStatus::Starting),
                (server_id.clone(), ContextServerStatus::AuthRequired),
            ],
            cx,
        );

        // The 401 tears down the client, and the request that carried it fails
        // with the typed error.
        let error = client
            .request::<context_server::types::requests::ListTools>(())
            .await
            .expect_err("request challenged with a 401 should fail");
        assert!(
            matches!(
                error.downcast_ref::<TransportError>(),
                Some(TransportError::AuthRequired { .. })
            ),
            "expected an AuthRequired error, got: {error}"
        );

        cx.run_until_parked();
    }

    cx.update(|cx| {
        assert_eq!(
            store.read(cx).status_for_server(&server_id),
            Some(ContextServerStatus::AuthRequired),
            "server should require authentication after a post-initialize 401"
        );
    });
}

// The first 401 may even arrive on a notification (`notifications/initialized`
// here): the send fails with no request in flight to carry a typed error back,
// and the client is dead by the time anything notices. Watching the transport
// shutdown must still move the server into `AuthRequired`.
#[gpui::test]
async fn test_http_server_authenticates_on_notification_401(cx: &mut TestAppContext) {
    const SERVER_ID: &str = "auth-server";
    let server_id = ContextServerId(SERVER_ID.into());

    set_fake_mcp_http_client(cx, |message| {
        if message.contains("\"method\":\"initialize\"") {
            Ok(initialize_response())
        } else {
            Ok(unauthorized_response())
        }
    });

    let (_fs, project) = setup_context_server_test(cx, json!({ "code.rs": "" }), vec![]).await;
    let store = project.read_with(cx, |project, _| project.context_server_store());

    set_http_context_server_configuration(&server_id, cx);

    {
        let _server_events = assert_server_events(
            &store,
            vec![
                (server_id.clone(), ContextServerStatus::Starting),
                (server_id.clone(), ContextServerStatus::Running),
                (server_id.clone(), ContextServerStatus::Starting),
                (server_id.clone(), ContextServerStatus::AuthRequired),
            ],
            cx,
        );
        cx.run_until_parked();
    }

    cx.update(|cx| {
        assert_eq!(
            store.read(cx).status_for_server(&server_id),
            Some(ContextServerStatus::AuthRequired),
            "server should require authentication after a 401 on a notification"
        );
    });
}

// A transport failure that is not an authentication challenge must not touch
// the server's state: no spurious auth flow, and (as before the transport
// watch existed) the server stays `Running`.
#[gpui::test]
async fn test_http_server_ignores_non_auth_transport_failure(cx: &mut TestAppContext) {
    const SERVER_ID: &str = "flaky-server";
    let server_id = ContextServerId(SERVER_ID.into());

    set_fake_mcp_http_client(cx, |message| {
        if message.contains("\"method\":\"initialize\"") {
            Ok(initialize_response())
        } else if message.contains("notifications/initialized") {
            Ok(notification_accepted_response())
        } else {
            Err(anyhow::anyhow!("connection reset"))
        }
    });

    let (_fs, project) = setup_context_server_test(cx, json!({ "code.rs": "" }), vec![]).await;
    let store = project.read_with(cx, |project, _| project.context_server_store());

    set_http_context_server_configuration(&server_id, cx);

    {
        let _server_events = assert_server_events(
            &store,
            vec![
                (server_id.clone(), ContextServerStatus::Starting),
                (server_id.clone(), ContextServerStatus::Running),
            ],
            cx,
        );
        cx.run_until_parked();

        let client = store.read_with(cx, |store, _| {
            store
                .get_running_server(&server_id)
                .expect("server should be running")
                .client()
                .expect("running server should have a client")
        });

        client
            .request::<context_server::types::requests::ListTools>(())
            .await
            .expect_err("request should fail when the transport errors");

        cx.run_until_parked();
        // Dropping the events guard asserts no further status change happened.
    }

    cx.update(|cx| {
        assert_eq!(
            store.read(cx).status_for_server(&server_id),
            Some(ContextServerStatus::Running),
            "a non-auth transport failure should not change the server state"
        );
    });
}

// A server may also require authentication on `initialize` itself. The
// challenge is read from the transport slot rather than the returned error, so
// the 401 is recognized even if another error (e.g. the request timeout) wins
// the race to become the reported startup failure.
#[gpui::test]
async fn test_http_server_authenticates_on_initialize_401(cx: &mut TestAppContext) {
    const SERVER_ID: &str = "auth-server";
    let server_id = ContextServerId(SERVER_ID.into());

    set_fake_mcp_http_client(cx, |_message| Ok(unauthorized_response()));

    let (_fs, project) = setup_context_server_test(cx, json!({ "code.rs": "" }), vec![]).await;
    let store = project.read_with(cx, |project, _| project.context_server_store());

    set_http_context_server_configuration(&server_id, cx);

    {
        let _server_events = assert_server_events(
            &store,
            vec![
                (server_id.clone(), ContextServerStatus::Starting),
                (server_id.clone(), ContextServerStatus::AuthRequired),
            ],
            cx,
        );
        cx.run_until_parked();
    }

    cx.update(|cx| {
        assert_eq!(
            store.read(cx).status_for_server(&server_id),
            Some(ContextServerStatus::AuthRequired),
            "server should require authentication after a 401 on initialize"
        );
    });
}

// Restarting a server reuses its transport (e.g. via the MCP settings page),
// so a challenge recorded by a previous client generation must not leak into
// the next one: after a successful restart, a non-auth transport failure must
// not trip a spurious auth flow on the stale challenge.
#[gpui::test]
async fn test_http_server_restart_clears_stale_auth_challenge(cx: &mut TestAppContext) {
    use std::sync::atomic::{AtomicBool, Ordering};

    const SERVER_ID: &str = "auth-server";
    let server_id = ContextServerId(SERVER_ID.into());

    let restarted = Arc::new(AtomicBool::new(false));
    set_fake_mcp_http_client(cx, {
        let restarted = restarted.clone();
        move |message| {
            if message.contains("\"method\":\"initialize\"") {
                Ok(initialize_response())
            } else if message.contains("notifications/initialized") {
                Ok(notification_accepted_response())
            } else if restarted.load(Ordering::SeqCst) {
                Err(anyhow::anyhow!("connection reset"))
            } else {
                Ok(unauthorized_response())
            }
        }
    });

    let (_fs, project) = setup_context_server_test(cx, json!({ "code.rs": "" }), vec![]).await;
    let store = project.read_with(cx, |project, _| project.context_server_store());

    set_http_context_server_configuration(&server_id, cx);
    cx.run_until_parked();

    // A post-initialize 401 records a challenge on the transport and moves the
    // server into AuthRequired.
    let client = store.read_with(cx, |store, _| {
        store
            .get_running_server(&server_id)
            .expect("server should be running")
            .client()
            .expect("running server should have a client")
    });
    client
        .request::<context_server::types::requests::ListTools>(())
        .await
        .expect_err("request challenged with a 401 should fail");
    // Drop our handle so the dead client fully goes away, as it does in
    // production once the store has stopped it: a lingering client would
    // compete with its successor for the reused transport's response channel.
    drop(client);
    cx.run_until_parked();
    cx.update(|cx| {
        assert_eq!(
            store.read(cx).status_for_server(&server_id),
            Some(ContextServerStatus::AuthRequired),
        );
    });

    // The user restarts the same server instance instead of authenticating,
    // and the server no longer challenges.
    restarted.store(true, Ordering::SeqCst);
    store.update(cx, |store, cx| {
        let server = store.get_server(&server_id).expect("server should exist");
        store.start_server(server, cx);
    });
    cx.run_until_parked();
    cx.update(|cx| {
        assert_eq!(
            store.read(cx).status_for_server(&server_id),
            Some(ContextServerStatus::Running),
        );
    });

    let client = store.read_with(cx, |store, _| {
        store
            .get_running_server(&server_id)
            .expect("server should be running")
            .client()
            .expect("running server should have a client")
    });
    client
        .request::<context_server::types::requests::ListTools>(())
        .await
        .expect_err("request should fail when the transport errors");
    cx.run_until_parked();

    cx.update(|cx| {
        assert_eq!(
            store.read(cx).status_for_server(&server_id),
            Some(ContextServerStatus::Running),
            "a stale challenge from a previous client generation must not trigger auth"
        );
    });
}

fn set_http_context_server_configuration(server_id: &ContextServerId, cx: &mut TestAppContext) {
    set_context_server_configuration(
        vec![(
            server_id.0.clone(),
            settings::ContextServerSettingsContent::Http {
                enabled: true,
                url: "https://mcp.example.com/mcp".to_string(),
                headers: Default::default(),
                timeout: None,
                oauth: None,
            },
        )],
        cx,
    );
}

/// A fake HTTP client that serves the OAuth discovery documents (CIMD-capable,
/// so no dynamic client registration is needed) and routes MCP endpoint POSTs
/// to `respond_to_mcp_message` by the JSON-RPC message in the request body.
fn set_fake_mcp_http_client(
    cx: &mut TestAppContext,
    respond_to_mcp_message: impl Fn(&str) -> Result<Response<http_client::AsyncBody>>
    + Send
    + Sync
    + 'static,
) {
    let respond_to_mcp_message = Arc::new(respond_to_mcp_message);
    let client = FakeHttpClient::create(move |request| {
        let respond_to_mcp_message = respond_to_mcp_message.clone();
        async move {
            let uri = request.uri().to_string();
            let discovery_document = if uri.contains("oauth-protected-resource") {
                Some(json!({
                    "resource": "https://mcp.example.com",
                    "authorization_servers": ["https://auth.example.com"],
                    "scopes_supported": ["mcp:read"]
                }))
            } else if uri.contains("oauth-authorization-server") {
                Some(json!({
                    "issuer": "https://auth.example.com",
                    "authorization_endpoint": "https://auth.example.com/authorize",
                    "token_endpoint": "https://auth.example.com/token",
                    "code_challenge_methods_supported": ["S256"],
                    "client_id_metadata_document_supported": true
                }))
            } else {
                None
            };
            if let Some(document) = discovery_document {
                return Ok(json_response(document));
            }

            let mut body = request.into_body();
            let mut message = String::new();
            futures::AsyncReadExt::read_to_string(&mut body, &mut message).await?;
            respond_to_mcp_message(&message)
        }
    });
    cx.update(|cx| cx.set_http_client(client));
}

fn json_response(body: serde_json::Value) -> Response<http_client::AsyncBody> {
    Response::builder()
        .status(200)
        .header("Content-Type", "application/json")
        .body(http_client::AsyncBody::from(body.to_string()))
        .unwrap()
}

fn initialize_response() -> Response<http_client::AsyncBody> {
    json_response(json!({
        "jsonrpc": "2.0",
        "id": 0,
        "result": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "serverInfo": { "name": "test-server", "version": "1.0.0" }
        }
    }))
}

fn notification_accepted_response() -> Response<http_client::AsyncBody> {
    Response::builder()
        .status(202)
        .body(http_client::AsyncBody::empty())
        .unwrap()
}

fn unauthorized_response() -> Response<http_client::AsyncBody> {
    Response::builder()
        .status(401)
        .header("WWW-Authenticate", "Bearer")
        .body(http_client::AsyncBody::empty())
        .unwrap()
}

struct ServerEvents {
    received_event_count: Rc<RefCell<usize>>,
    expected_event_count: usize,
    _subscription: Subscription,
}

impl Drop for ServerEvents {
    fn drop(&mut self) {
        let actual_event_count = *self.received_event_count.borrow();
        assert_eq!(
            actual_event_count, self.expected_event_count,
            "
               Expected to receive {} context server store events, but received {} events",
            self.expected_event_count, actual_event_count
        );
    }
}

#[gpui::test]
async fn test_context_server_global_timeout(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
        SettingsStore::update_global(cx, |store, cx| {
            store
                .set_user_settings(r#"{"context_server_timeout": 90}"#, cx)
                .expect("Failed to set test user settings");
        });
    });

    let (_fs, project) = setup_context_server_test(cx, json!({"code.rs": ""}), vec![]).await;

    let registry = cx.new(|_| ContextServerDescriptorRegistry::new());
    let store = cx.new(|cx| {
        ContextServerStore::test(
            registry.clone(),
            project.read(cx).worktree_store(),
            Some(project.downgrade()),
            cx,
        )
    });

    let mut async_cx = cx.to_async();
    let result = ContextServerStore::create_context_server(
        store.downgrade(),
        ContextServerId("test-server".into()),
        Arc::new(ContextServerConfiguration::Http {
            url: url::Url::parse("http://localhost:8080").expect("Failed to parse test URL"),
            headers: Default::default(),
            timeout: None,
            oauth: None,
        }),
        &mut async_cx,
    )
    .await;

    assert!(
        result.is_ok(),
        "Server should be created successfully with global timeout"
    );
}

#[gpui::test]
async fn test_context_server_per_server_timeout_override(cx: &mut TestAppContext) {
    const SERVER_ID: &str = "test-server";

    cx.update(|cx| {
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
        SettingsStore::update_global(cx, |store, cx| {
            store
                .set_user_settings(r#"{"context_server_timeout": 60}"#, cx)
                .expect("Failed to set test user settings");
        });
    });

    let (_fs, project) = setup_context_server_test(
        cx,
        json!({"code.rs": ""}),
        vec![(
            SERVER_ID.into(),
            ContextServerSettings::Http {
                enabled: true,
                url: "http://localhost:8080".to_string(),
                headers: Default::default(),
                timeout: Some(120),
                oauth: None,
            },
        )],
    )
    .await;

    let registry = cx.new(|_| ContextServerDescriptorRegistry::new());
    let store = cx.new(|cx| {
        ContextServerStore::test(
            registry.clone(),
            project.read(cx).worktree_store(),
            Some(project.downgrade()),
            cx,
        )
    });

    let mut async_cx = cx.to_async();
    let result = ContextServerStore::create_context_server(
        store.downgrade(),
        ContextServerId("test-server".into()),
        Arc::new(ContextServerConfiguration::Http {
            url: url::Url::parse("http://localhost:8080").expect("Failed to parse test URL"),
            headers: Default::default(),
            timeout: Some(120),
            oauth: None,
        }),
        &mut async_cx,
    )
    .await;

    assert!(
        result.is_ok(),
        "Server should be created successfully with per-server timeout override"
    );
}

#[gpui::test]
async fn test_context_server_stdio_timeout(cx: &mut TestAppContext) {
    let (_fs, project) = setup_context_server_test(cx, json!({"code.rs": ""}), vec![]).await;

    let registry = cx.new(|_| ContextServerDescriptorRegistry::new());
    let store = cx.new(|cx| {
        ContextServerStore::test(
            registry.clone(),
            project.read(cx).worktree_store(),
            Some(project.downgrade()),
            cx,
        )
    });

    let mut async_cx = cx.to_async();
    let result = ContextServerStore::create_context_server(
        store.downgrade(),
        ContextServerId("stdio-server".into()),
        Arc::new(ContextServerConfiguration::Custom {
            command: ContextServerCommand {
                path: "/usr/bin/node".into(),
                args: vec!["server.js".into()],
                env: None,
                timeout: Some(180000),
            },
            remote: false,
        }),
        &mut async_cx,
    )
    .await;

    assert!(
        result.is_ok(),
        "Stdio server should be created successfully with timeout"
    );
}

#[gpui::test]
async fn test_multi_worktree_context_server_settings(cx: &mut TestAppContext) {
    const SERVER_A: &str = "server-from-project-a";
    const SERVER_B: &str = "server-from-project-b";

    let server_a_id = ContextServerId(SERVER_A.into());
    let server_b_id = ContextServerId(SERVER_B.into());

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/project_a"),
        json!({
            ".zed": {
                "settings.json": serde_json::to_string(&json!({
                    "context_servers": {
                        "server-from-project-a": {
                            "command": "server-a-binary",
                            "args": []
                        }
                    }
                })).unwrap()
            },
            "code.rs": ""
        }),
    )
    .await;
    fs.insert_tree(
        path!("/project_b"),
        json!({
            ".zed": {
                "settings.json": serde_json::to_string(&json!({
                    "context_servers": {
                        "server-from-project-b": {
                            "command": "server-b-binary",
                            "args": []
                        }
                    }
                })).unwrap()
            },
            "code.rs": ""
        }),
    )
    .await;

    cx.update(|cx| {
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
    });
    // Both servers come from `.zed/settings.json`, which needs approval (ADR-F42).
    cx.update(ProjectLaunchApprovals::approve_everything_for_tests);

    // Create project with only project_a initially
    let project = Project::test(fs.clone(), [path!("/project_a").as_ref()], cx).await;

    let executor = cx.executor();
    let store = project.read_with(cx, |project, _| project.context_server_store());
    store.update(cx, |store, _| {
        store.set_context_server_factory(Box::new(move |id, _| {
            Arc::new(ContextServer::new(
                id.clone(),
                Arc::new(create_fake_transport(id.0.to_string(), executor.clone())),
            ))
        }));
    });

    cx.run_until_parked();

    // Only server-a should be configured
    cx.update(|cx| {
        let configured = store.read(cx).configured_server_ids();
        assert!(
            configured.contains(&server_a_id),
            "server-a should be configured from project_a"
        );
        assert!(
            !configured.contains(&server_b_id),
            "server-b should not be configured yet"
        );
    });

    // Add project_b as a second worktree
    project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path!("/project_b"), true, cx)
        })
        .await
        .expect("Failed to add second worktree");

    cx.run_until_parked();

    // Both servers should now be configured
    cx.update(|cx| {
        let configured = store.read(cx).configured_server_ids();
        assert!(
            configured.contains(&server_a_id),
            "server-a should still be configured from project_a"
        );
        assert!(
            configured.contains(&server_b_id),
            "server-b should now be configured from project_b"
        );

        let store = store.read(cx);
        for server_id in [&server_a_id, &server_b_id] {
            assert_eq!(
                store
                    .launch_approval_details(server_id)
                    .map(|details| details.state),
                Some(LaunchApprovalState::Approved),
                "{server_id} comes from a project and was pre-approved"
            );
        }
        assert_eq!(
            store.status_for_server(&server_b_id),
            Some(ContextServerStatus::Running)
        );
    });
}

#[gpui::test]
async fn test_multi_worktree_duplicate_server_first_wins(cx: &mut TestAppContext) {
    const SHARED_SERVER: &str = "shared-server";

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/project_a"),
        json!({
            ".zed": {
                "settings.json": serde_json::to_string(&json!({
                    "context_servers": {
                        "shared-server": {
                            "command": "binary-from-a",
                            "args": ["arg-a"]
                        }
                    }
                })).unwrap()
            },
            "code.rs": ""
        }),
    )
    .await;
    fs.insert_tree(
        path!("/project_b"),
        json!({
            ".zed": {
                "settings.json": serde_json::to_string(&json!({
                    "context_servers": {
                        "shared-server": {
                            "command": "binary-from-b",
                            "args": ["arg-b"]
                        }
                    }
                })).unwrap()
            },
            "code.rs": ""
        }),
    )
    .await;

    cx.update(|cx| {
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
    });
    cx.update(ProjectLaunchApprovals::approve_everything_for_tests);

    // Create project with both worktrees
    let project = Project::test(
        fs.clone(),
        [path!("/project_a").as_ref(), path!("/project_b").as_ref()],
        cx,
    )
    .await;

    cx.run_until_parked();

    let store = project.read_with(cx, |project, _| project.context_server_store());

    // The server should appear exactly once
    cx.update(|cx| {
        let configured = store.read(cx).configured_server_ids();
        let count = configured
            .iter()
            .filter(|id| id.0.as_ref() == SHARED_SERVER)
            .count();
        assert_eq!(count, 1, "duplicate server ID should appear exactly once");

        let details = store
            .read(cx)
            .launch_approval_details(&ContextServerId(SHARED_SERVER.into()))
            .expect("the shared server is defined by a project");
        assert_eq!(details.target.display().to_string(), "binary-from-a arg-a");
        assert_eq!(details.state, LaunchApprovalState::Approved);
    });
}

#[gpui::test]
async fn test_is_server_enabled(cx: &mut TestAppContext) {
    // We'll be setting up 4 different servers in order to test the following
    // scenarios:
    //
    // 1. Explicit Settings, Enabled
    // 2. Explicit Settings, Disabled
    // 3. No Settings, Registry Descriptor, Enabled
    // 4. No Settings, No Descriptor, Disabled
    const SERVER_1_ID: &str = "mcp-1";
    const SERVER_2_ID: &str = "mcp-2";
    const SERVER_3_ID: &str = "mcp-3";
    const SERVER_4_ID: &str = "mcp-4";

    let (_fs, project) = setup_context_server_test(
        cx,
        json!({"code.rs": ""}),
        vec![
            (
                SERVER_1_ID.into(),
                ContextServerSettings::Extension {
                    enabled: true,
                    remote: false,
                    settings: json!({}),
                },
            ),
            (
                SERVER_2_ID.into(),
                ContextServerSettings::Extension {
                    enabled: false,
                    remote: false,
                    settings: json!({}),
                },
            ),
        ],
    )
    .await;

    let registry = cx.new(|cx| {
        let mut registry = ContextServerDescriptorRegistry::new();
        let descriptor = Arc::new(FakeContextServerDescriptor::new(SERVER_3_ID));
        registry.register_context_server_descriptor(SERVER_3_ID.into(), descriptor, cx);

        registry
    });

    let store = cx.new(|cx| {
        ContextServerStore::test(
            registry.clone(),
            project.read(cx).worktree_store(),
            Some(project.downgrade()),
            cx,
        )
    });

    // Sanity check before proceeding, confirm server 1 and 2 have settings,
    // server 3 is present in the registry while server 4 does not meet any of
    // the conditions.
    cx.update(|cx| {
        let settings = ProjectSettings::get_global(cx);

        assert!(settings.context_servers.contains_key(SERVER_1_ID));
        assert!(settings.context_servers.contains_key(SERVER_2_ID));
        assert!(!settings.context_servers.contains_key(SERVER_3_ID));
        assert!(!settings.context_servers.contains_key(SERVER_4_ID));
    });

    registry.update(cx, |registry, _cx| {
        let descriptors = registry.context_server_descriptors();
        assert_eq!(descriptors.len(), 1);
        assert_eq!(descriptors[0].0.as_ref(), SERVER_3_ID);
    });

    let server_1_id = ContextServerId(SERVER_1_ID.into());
    let server_2_id = ContextServerId(SERVER_2_ID.into());
    let server_3_id = ContextServerId(SERVER_3_ID.into());
    let server_4_id = ContextServerId(SERVER_4_ID.into());

    store.read_with(cx, |store, cx| {
        assert!(store.is_server_enabled(&server_1_id, cx));
        assert!(!store.is_server_enabled(&server_2_id, cx));
        assert!(store.is_server_enabled(&server_3_id, cx));
        assert!(!store.is_server_enabled(&server_4_id, cx));
    })
}

fn assert_server_events(
    store: &Entity<ContextServerStore>,
    expected_events: Vec<(ContextServerId, ContextServerStatus)>,
    cx: &mut TestAppContext,
) -> ServerEvents {
    cx.update(|cx| {
        let mut ix = 0;
        let received_event_count = Rc::new(RefCell::new(0));
        let expected_event_count = expected_events.len();
        let subscription = cx.subscribe(store, {
            let received_event_count = received_event_count.clone();
            move |_, event, _| {
                let ServerStatusChangedEvent {
                    server_id: actual_server_id,
                    status: actual_status,
                } = event;
                let (expected_server_id, expected_status) = &expected_events[ix];

                assert_eq!(
                    actual_server_id, expected_server_id,
                    "Expected different server id at index {}",
                    ix
                );
                assert_eq!(
                    actual_status, expected_status,
                    "Expected different status at index {}",
                    ix
                );
                ix += 1;
                *received_event_count.borrow_mut() += 1;
            }
        });
        ServerEvents {
            expected_event_count,
            received_event_count,
            _subscription: subscription,
        }
    })
}

async fn setup_context_server_test(
    cx: &mut TestAppContext,
    files: serde_json::Value,
    context_server_configurations: Vec<(Arc<str>, ContextServerSettings)>,
) -> (Arc<FakeFs>, Entity<Project>) {
    cx.update(|cx| {
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
        let mut settings = ProjectSettings::get_global(cx).clone();
        for (id, config) in context_server_configurations {
            settings.context_servers.insert(id, config);
        }
        ProjectSettings::override_global(settings, cx);
    });

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/test"), files).await;
    let project = Project::test(fs.clone(), [path!("/test").as_ref()], cx).await;

    (fs, project)
}

struct FakeContextServerDescriptor {
    path: PathBuf,
}

impl FakeContextServerDescriptor {
    fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

impl ContextServerDescriptor for FakeContextServerDescriptor {
    fn command(
        &self,
        _worktree_store: Entity<WorktreeStore>,
        _cx: &AsyncApp,
    ) -> Task<Result<ContextServerCommand>> {
        Task::ready(Ok(ContextServerCommand {
            path: self.path.clone(),
            args: vec!["arg1".to_string(), "arg2".to_string()],
            env: None,
            timeout: None,
        }))
    }

    fn configuration(
        &self,
        _worktree_store: Entity<WorktreeStore>,
        _cx: &AsyncApp,
    ) -> Task<Result<Option<::extension::ContextServerConfiguration>>> {
        Task::ready(Ok(None))
    }
}

const PROJECT_SERVER_ID: &str = "project-server";

fn project_server_settings(args: &[&str]) -> String {
    json!({
        "context_servers": {
            "project-server": {
                "command": "project-server-binary",
                "args": args,
                "env": { "API_TOKEN": "secret" }
            }
        }
    })
    .to_string()
}

fn init_launch_approval_test(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
        cx.set_global(db::AppDatabase::test_new());
    });
}

/// Opens `/test` holding `files` once a factory is in place that records
/// every server it creates, so no server is ever spawned for real.
async fn open_project_with_fake_servers(
    files: serde_json::Value,
    cx: &mut TestAppContext,
) -> (
    Arc<FakeFs>,
    Entity<Project>,
    Entity<ContextServerStore>,
    Rc<RefCell<Vec<ContextServerId>>>,
) {
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/test"), files).await;
    let project = Project::test(fs.clone(), std::iter::empty::<&std::path::Path>(), cx).await;
    let store = project.read_with(cx, |project, _| project.context_server_store());
    let created_servers = Rc::new(RefCell::new(Vec::new()));
    let executor = cx.executor();
    store.update(cx, |store, _| {
        let created_servers = created_servers.clone();
        store.set_context_server_factory(Box::new(move |id, _| {
            created_servers.borrow_mut().push(id.clone());
            Arc::new(ContextServer::new(
                id.clone(),
                Arc::new(create_fake_transport(id.0.to_string(), executor.clone())),
            ))
        }));
    });
    project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path!("/test"), true, cx)
        })
        .await
        .expect("failed to add worktree");
    cx.run_until_parked();
    (fs, project, store, created_servers)
}

fn status_of(
    store: &Entity<ContextServerStore>,
    server_id: &ContextServerId,
    cx: &mut TestAppContext,
) -> Option<ContextServerStatus> {
    store.read_with(cx, |store, _| store.status_for_server(server_id))
}

fn approval_state_of(
    store: &Entity<ContextServerStore>,
    server_id: &ContextServerId,
    cx: &mut TestAppContext,
) -> Option<LaunchApprovalState> {
    store.read_with(cx, |store, _| {
        store
            .launch_approval_details(server_id)
            .map(|details| details.state)
    })
}

/// The key of the launch the approval UI would show for `server_id`.
fn shown_key(
    store: &Entity<ContextServerStore>,
    server_id: &ContextServerId,
    cx: &mut TestAppContext,
) -> LaunchApprovalKey {
    store.read_with(cx, |store, _| {
        store
            .launch_approval_details(server_id)
            .expect("the server needs approval")
            .key
    })
}

async fn approve_shown(
    store: &Entity<ContextServerStore>,
    server_id: &ContextServerId,
    cx: &mut TestAppContext,
) {
    let key = shown_key(store, server_id, cx);
    store
        .update(cx, |store, cx| store.approve_server(server_id, &key, cx))
        .await
        .expect("the approval is stored");
}

#[gpui::test]
async fn test_project_context_server_waits_for_approval(cx: &mut TestAppContext) {
    let server_id = ContextServerId(PROJECT_SERVER_ID.into());
    init_launch_approval_test(cx);
    let (_fs, _project, store, created_servers) = open_project_with_fake_servers(
        json!({
            ".zed": { "settings.json": project_server_settings(&["--serve", "scripts/serve.js"]) },
            "scripts": { "serve.js": "" },
        }),
        cx,
    )
    .await;

    let details = store.read_with(cx, |store, cx| {
        assert_eq!(
            store.status_for_server(&server_id),
            Some(ContextServerStatus::ApprovalRequired)
        );
        assert!(store.get_server(&server_id).is_none());
        assert!(
            store.configuration_for_server(&server_id).is_none(),
            "nothing may forward an unapproved server to an external agent"
        );
        assert!(store.configured_server_ids().contains(&server_id));
        assert!(matches!(
            store.server_source(&server_id, cx),
            Some(ContextServerSource::Project { .. })
        ));
        store
            .launch_approval_details(&server_id)
            .expect("a project-defined server has approval details")
    });
    assert_eq!(details.kind, LaunchKind::ContextServer);
    assert_eq!(details.state, LaunchApprovalState::Pending);
    assert_eq!(
        details.target.display().to_string(),
        "project-server-binary --serve scripts/serve.js"
    );
    assert_eq!(
        details.environment_variable_names,
        vec![SharedString::from("API_TOKEN")]
    );
    assert_eq!(details.execution_host, ExecutionHost::ThisMachine);
    assert_eq!(
        details.definition_file,
        LaunchDefinitionFile::Worktree(path!("/test/.zed/settings.json").into())
    );
    assert!(
        details.references_worktree_file(),
        "scripts/serve.js is a file in the worktree"
    );
    assert!(
        details
            .unbound_worktree_content
            .contains(&UnboundWorktreeContent::WorkingDirectory {
                program: "project-server-binary".into(),
                working_directory: path!("/test").into(),
            }),
        "a program from PATH starts in the project, the directory it is \
         spawned in below: {:?}",
        details.unbound_worktree_content
    );
    assert!(!details.replaces_user_entry);

    // Starting it directly, as the settings switch does, is held back as well.
    let server = Arc::new(ContextServer::new(
        server_id.clone(),
        Arc::new(create_fake_transport(PROJECT_SERVER_ID, cx.executor())),
    ));
    store.update(cx, |store, cx| store.start_server(server, cx));
    cx.run_until_parked();
    assert_eq!(
        status_of(&store, &server_id, cx),
        Some(ContextServerStatus::ApprovalRequired)
    );

    let outcome = cx
        .update(|cx| wait_for_context_server(&store, server_id.clone(), cx))
        .await;
    let error = outcome.expect_err("waiting on a server that needs approval fails at once");
    assert!(error.contains("承認待ち"), "unexpected message: {error}");
    assert!(created_servers.borrow().is_empty());

    approve_shown(&store, &server_id, cx).await;
    cx.run_until_parked();

    assert_eq!(
        status_of(&store, &server_id, cx),
        Some(ContextServerStatus::Running)
    );
    assert_eq!(
        approval_state_of(&store, &server_id, cx),
        Some(LaunchApprovalState::Approved)
    );
    store.read_with(cx, |store, _| {
        assert!(store.configuration_for_server(&server_id).is_some());
        assert_eq!(
            store.working_directory_for_test(&server_id),
            Some(Some(Arc::from(std::path::Path::new(path!("/test"))))),
            "an approved project server was approved to run the project's content"
        );
    });
    assert_eq!(*created_servers.borrow(), vec![server_id]);
}

#[gpui::test]
async fn test_project_context_server_running_worktree_code_is_flagged(cx: &mut TestAppContext) {
    let server_id = ContextServerId(PROJECT_SERVER_ID.into());
    init_launch_approval_test(cx);
    let (_fs, _project, store, created_servers) = open_project_with_fake_servers(
        json!({
            ".zed": {
                "settings.json": json!({
                    "context_servers": {
                        "project-server": {
                            "command": "node",
                            "args": ["server", "../test/tools/check.js"],
                            "env": { "NODE_OPTIONS": "--require=./hook.js", "API_TOKEN": "secret" }
                        }
                    }
                })
                .to_string()
            },
            "hook.js": "",
            "tools": { "check.js": "" },
        }),
        cx,
    )
    .await;

    let details = store.read_with(cx, |store, _| {
        store
            .launch_approval_details(&server_id)
            .expect("a project-defined server has approval details")
    });
    assert!(details.references_worktree_file());
    let unbound = &details.unbound_worktree_content;
    assert!(
        unbound.iter().any(|content| matches!(
            content,
            UnboundWorktreeContent::WorkingDirectory { program, .. } if program.as_ref() == "node"
        )),
        "node started in the worktree loads `server` from it: {unbound:?}"
    );
    assert!(
        details.referenced_worktree_files.is_empty(),
        "`server` is no file, so nothing binds the key to its contents: {:?}",
        details.referenced_worktree_files
    );
    for file in [path!("/test/tools/check.js"), path!("/test/hook.js")] {
        assert!(
            unbound.contains(&UnboundWorktreeContent::File(file.into())),
            "{file}, named through `..` or NODE_OPTIONS, is shown but not bound: {unbound:?}"
        );
    }
    assert_eq!(
        details.code_loading_environment,
        vec![(
            SharedString::from("NODE_OPTIONS"),
            SharedString::from("--require=./hook.js")
        )]
    );
    assert_eq!(
        details.environment_variable_names,
        vec![SharedString::from("API_TOKEN")],
        "only the variables whose values stay hidden"
    );
    assert!(created_servers.borrow().is_empty());
}

#[gpui::test]
async fn test_extension_server_defined_in_any_worktree_is_project_defined(cx: &mut TestAppContext) {
    let server_id = ContextServerId("extension-server".into());
    init_launch_approval_test(cx);
    set_context_server_configuration(
        vec![(
            server_id.0.clone(),
            settings::ContextServerSettingsContent::Extension {
                enabled: true,
                remote: false,
                settings: json!({ "from": "user" }),
            },
        )],
        cx,
    );
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/first"), json!({ "code.rs": "" }))
        .await;
    fs.insert_tree(
        path!("/second"),
        json!({
            ".zed": {
                "settings.json": json!({
                    "context_servers": { "extension-server": { "settings": { "from": "project" } } }
                })
                .to_string()
            }
        }),
    )
    .await;
    let project = Project::test(
        fs,
        [path!("/first").as_ref(), path!("/second").as_ref()],
        cx,
    )
    .await;
    cx.run_until_parked();

    let store = project.read_with(cx, |project, _| project.context_server_store());
    let second_worktree_id = project.read_with(cx, |project, cx| {
        project
            .visible_worktrees(cx)
            .nth(1)
            .map(|worktree| worktree.read(cx).id())
            .expect("two worktrees")
    });
    // The first worktree only sees the user's entry, but the extension reads
    // the second worktree's settings as well.
    store.read_with(cx, |store, cx| {
        assert_eq!(
            store.server_source(&server_id, cx),
            Some(ContextServerSource::Project {
                worktree_id: second_worktree_id,
                settings_file: LaunchDefinitionFile::Worktree(
                    path!("/second/.zed/settings.json").into()
                ),
                host: LaunchHost::ThisMachine,
            })
        );
    });
}

/// A client connected to a mock remote server whose session answers the ping
/// sent while connecting. Handlers registered on the returned session live as
/// long as the returned entity.
async fn connect_to_mock_remote(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) -> (
    Entity<remote::RemoteClient>,
    rpc::AnyProtoClient,
    Entity<()>,
) {
    cx.update(|cx| release_channel::init(semver::Version::new(0, 0, 0), cx));
    server_cx.update(|cx| release_channel::init(semver::Version::new(0, 0, 0), cx));
    let (options, server_session, connect_guard) = remote::RemoteClient::fake_server(cx, server_cx);
    let handler_owner = server_cx.new(|_| ());
    server_session.add_request_handler(
        handler_owner.downgrade(),
        |_, _: rpc::TypedEnvelope<rpc::proto::Ping>, _| async { Ok(rpc::proto::Ack {}) },
    );
    drop(connect_guard);
    let remote_client = remote::RemoteClient::connect_mock(options, cx).await;
    (remote_client, server_session, handler_owner)
}

/// A project with a remote worktree, and a store for it that talks to
/// `remote_client`. Both its store and the project's own create fake servers.
async fn open_remote_project_with_fake_servers(
    remote_client: &Entity<remote::RemoteClient>,
    cx: &mut TestAppContext,
) -> (
    Entity<Project>,
    Entity<ContextServerStore>,
    Rc<RefCell<Vec<ContextServerId>>>,
) {
    let fs = FakeFs::new(cx.executor());
    let project = Project::test(fs, std::iter::empty::<&std::path::Path>(), cx).await;
    project.update(cx, |project, cx| {
        project.add_test_remote_worktree("/remote/project", cx);
    });
    let executor = cx.executor();
    project
        .read_with(cx, |project, _| project.context_server_store())
        .update(cx, |store, _| {
            let executor = executor.clone();
            store.set_context_server_factory(Box::new(move |id, _| {
                Arc::new(ContextServer::new(
                    id.clone(),
                    Arc::new(create_fake_transport(id.0.to_string(), executor.clone())),
                ))
            }));
        });
    let worktree_store = project.read_with(cx, |project, _| project.worktree_store());
    let created_servers = Rc::new(RefCell::new(Vec::new()));
    let store = cx.new(|cx| {
        let mut store = ContextServerStore::remote(
            0,
            remote_client.clone(),
            worktree_store,
            Some(project.downgrade()),
            cx,
        );
        let created_servers = created_servers.clone();
        store.set_context_server_factory(Box::new(move |id, _| {
            created_servers.borrow_mut().push(id.clone());
            Arc::new(ContextServer::new(
                id.clone(),
                Arc::new(create_fake_transport(id.0.to_string(), executor.clone())),
            ))
        }));
        store
    });
    (project, store, created_servers)
}

// A `remote: true` server is resolved again by the remote host from its own
// settings, which commands run over there can rewrite. What comes back must
// be what this machine resolved, even for a server from the user's settings.
#[gpui::test]
async fn test_remote_command_must_match_what_this_machine_resolved(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let matching_id = ContextServerId("matching".into());
    let overridden_id = ContextServerId("overridden".into());
    init_launch_approval_test(cx);
    let (remote_client, server_session, handler_owner) =
        connect_to_mock_remote(cx, server_cx).await;
    server_session.add_request_handler(
        handler_owner.downgrade(),
        |_, envelope: rpc::TypedEnvelope<rpc::proto::GetContextServerCommand>, _| async move {
            let path = if envelope.payload.server_id == "matching" {
                "matching-binary"
            } else {
                "binary-from-the-remote-settings"
            };
            Ok(rpc::proto::ContextServerCommand {
                path: path.to_string(),
                args: vec!["--stdio".to_string()],
                env: [("HTTP_PROXY".to_string(), "http://127.0.0.1:1".to_string())]
                    .into_iter()
                    .collect(),
            })
        },
    );
    let (_project, store, created_servers) =
        open_remote_project_with_fake_servers(&remote_client, cx).await;
    let errors = Rc::new(RefCell::new(Vec::new()));
    let _subscription = cx.update(|cx| {
        let errors = errors.clone();
        cx.subscribe(&store, move |_, event, _| {
            let ServerStatusChangedEvent { server_id, status } = event;
            if let ContextServerStatus::Error(message) = status {
                errors
                    .borrow_mut()
                    .push((server_id.clone(), message.clone()));
            }
        })
    });

    let stdio = |path: &str| settings::ContextServerSettingsContent::Stdio {
        enabled: true,
        remote: true,
        command: ContextServerCommand {
            path: path.into(),
            args: vec!["--stdio".to_string()],
            env: None,
            timeout: None,
        },
    };
    set_context_server_configuration(
        vec![
            (matching_id.0.clone(), stdio("matching-binary")),
            (overridden_id.0.clone(), stdio("user-binary")),
        ],
        cx,
    );
    cx.run_until_parked();

    assert_eq!(
        status_of(&store, &matching_id, cx),
        Some(ContextServerStatus::Running)
    );
    assert!(
        errors
            .borrow()
            .iter()
            .any(|(server_id, message)| server_id == &overridden_id
                && message.contains("一致しません")),
        "unexpected errors: {:?}",
        errors.borrow()
    );
    // A running `remote: true` server is compared with the definition it was
    // started from rather than the command wrapped for the remote host, so
    // it is started once.
    assert_eq!(
        created_servers
            .borrow()
            .iter()
            .filter(|id| **id == matching_id)
            .count(),
        1
    );
    assert!(!created_servers.borrow().contains(&overridden_id));
}

// An extension's server with `remote: true` gets its command from the
// extension on the remote host, which nothing here can predict: that exact
// command needs an approval before it runs.
#[gpui::test]
async fn test_remote_resolved_extension_command_needs_approval(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let server_id = ContextServerId("remote-extension".into());
    init_launch_approval_test(cx);
    let (remote_client, server_session, handler_owner) =
        connect_to_mock_remote(cx, server_cx).await;
    server_session.add_request_handler(
        handler_owner.downgrade(),
        |_, _: rpc::TypedEnvelope<rpc::proto::GetContextServerCommand>, _| async move {
            Ok(rpc::proto::ContextServerCommand {
                path: "/remote/extensions/mcp-server".to_string(),
                args: vec!["--stdio".to_string()],
                env: [
                    ("HTTP_PROXY".to_string(), "http://127.0.0.1:1".to_string()),
                    ("NODE_OPTIONS".to_string(), "--inspect".to_string()),
                ]
                .into_iter()
                .collect(),
            })
        },
    );
    let (_project, store, created_servers) =
        open_remote_project_with_fake_servers(&remote_client, cx).await;
    store.update(cx, |store, cx| {
        store.registry().update(cx, |registry, cx| {
            registry.register_context_server_descriptor(
                server_id.0.clone(),
                Arc::new(FakeContextServerDescriptor::new(
                    "what-this-machine-would-run",
                )),
                cx,
            );
        });
    });
    set_context_server_configuration(
        vec![(
            server_id.0.clone(),
            settings::ContextServerSettingsContent::Extension {
                enabled: true,
                remote: true,
                settings: json!({}),
            },
        )],
        cx,
    );
    cx.run_until_parked();

    let details = store.read_with(cx, |store, _| {
        assert_eq!(
            store.status_for_server(&server_id),
            Some(ContextServerStatus::ApprovalRequired)
        );
        store
            .launch_approval_details(&server_id)
            .expect("the remote-resolved command has approval details")
    });
    assert_eq!(
        details.definition_file,
        LaunchDefinitionFile::ResolvedOnRemoteHost
    );
    assert!(matches!(
        details.execution_host,
        ExecutionHost::Remote { .. }
    ));
    assert_eq!(
        details.target.display().to_string(),
        "/remote/extensions/mcp-server --stdio"
    );
    assert!(
        details.environment_variable_names.is_empty(),
        "the remote host's own proxy variables are not part of the definition, \
         and NODE_OPTIONS is shown with its value: {:?}",
        details.environment_variable_names
    );
    assert_eq!(
        details.code_loading_environment,
        vec![(
            SharedString::from("NODE_OPTIONS"),
            SharedString::from("--inspect")
        )]
    );
    assert!(created_servers.borrow().is_empty());

    approve_shown(&store, &server_id, cx).await;
    cx.run_until_parked();
    assert_eq!(
        status_of(&store, &server_id, cx),
        Some(ContextServerStatus::Running)
    );
    assert_eq!(*created_servers.borrow(), vec![server_id]);
}

#[gpui::test]
async fn test_project_context_server_approval_survives_restart(
    cx: &mut TestAppContext,
    restarted_cx: &mut TestAppContext,
) {
    let server_id = ContextServerId(PROJECT_SERVER_ID.into());
    let files = json!({ ".zed": { "settings.json": project_server_settings(&["--serve"]) } });

    init_launch_approval_test(cx);
    let (_fs, _project, store, _created_servers) =
        open_project_with_fake_servers(files.clone(), cx).await;
    approve_shown(&store, &server_id, cx).await;
    cx.run_until_parked();
    assert_eq!(
        status_of(&store, &server_id, cx),
        Some(ContextServerStatus::Running)
    );

    // A second app on the same database stands in for the next session.
    let database = cx.update(|cx| cx.global::<db::AppDatabase>().0.clone());
    restarted_cx.update(|cx| {
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
        cx.set_global(db::AppDatabase(database));
    });
    let (_fs, _project, restarted_store, created_servers) =
        open_project_with_fake_servers(files, restarted_cx).await;
    assert_eq!(
        status_of(&restarted_store, &server_id, restarted_cx),
        Some(ContextServerStatus::Running)
    );
    assert_eq!(*created_servers.borrow(), vec![server_id]);
}

#[gpui::test]
async fn test_editing_project_context_server_requires_new_approval(cx: &mut TestAppContext) {
    let server_id = ContextServerId(PROJECT_SERVER_ID.into());
    init_launch_approval_test(cx);
    let (fs, _project, store, created_servers) = open_project_with_fake_servers(
        json!({ ".zed": { "settings.json": project_server_settings(&["--serve"]) } }),
        cx,
    )
    .await;
    approve_shown(&store, &server_id, cx).await;
    cx.run_until_parked();
    assert_eq!(
        status_of(&store, &server_id, cx),
        Some(ContextServerStatus::Running)
    );
    let approved_key = store.read_with(cx, |store, _| {
        store
            .launch_approval_details(&server_id)
            .expect("approved server has details")
            .key
    });

    fs.insert_file(
        path!("/test/.zed/settings.json"),
        project_server_settings(&["--serve", "--debug"]).into_bytes(),
    )
    .await;
    cx.run_until_parked();

    store.read_with(cx, |store, _| {
        assert_eq!(
            store.status_for_server(&server_id),
            Some(ContextServerStatus::ApprovalRequired)
        );
        assert!(
            store.get_server(&server_id).is_none(),
            "the old approval does not cover the new definition"
        );
        assert!(store.configuration_for_server(&server_id).is_none());
        let details = store
            .launch_approval_details(&server_id)
            .expect("the edited server has details");
        assert_eq!(details.state, LaunchApprovalState::Pending);
        assert_ne!(details.key, approved_key);
        assert_eq!(
            details.target.display().to_string(),
            "project-server-binary --serve --debug"
        );
    });
    assert_eq!(created_servers.borrow().len(), 1);

    // The approval shown before the edit cannot be applied to the new
    // definition.
    let refused = store
        .update(cx, |store, cx| {
            store.approve_server(&server_id, &approved_key, cx)
        })
        .await;
    assert_eq!(
        refused
            .expect_err("the definition changed since it was shown")
            .to_string(),
        CHANGED_SINCE_SHOWN_MESSAGE
    );
    assert_eq!(
        status_of(&store, &server_id, cx),
        Some(ContextServerStatus::ApprovalRequired)
    );
}

#[gpui::test]
async fn test_user_context_server_needs_no_approval(cx: &mut TestAppContext) {
    let server_id = ContextServerId("user-server".into());
    // No database: a server from the user's settings never looks for one.
    let (_fs, project) = setup_context_server_test(cx, json!({"code.rs": ""}), vec![]).await;
    let store = project.read_with(cx, |project, _| project.context_server_store());
    let executor = cx.executor();
    store.update(cx, |store, _| {
        store.set_context_server_factory(Box::new(move |id, _| {
            Arc::new(ContextServer::new(
                id.clone(),
                Arc::new(create_fake_transport(id.0.to_string(), executor.clone())),
            ))
        }));
    });

    set_context_server_configuration(
        vec![(
            server_id.0.clone(),
            settings::ContextServerSettingsContent::Stdio {
                enabled: true,
                remote: false,
                command: ContextServerCommand {
                    path: "user-server-binary".into(),
                    args: vec![],
                    env: None,
                    timeout: None,
                },
            },
        )],
        cx,
    );
    cx.run_until_parked();

    cx.update(|cx| {
        let store = store.read(cx);
        assert_eq!(
            store.status_for_server(&server_id),
            Some(ContextServerStatus::Running)
        );
        assert_eq!(
            store.server_source(&server_id, cx),
            Some(ContextServerSource::User)
        );
        assert!(store.launch_approval_details(&server_id).is_none());
        let approvals =
            ProjectLaunchApprovals::try_global(cx).expect("a local project sets up approvals");
        assert!(approvals.read(cx).pending_launches().is_empty());
    });
}

#[gpui::test]
async fn test_project_context_server_replacing_user_entry_requires_approval(
    cx: &mut TestAppContext,
) {
    let server_id = ContextServerId(PROJECT_SERVER_ID.into());
    init_launch_approval_test(cx);
    let (_fs, _project, store, created_servers) = open_project_with_fake_servers(
        json!({ ".zed": { "settings.json": project_server_settings(&["--serve"]) } }),
        cx,
    )
    .await;
    // Added once the project's file is loaded, so the only definition under
    // this name that could run is the project's. (Before the file is read,
    // the user's own server may run under the name, which is fine.)
    set_context_server_configuration(
        vec![(
            server_id.0.clone(),
            settings::ContextServerSettingsContent::Stdio {
                enabled: true,
                remote: false,
                command: ContextServerCommand {
                    path: "user-server-binary".into(),
                    args: vec![],
                    env: None,
                    timeout: None,
                },
            },
        )],
        cx,
    );
    cx.run_until_parked();

    store.read_with(cx, |store, _| {
        assert_eq!(
            store.status_for_server(&server_id),
            Some(ContextServerStatus::ApprovalRequired)
        );
        let details = store
            .launch_approval_details(&server_id)
            .expect("the project's entry has details");
        assert!(details.replaces_user_entry);
        assert_eq!(
            details.target.display().to_string(),
            "project-server-binary --serve"
        );
    });
    assert!(created_servers.borrow().is_empty());
}

#[gpui::test]
async fn test_denied_project_context_server_stays_stopped_without_new_notification(
    cx: &mut TestAppContext,
) {
    let server_id = ContextServerId(PROJECT_SERVER_ID.into());
    init_launch_approval_test(cx);
    let notifications = Rc::new(RefCell::new(Vec::new()));
    let _subscription = cx.update(|cx| {
        let approvals = ProjectLaunchApprovals::global(cx);
        let notifications = notifications.clone();
        cx.subscribe(&approvals, move |_, event, _| {
            if let ProjectLaunchApprovalsEvent::PendingLaunchAppeared(details) = event {
                notifications.borrow_mut().push(details.name.clone());
            }
        })
    });
    let files = json!({ ".zed": { "settings.json": project_server_settings(&["--serve"]) } });
    let (_fs, _project, store, created_servers) =
        open_project_with_fake_servers(files.clone(), cx).await;
    assert_eq!(
        *notifications.borrow(),
        vec![SharedString::from(PROJECT_SERVER_ID)]
    );

    let key = shown_key(&store, &server_id, cx);
    store
        .update(cx, |store, cx| store.deny_server(&server_id, &key, cx))
        .await
        .expect("the denial is stored");
    cx.run_until_parked();
    assert_eq!(
        status_of(&store, &server_id, cx),
        Some(ContextServerStatus::ApprovalRequired)
    );
    assert_eq!(
        approval_state_of(&store, &server_id, cx),
        Some(LaunchApprovalState::Denied)
    );

    // Opening the same folder again finds the denial and stays quiet.
    let (_fs, _project, other_store, other_created_servers) =
        open_project_with_fake_servers(files, cx).await;
    assert_eq!(
        status_of(&other_store, &server_id, cx),
        Some(ContextServerStatus::ApprovalRequired)
    );
    assert_eq!(
        approval_state_of(&other_store, &server_id, cx),
        Some(LaunchApprovalState::Denied)
    );
    assert_eq!(
        notifications.borrow().len(),
        1,
        "a denied definition never notifies again"
    );
    assert!(created_servers.borrow().is_empty());
    assert!(other_created_servers.borrow().is_empty());
}

#[gpui::test]
async fn test_revoking_project_context_server_approval_removes_it(cx: &mut TestAppContext) {
    let server_id = ContextServerId(PROJECT_SERVER_ID.into());
    init_launch_approval_test(cx);
    let (_fs, _project, store, _created_servers) = open_project_with_fake_servers(
        json!({ ".zed": { "settings.json": project_server_settings(&["--serve"]) } }),
        cx,
    )
    .await;
    approve_shown(&store, &server_id, cx).await;
    cx.run_until_parked();
    assert_eq!(
        status_of(&store, &server_id, cx),
        Some(ContextServerStatus::Running)
    );

    let key = shown_key(&store, &server_id, cx);
    store
        .update(cx, |store, cx| {
            store.revoke_server_approval(&server_id, &key, cx)
        })
        .await
        .expect("the revocation is stored");
    cx.run_until_parked();

    store.read_with(cx, |store, _| {
        assert_eq!(
            store.status_for_server(&server_id),
            Some(ContextServerStatus::ApprovalRequired)
        );
        assert!(
            store.get_server(&server_id).is_none(),
            "a revoked server is removed, not just stopped"
        );
        assert!(store.configuration_for_server(&server_id).is_none());
    });
    assert_eq!(
        approval_state_of(&store, &server_id, cx),
        Some(LaunchApprovalState::Pending)
    );
}

#[gpui::test]
async fn test_trusting_all_worktrees_does_not_approve_project_context_servers(
    cx: &mut TestAppContext,
) {
    let server_id = ContextServerId(PROJECT_SERVER_ID.into());
    init_launch_approval_test(cx);
    cx.update(|cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |content| {
                content.session.get_or_insert_default().trust_all_worktrees = Some(true);
            });
        });
    });
    let (_fs, _project, store, created_servers) = open_project_with_fake_servers(
        json!({ ".zed": { "settings.json": project_server_settings(&["--serve"]) } }),
        cx,
    )
    .await;

    assert_eq!(
        status_of(&store, &server_id, cx),
        Some(ContextServerStatus::ApprovalRequired)
    );
    assert!(created_servers.borrow().is_empty());
}

// A remote project's `.zed/settings.json` with `remote: false` (the default)
// would run its command on this machine.
#[gpui::test]
async fn test_remote_project_context_server_waits_for_approval(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let server_id = ContextServerId("remote-defined".into());
    init_launch_approval_test(cx);
    cx.update(|cx| release_channel::init(semver::Version::new(0, 0, 0), cx));
    server_cx.update(|cx| release_channel::init(semver::Version::new(0, 0, 0), cx));
    let (options, server_session, connect_guard) = remote::RemoteClient::fake_server(cx, server_cx);
    // Connecting pings the server, which a headless project would answer.
    let _ping_responder = server_cx.new(|_| ());
    server_session.add_request_handler(
        _ping_responder.downgrade(),
        |_, _: rpc::TypedEnvelope<rpc::proto::Ping>, _| async { Ok(rpc::proto::Ack {}) },
    );
    drop(connect_guard);
    let remote_client = remote::RemoteClient::connect_mock(options, cx).await;

    let fs = FakeFs::new(cx.executor());
    let project = Project::test(fs, std::iter::empty::<&std::path::Path>(), cx).await;
    let worktree = project.update(cx, |project, cx| {
        project.add_test_remote_worktree("/remote/project", cx)
    });
    let worktree_id = worktree.read_with(cx, |worktree, _| worktree.id());
    cx.update(|cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store
                .set_local_settings(
                    worktree_id,
                    settings::LocalSettingsPath::InWorktree(util::rel_path::RelPath::empty_arc()),
                    settings::LocalSettingsKind::Settings,
                    Some(
                        &json!({
                            "context_servers": {
                                "remote-defined": { "command": "helper", "args": ["--stdio"] }
                            }
                        })
                        .to_string(),
                    ),
                    cx,
                )
                .expect("the project settings parse");
        });
    });

    let worktree_store = project.read_with(cx, |project, _| project.worktree_store());
    let created_servers = Rc::new(RefCell::new(Vec::new()));
    let executor = cx.executor();
    let store = cx.new(|cx| {
        let mut store = ContextServerStore::remote(
            0,
            remote_client.clone(),
            worktree_store,
            Some(project.downgrade()),
            cx,
        );
        let created_servers = created_servers.clone();
        store.set_context_server_factory(Box::new(move |id, _| {
            created_servers.borrow_mut().push(id.clone());
            Arc::new(ContextServer::new(
                id.clone(),
                Arc::new(create_fake_transport(id.0.to_string(), executor.clone())),
            ))
        }));
        store
    });
    cx.run_until_parked();

    let details = store.read_with(cx, |store, _| {
        assert_eq!(
            store.status_for_server(&server_id),
            Some(ContextServerStatus::ApprovalRequired)
        );
        store
            .launch_approval_details(&server_id)
            .expect("a project-defined server has approval details")
    });
    assert_eq!(
        details.execution_host,
        ExecutionHost::ThisMachine,
        "`remote: false` runs the command here"
    );
    assert!(matches!(details.host, LaunchHost::Remote { .. }));
    assert_eq!(
        details.definition_file,
        LaunchDefinitionFile::Worktree("/remote/project/.zed/settings.json".into())
    );
    assert!(created_servers.borrow().is_empty());

    approve_shown(&store, &server_id, cx).await;
    cx.run_until_parked();
    assert_eq!(
        status_of(&store, &server_id, cx),
        Some(ContextServerStatus::Running)
    );
    store.read_with(cx, |store, _| {
        assert_eq!(
            store.working_directory_for_test(&server_id),
            Some(Some(neutral_working_directory())),
            "spawned here for a remote project, it starts in the neutral directory"
        );
    });
    assert_eq!(*created_servers.borrow(), vec![server_id]);
}
