use super::tool_permissions::{
    ambiguous_worktree_note, authorize_symlink_escapes, canonicalize_worktree_roots,
    collect_symlink_escapes, resolve_creatable_global_skill_descendant_path,
    resolve_global_skill_descendant_path, resolve_new_path_in_the_project, sensitive_settings_kind,
};
use crate::{
    AgentTool, ToolCallEventStream, ToolInput, ToolPermissionDecision,
    authorize_with_sensitive_settings, decide_permission_for_paths,
};
use agent_client_protocol::schema::v1 as acp;
use agent_settings::AgentSettings;
use futures::FutureExt as _;
use gpui::{App, Entity, Task};
use project::Project;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings::Settings;
use std::path::Path;
use std::sync::Arc;
use util::markdown::MarkdownInlineCode;

/// Copies a file or directory in the project, and returns confirmation that the copy succeeded.
/// Directory contents will be copied recursively.
///
/// This tool should be used when it's desirable to create a copy of a file or directory without modifying the original.
/// It's much more efficient than doing this by separately reading and then writing the file or directory's contents, so this tool should be preferred over that approach whenever copying is the goal.
/// The only supported paths outside the project are descendants of `~/.agents/skills`, for global agent skills.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct CopyPathToolInput {
    /// The source path of the file or directory to copy.
    /// If a directory is specified, its contents will be copied recursively.
    ///
    /// <example>
    /// If the project has the following files:
    ///
    /// - directory1/a/something.txt
    /// - directory2/a/things.txt
    /// - directory3/a/other.txt
    ///
    /// You can copy the first file by providing a source_path of "directory1/a/something.txt"
    /// </example>
    pub source_path: String,
    /// The destination path where the file or directory should be copied to.
    ///
    /// <example>
    /// To copy "directory1/a/something.txt" to "directory2/b/copy.txt", provide a destination_path of "directory2/b/copy.txt"
    /// </example>
    pub destination_path: String,
}

pub struct CopyPathTool {
    project: Entity<Project>,
}

impl CopyPathTool {
    pub fn new(project: Entity<Project>) -> Self {
        Self { project }
    }
}

impl AgentTool for CopyPathTool {
    type Input = CopyPathToolInput;
    type Output = String;

    const NAME: &'static str = "copy_path";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Move
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> ui::SharedString {
        if let Ok(input) = input {
            let src = MarkdownInlineCode(&input.source_path);
            let dest = MarkdownInlineCode(&input.destination_path);
            format!("{src} を {dest} にコピー").into()
        } else {
            "パスをコピー".into()
        }
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        let project = self.project.clone();
        cx.spawn(async move |cx| {
            let input = input.recv().await.map_err(|e| e.to_string())?;
            let paths = vec![input.source_path.clone(), input.destination_path.clone()];
            let decision = cx.update(|cx| {
                decide_permission_for_paths(Self::NAME, &paths, &AgentSettings::get_global(cx))
            });
            if let ToolPermissionDecision::Deny(reason) = decision {
                return Err(reason);
            }

            let fs = project.read_with(cx, |project, _cx| project.fs().clone());
            let canonical_roots = canonicalize_worktree_roots(&project, &fs, cx).await;

            let global_source_path =
                resolve_global_skill_descendant_path(Path::new(&input.source_path), fs.as_ref())
                    .await;
            let global_destination_path = resolve_creatable_global_skill_descendant_path(
                Path::new(&input.destination_path),
                fs.as_ref(),
            )
            .await;

            let symlink_escapes: Vec<(&str, std::path::PathBuf)> =
                project.read_with(cx, |project, cx| {
                    collect_symlink_escapes(
                        project,
                        &input.source_path,
                        &input.destination_path,
                        &canonical_roots,
                        cx,
                    )
                });

            let sensitive_kind = sensitive_settings_kind(
                Path::new(&input.source_path),
                &canonical_roots,
                fs.as_ref(),
            )
            .await
            .or(sensitive_settings_kind(
                Path::new(&input.destination_path),
                &canonical_roots,
                fs.as_ref(),
            )
            .await);

            // A destination that doesn't exist yet resolves nowhere, so
            // `find_project_path` alone would call a copy that stays inside
            // the project "outside the project" — and then refuse it after
            // the user approved. Place it where exactly one worktree could
            // hold it (ADR-F54 決定 3).
            // The count is kept because the card below promises a specific
            // worktree when there is more than one candidate, and approving
            // that card has to put the copy there rather than refuse it.
            let (destination_project_path, destination_candidate_count) =
                project.read_with(cx, |project, cx| {
                    match project.find_project_path(&input.destination_path, cx) {
                        Some(project_path) => (Some(project_path), 1),
                        None => match resolve_new_path_in_the_project(
                            project,
                            Path::new(&input.destination_path),
                            cx,
                        ) {
                            Some(placement) => {
                                (Some(placement.project_path), placement.candidate_count)
                            }
                            None => (None, 0),
                        },
                    }
                });

            // 決定 3: either end outside every worktree root is never
            // approved by the mode alone. A sensitive location always prompts
            // anyway, so it is left to the sensitive-settings path below.
            let outside_project = sensitive_kind.is_none()
                && (destination_candidate_count != 1
                    || project.read_with(cx, |project, cx| {
                        project.find_project_path(&input.source_path, cx).is_none()
                    }));

            // 決定 1: when several worktrees could hold the destination the
            // copy silently lands in the first, so the card names it rather
            // than claiming the path is outside the project.
            let ambiguous_destination_note = destination_project_path
                .as_ref()
                // `outside_project` is what carries "the sensitive-settings card
                // isn't the one being shown": overriding that card here would
                // offer a standing approval for a settings file.
                .filter(|_| outside_project && destination_candidate_count >= 2)
                .and_then(|project_path| {
                    project.read_with(cx, |project, cx| {
                        project.short_full_path_for_project_path(project_path, cx)
                    })
                })
                .map(|destination| ambiguous_worktree_note(&destination, "コピーされます"));

            // A judged decision reaches the same authorization call, which
            // re-evaluates the settings and runs the judge there; treating it
            // as "no confirmation needed" would run the tool unreviewed.
            let needs_confirmation = matches!(
                decision,
                ToolPermissionDecision::Confirm | ToolPermissionDecision::Judge
            ) || (matches!(decision, ToolPermissionDecision::Allow) && sensitive_kind.is_some());

            let authorize = if !symlink_escapes.is_empty() {
                // Symlink escape authorization replaces (rather than supplements)
                // the normal tool-permission prompt. The symlink prompt already
                // requires explicit user approval with the canonical target shown,
                // which is strictly more security-relevant than a generic confirm.
                Some(cx.update(|cx| {
                    authorize_symlink_escapes(Self::NAME, &symlink_escapes, &event_stream, cx)
                }))
            } else if outside_project || needs_confirmation {
                Some(cx.update(|cx| {
                    let src = MarkdownInlineCode(&input.source_path);
                    let dest = MarkdownInlineCode(&input.destination_path);
                    let context = crate::ToolPermissionContext::new(
                        Self::NAME,
                        vec![input.source_path.clone(), input.destination_path.clone()],
                    );
                    let title = format!("{src} を {dest} にコピー");
                    if let Some(note) = &ambiguous_destination_note {
                        event_stream.authorize_with_note(&title, note, context, cx)
                    } else if outside_project {
                        event_stream.authorize_outside_project(&title, context, cx)
                    } else {
                        authorize_with_sensitive_settings(
                            sensitive_kind,
                            context,
                            &title,
                            &event_stream,
                            cx,
                        )
                    }
                }))
            } else {
                None
            };

            if let Some(authorize) = authorize {
                authorize.await.map_err(|e| e.to_string())?;
            }

            if global_source_path.is_some() || global_destination_path.is_some() {
                let source_path = if let Some(global_source_path) = global_source_path {
                    global_source_path
                } else {
                    project.read_with(cx, |project, cx| {
                        let project_path = project.find_project_path(&input.source_path, cx).ok_or_else(|| {
                            format!("Source path {} was not found in the project.", input.source_path)
                        })?;
                        project.entry_for_path(&project_path, cx).ok_or_else(|| {
                            format!("Source path {} was not found in the project.", input.source_path)
                        })?;
                        project.absolute_path(&project_path, cx).ok_or_else(|| {
                            format!("Source path {} could not be resolved.", input.source_path)
                        })
                    })?
                };

                let destination_path = if let Some(global_destination_path) = global_destination_path
                {
                    global_destination_path
                } else {
                    let project_path = destination_project_path.clone().ok_or_else(|| {
                        format!(
                            "Destination path {} was outside the project.",
                            input.destination_path
                        )
                    })?;
                    project.read_with(cx, |project, cx| {
                        project.absolute_path(&project_path, cx).ok_or_else(|| {
                            format!(
                                "Destination path {} could not be resolved.",
                                input.destination_path
                            )
                        })
                    })?
                };

                // ADR-F50: the copy may land in a worktree.
                project
                    .update(cx, |project, cx| {
                        project.note_agent_write_to_path(&destination_path, cx)
                    })
                    .await;
                futures::select! {
                    result = fs::copy_recursive(
                        fs.as_ref(),
                        &source_path,
                        &destination_path,
                        fs::CopyOptions::default(),
                    ).fuse() => {
                        result.map_err(|e| format!("Copying {} to {}: {e}", input.source_path, input.destination_path))?;
                    }
                    _ = event_stream.cancelled_by_user().fuse() => {
                        return Err("Copy cancelled by user".to_string());
                    }
                }

                project.update(cx, |project, cx| {
                    project.note_agent_write_landed(&destination_path, cx)
                });
                return Ok(format!(
                    "Copied {} to {}",
                    input.source_path, input.destination_path
                ));
            }

            // ADR-F50: the copy lands in the destination's worktree.
            let noted = project.update(cx, |project, cx| {
                destination_project_path
                    .as_ref()
                    .map(|destination| project.note_agent_write_to_project_path(destination, cx))
            });
            if let Some(noted) = noted {
                noted.await;
            }
            // The destination the card was about is the destination we copy
            // to: re-resolving it here would drop the new-path placement and
            // refuse the copy after the user approved it.
            let copy_task = project.update(cx, |project, cx| {
                match project
                    .find_project_path(&input.source_path, cx)
                    .and_then(|project_path| project.entry_for_path(&project_path, cx))
                {
                    Some(entity) => match destination_project_path.clone() {
                        Some(project_path) => Ok(project.copy_entry(entity.id, project_path, cx)),
                        None => Err(format!(
                            "Destination path {} was outside the project.",
                            input.destination_path
                        )),
                    },
                    None => Err(format!(
                        "Source path {} was not found in the project.",
                        input.source_path
                    )),
                }
            })?;

            let result = futures::select! {
                result = copy_task.fuse() => result,
                _ = event_stream.cancelled_by_user().fuse() => {
                    return Err("Copy cancelled by user".to_string());
                }
            };
            result.map_err(|e| {
                format!(
                    "Copying {} to {}: {e}",
                    input.source_path, input.destination_path
                )
            })?;
            Ok(format!(
                "Copied {} to {}",
                input.source_path, input.destination_path
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::Fs as _;
    use gpui::TestAppContext;
    use project::{FakeFs, Project};
    use serde_json::json;
    use settings::SettingsStore;
    use std::path::PathBuf;
    use util::path;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
        cx.update(|cx| {
            let mut settings = AgentSettings::get_global(cx).clone();
            settings.tool_permissions.default = settings::ToolPermissionMode::Allow;
            AgentSettings::override_global(settings, cx);
        });
    }

    /// 決定 3: a copy whose destination doesn't exist yet resolves nowhere,
    /// but with one worktree it plainly stays in the project — it used to be
    /// called "outside the project" on the card and then refused after the
    /// user approved it.
    #[gpui::test]
    async fn test_copy_path_to_new_relative_destination_stays_in_the_project(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({ "a.txt": "中身" }))
            .await;
        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let tool = Arc::new(CopyPathTool::new(project));
        let (event_stream, mut event_rx) = ToolCallEventStream::test();
        let result = cx
            .update(|cx| {
                tool.run(
                    ToolInput::resolved(CopyPathToolInput {
                        source_path: "a.txt".to_string(),
                        destination_path: "b.txt".to_string(),
                    }),
                    event_stream,
                    cx,
                )
            })
            .await;
        assert!(result.is_ok(), "{result:?}");
        event_rx.expect_no_authorization();
        assert_eq!(
            fs.load(path!("/root/a.txt").as_ref()).await.unwrap(),
            "中身"
        );
        assert_eq!(
            fs.load(path!("/root/b.txt").as_ref()).await.unwrap(),
            "中身"
        );
    }

    #[gpui::test]
    async fn test_copy_path_global_skill_directory_to_project(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root/project"), json!({})).await;
        let skill_dir = agent_skills::global_skills_dir().join("my-skill");
        fs.insert_tree(&skill_dir, json!({ "SKILL.md": "content" }))
            .await;
        let project = Project::test(fs.clone(), [path!("/root/project").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let tool = Arc::new(CopyPathTool::new(project));
        let input_path = PathBuf::from("~")
            .join(".agents")
            .join("skills")
            .join("my-skill")
            .to_string_lossy()
            .into_owned();

        let (event_stream, mut event_rx) = ToolCallEventStream::test();
        let task = cx.update(|cx| {
            tool.run(
                ToolInput::resolved(CopyPathToolInput {
                    source_path: input_path,
                    destination_path: path!("/root/project/my-skill").to_string(),
                }),
                event_stream,
                cx,
            )
        });

        let auth = event_rx.expect_authorization().await;
        let title = auth.tool_call.fields.title.as_deref().unwrap_or("");
        assert!(
            title.contains("エージェントのスキル"),
            "Authorization title should mention agent skills, got: {title}",
        );
        assert!(
            auth.options
                .first_option_of_kind(acp::PermissionOptionKind::AllowAlways)
                .is_none(),
            "agent skills prompt must not offer an \"Always allow\" option: {:?}",
            auth.options,
        );
        auth.response
            .send(acp_thread::SelectedPermissionOutcome::new(
                acp::PermissionOptionId::new("allow"),
                acp::PermissionOptionKind::AllowOnce,
            ))
            .expect("authorization response should send");

        let result = task.await;
        assert!(result.is_ok(), "should copy after approval: {result:?}");
        assert!(fs.is_dir(&skill_dir).await);
        assert_eq!(
            fs.load(path!("/root/project/my-skill/SKILL.md").as_ref())
                .await
                .unwrap(),
            "content"
        );
    }

    #[gpui::test]
    async fn test_copy_path_project_directory_to_global_skill_directory(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root/project"),
            json!({ "exported-skill": { "SKILL.md": "content" } }),
        )
        .await;
        let skills_dir = agent_skills::global_skills_dir();
        fs.create_dir(&skills_dir).await.unwrap();
        let project = Project::test(fs.clone(), [path!("/root/project").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let tool = Arc::new(CopyPathTool::new(project));
        let destination_path = PathBuf::from("~")
            .join(".agents")
            .join("skills")
            .join("exported-skill")
            .to_string_lossy()
            .into_owned();

        let (event_stream, mut event_rx) = ToolCallEventStream::test();
        let task = cx.update(|cx| {
            tool.run(
                ToolInput::resolved(CopyPathToolInput {
                    source_path: path!("/root/project/exported-skill").to_string(),
                    destination_path,
                }),
                event_stream,
                cx,
            )
        });

        let auth = event_rx.expect_authorization().await;
        let title = auth.tool_call.fields.title.as_deref().unwrap_or("");
        assert!(
            title.contains("エージェントのスキル"),
            "Authorization title should mention agent skills, got: {title}",
        );
        assert!(
            auth.options
                .first_option_of_kind(acp::PermissionOptionKind::AllowAlways)
                .is_none(),
            "agent skills prompt must not offer an \"Always allow\" option: {:?}",
            auth.options,
        );
        auth.response
            .send(acp_thread::SelectedPermissionOutcome::new(
                acp::PermissionOptionId::new("allow"),
                acp::PermissionOptionKind::AllowOnce,
            ))
            .expect("authorization response should send");

        let result = task.await;
        assert!(result.is_ok(), "should copy after approval: {result:?}");
        assert!(
            fs.is_dir(path!("/root/project/exported-skill").as_ref())
                .await
        );
        assert_eq!(
            fs.load(skills_dir.join("exported-skill").join("SKILL.md").as_ref())
                .await
                .unwrap(),
            "content"
        );
    }

    #[gpui::test]
    async fn test_copy_path_symlink_escape_source_requests_authorization(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                "project": {
                    "src": { "file.txt": "content" }
                },
                "external": {
                    "secret.txt": "SECRET"
                }
            }),
        )
        .await;

        fs.create_symlink(
            path!("/root/project/link_to_external").as_ref(),
            PathBuf::from("../external"),
        )
        .await
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root/project").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let tool = Arc::new(CopyPathTool::new(project));

        let input = CopyPathToolInput {
            source_path: "project/link_to_external".into(),
            destination_path: "project/external_copy".into(),
        };

        let (event_stream, mut event_rx) = ToolCallEventStream::test();
        let task = cx.update(|cx| tool.run(ToolInput::resolved(input), event_stream, cx));

        let auth = event_rx.expect_authorization().await;
        let title = auth.tool_call.fields.title.as_deref().unwrap_or("");
        assert!(
            title.contains("プロジェクトの外を指しています")
                || title.contains("プロジェクト外へのシンボリックリンク"),
            "Authorization title should mention symlink escape, got: {title}",
        );

        auth.response
            .send(acp_thread::SelectedPermissionOutcome::new(
                acp::PermissionOptionId::new("allow"),
                acp::PermissionOptionKind::AllowOnce,
            ))
            .unwrap();

        let result = task.await;
        assert!(result.is_ok(), "should succeed after approval: {result:?}");
    }

    #[gpui::test]
    async fn test_copy_path_symlink_escape_denied(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                "project": {
                    "src": { "file.txt": "content" }
                },
                "external": {
                    "secret.txt": "SECRET"
                }
            }),
        )
        .await;

        fs.create_symlink(
            path!("/root/project/link_to_external").as_ref(),
            PathBuf::from("../external"),
        )
        .await
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root/project").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let tool = Arc::new(CopyPathTool::new(project));

        let input = CopyPathToolInput {
            source_path: "project/link_to_external".into(),
            destination_path: "project/external_copy".into(),
        };

        let (event_stream, mut event_rx) = ToolCallEventStream::test();
        let task = cx.update(|cx| tool.run(ToolInput::resolved(input), event_stream, cx));

        let auth = event_rx.expect_authorization().await;
        drop(auth);

        let result = task.await;
        assert!(result.is_err(), "should fail when denied");
    }

    #[gpui::test]
    async fn test_copy_path_symlink_escape_confirm_requires_single_approval(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        cx.update(|cx| {
            let mut settings = AgentSettings::get_global(cx).clone();
            settings.tool_permissions.default = settings::ToolPermissionMode::Confirm;
            AgentSettings::override_global(settings, cx);
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                "project": {
                    "src": { "file.txt": "content" }
                },
                "external": {
                    "secret.txt": "SECRET"
                }
            }),
        )
        .await;

        fs.create_symlink(
            path!("/root/project/link_to_external").as_ref(),
            PathBuf::from("../external"),
        )
        .await
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root/project").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let tool = Arc::new(CopyPathTool::new(project));

        let input = CopyPathToolInput {
            source_path: "project/link_to_external".into(),
            destination_path: "project/external_copy".into(),
        };

        let (event_stream, mut event_rx) = ToolCallEventStream::test();
        let task = cx.update(|cx| tool.run(ToolInput::resolved(input), event_stream, cx));

        let auth = event_rx.expect_authorization().await;
        let title = auth.tool_call.fields.title.as_deref().unwrap_or("");
        assert!(
            title.contains("プロジェクトの外を指しています")
                || title.contains("プロジェクト外へのシンボリックリンク"),
            "Authorization title should mention symlink escape, got: {title}",
        );

        auth.response
            .send(acp_thread::SelectedPermissionOutcome::new(
                acp::PermissionOptionId::new("allow"),
                acp::PermissionOptionKind::AllowOnce,
            ))
            .unwrap();

        assert!(
            !matches!(
                event_rx.try_recv(),
                Ok(Ok(crate::ThreadEvent::ToolCallAuthorization(_)))
            ),
            "Expected a single authorization prompt",
        );

        let result = task.await;
        assert!(
            result.is_ok(),
            "Tool should succeed after one authorization: {result:?}"
        );
    }

    #[gpui::test]
    async fn test_copy_path_symlink_escape_honors_deny_policy(cx: &mut TestAppContext) {
        init_test(cx);
        cx.update(|cx| {
            let mut settings = AgentSettings::get_global(cx).clone();
            settings.tool_permissions.tools.insert(
                "copy_path".into(),
                agent_settings::ToolRules {
                    default: Some(settings::ToolPermissionMode::Deny),
                    ..Default::default()
                },
            );
            AgentSettings::override_global(settings, cx);
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                "project": {
                    "src": { "file.txt": "content" }
                },
                "external": {
                    "secret.txt": "SECRET"
                }
            }),
        )
        .await;

        fs.create_symlink(
            path!("/root/project/link_to_external").as_ref(),
            PathBuf::from("../external"),
        )
        .await
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root/project").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let tool = Arc::new(CopyPathTool::new(project));

        let input = CopyPathToolInput {
            source_path: "project/link_to_external".into(),
            destination_path: "project/external_copy".into(),
        };

        let (event_stream, mut event_rx) = ToolCallEventStream::test();
        let result = cx
            .update(|cx| tool.run(ToolInput::resolved(input), event_stream, cx))
            .await;

        assert!(result.is_err(), "Tool should fail when policy denies");
        assert!(
            !matches!(
                event_rx.try_recv(),
                Ok(Ok(crate::ThreadEvent::ToolCallAuthorization(_)))
            ),
            "Deny policy should not emit symlink authorization prompt",
        );
    }
}
