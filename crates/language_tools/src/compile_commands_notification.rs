//! Tells the user once per folder that clangd has no compile commands for a C or C++ file
//! there. Without them clangd cannot resolve the file's include paths, so going to a
//! function in a library or in another file finds nothing or only a prototype, with nothing
//! else saying why.

use std::{cell::RefCell, collections::HashSet, path::Path, rc::Rc};

use db::kvp::Dismissable;
use editor::Editor;
use gpui::{App, AppContext as _, Entity, Task};
use lsp::LanguageServerName;
use project::{Project, WorktreeId, project_settings::ProjectSettings};
use settings::{Settings as _, SettingsLocation};
use ui::prelude::*;
use util::rel_path::{RelPath, RelPathBuf};
use workspace::{
    Workspace,
    notifications::{NotificationId, simple_message_notification::MessageNotification},
};

struct MissingCompileCommands;

impl Dismissable for MissingCompileCommands {
    const KEY: &'static str = "dismissed-missing-compile-commands-notification";
}

const CLANGD: LanguageServerName = LanguageServerName::new_static("clangd");
const COMPILE_COMMANDS: &str = "compile_commands.json";
const COMPILE_FLAGS: &str = "compile_flags.txt";
/// clangd's configuration file, which can give the compile flags or the compile commands'
/// folder.
const CLANGD_CONFIG: &str = ".clangd";

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, cx| {
        let checked = Rc::new(RefCell::new(HashSet::<WorktreeId>::default()));
        cx.subscribe_self({
            let checked = checked.clone();
            move |workspace, event: &workspace::Event, cx| {
                if matches!(
                    event,
                    workspace::Event::ActiveItemChanged | workspace::Event::ItemAdded { .. }
                ) {
                    check_active_file(workspace, &checked, cx);
                }
            }
        })
        .detach();
        // A file's language can be known only after its editor is shown; its language server
        // starts once it is.
        let project = workspace.project().clone();
        cx.subscribe(&project, move |workspace, _, event: &project::Event, cx| {
            if let project::Event::LanguageServerAdded(..) = event {
                check_active_file(workspace, &checked, cx);
            }
        })
        .detach();
    })
    .detach();
}

fn check_active_file(
    workspace: &mut Workspace,
    checked: &Rc<RefCell<HashSet<WorktreeId>>>,
    cx: &mut Context<Workspace>,
) {
    let project = workspace.project().clone();
    // Only the host of a shared project can look at its files.
    if project.read(cx).is_via_collab() || MissingCompileCommands::dismissed(cx) {
        return;
    }
    let Some(editor) = workspace.active_item_as::<Editor>(cx) else {
        return;
    };
    let Some(buffer) = editor.read(cx).buffer().read(cx).as_singleton() else {
        return;
    };
    let buffer = buffer.read(cx);
    if !buffer
        .language()
        .is_some_and(|language| matches!(language.name().as_ref(), "C" | "C++"))
    {
        return;
    }
    let Some(file) = buffer.file() else {
        return;
    };
    // Another language server, such as ccls, finds its compile commands its own way. Until
    // clangd is started for the file, `LanguageServerAdded` brings the check back.
    if project
        .read(cx)
        .language_server_id_for_name(buffer, &CLANGD, cx)
        .is_none()
    {
        return;
    }
    let worktree_id = file.worktree_id(cx);
    let path = file.path().clone();
    if !checked.borrow_mut().insert(worktree_id) {
        return;
    }
    if names_compile_commands_dir(worktree_id, &path, cx) {
        return;
    }
    let Some(lookup) = lookup_for(&project, worktree_id, &path, cx) else {
        return;
    };
    let file_name = path
        .file_name()
        .map(str::to_string)
        .unwrap_or_else(|| lookup.root_name.clone());
    cx.spawn(async move |workspace, cx| {
        if !lookup.is_missing.await {
            return;
        }
        if cx.update(|cx| MissingCompileCommands::dismissed(cx)) {
            return;
        }
        workspace
            .update(cx, |workspace, cx| {
                show_notification(workspace, worktree_id, file_name, cx)
            })
            .ok();
    })
    .detach();
}

/// Whether the user's or the project's settings point clangd at its compile commands.
fn names_compile_commands_dir(worktree_id: WorktreeId, path: &RelPath, cx: &App) -> bool {
    let settings = ProjectSettings::get(Some(SettingsLocation { worktree_id, path }), cx);
    let Some(settings) = settings.lsp.get(&CLANGD) else {
        return false;
    };
    let in_initialization_options = settings
        .initialization_options
        .as_ref()
        .and_then(|options| options.get("compilationDatabasePath"))
        .is_some_and(|value| !value.is_null());
    in_initialization_options
        || settings
            .binary
            .as_ref()
            .and_then(|binary| binary.arguments.as_ref())
            .is_some_and(|arguments| {
                arguments.iter().any(|argument| {
                    let option = argument.trim_start_matches('-');
                    argument.starts_with('-')
                        && (option == "compile-commands-dir"
                            || option.starts_with("compile-commands-dir="))
                })
            })
}

struct Lookup {
    root_name: String,
    is_missing: Task<bool>,
}

/// Looks where clangd does: in each folder from the file's up to the root of the file
/// system, and in the `build` folder of each. A `.clangd` file on the way may set the flags
/// or the commands' folder, which is not read here, so it keeps the notification away too. A file the folder's snapshot holds settles it
/// at once; the rest is asked of the file system, because the snapshot leaves out ignored
/// and excluded folders, such as a `build` folder, and what lies above the folder.
fn lookup_for(
    project: &Entity<Project>,
    worktree_id: WorktreeId,
    path: &RelPath,
    cx: &App,
) -> Option<Lookup> {
    let project_ref = project.read(cx);
    let worktree = project_ref.worktree_for_id(worktree_id, cx)?;
    let worktree = worktree.read(cx);
    let path_style = project_ref.path_style(cx);
    let root_name = worktree.root_name_str().to_string();
    let snapshot = worktree.snapshot();
    let root = worktree.abs_path();

    let mut inside = Vec::new();
    if !worktree.is_single_file() {
        let mut folder = path.parent();
        while let Some(current) = folder {
            inside.extend(candidates_in(current));
            folder = current.parent();
        }
    }
    if inside.iter().any(|candidate| {
        snapshot
            .entry_for_path(candidate.as_rel_path())
            .is_some_and(|entry| entry.is_file())
    }) {
        return Some(Lookup {
            root_name,
            is_missing: Task::ready(false),
        });
    }

    let mut abs_paths = Vec::new();
    for candidate in &inside {
        abs_paths
            .push(path_style.join(&root, candidate.as_rel_path().display(path_style).as_ref())?);
    }
    // Above the worktree's root, or above the single file it is.
    let mut folder = path_style.parent(&root);
    while let Some(current) = folder {
        for name in [COMPILE_COMMANDS, COMPILE_FLAGS, CLANGD_CONFIG] {
            abs_paths.push(path_style.join(current, name)?);
        }
        abs_paths.push(path_style.join(path_style.join(current, "build")?, COMPILE_COMMANDS)?);
        folder = path_style.parent(current);
    }

    let checks = abs_paths
        .into_iter()
        .map(|abs_path| file_exists(project, abs_path, cx))
        .collect::<Vec<_>>();
    Some(Lookup {
        root_name,
        is_missing: cx.background_spawn(async move {
            // Anything not known to be absent keeps the notification away.
            futures::future::join_all(checks)
                .await
                .into_iter()
                .all(|exists| exists == Some(false))
        }),
    })
}

fn candidates_in(folder: &RelPath) -> Vec<RelPathBuf> {
    [
        COMPILE_COMMANDS,
        COMPILE_FLAGS,
        CLANGD_CONFIG,
        "build/compile_commands.json",
    ]
    .into_iter()
    .filter_map(|name| RelPath::from_unix_str(name).ok())
    .map(|name| folder.join(name))
    .collect()
}

/// `None` when it could not be found out.
fn file_exists(project: &Entity<Project>, abs_path: String, cx: &App) -> Task<Option<bool>> {
    let project = project.read(cx);
    if project.is_local() {
        let fs = project.fs().clone();
        return cx.background_spawn(async move {
            fs.metadata(Path::new(&abs_path))
                .await
                .ok()
                .map(|metadata| metadata.is_some_and(|metadata| !metadata.is_dir))
        });
    }
    let Some(remote_client) = project.remote_client() else {
        return Task::ready(None);
    };
    let request = remote_client
        .read(cx)
        .proto_client()
        .request(proto::GetPathMetadata {
            project_id: proto::REMOTE_SERVER_PROJECT_ID,
            path: abs_path,
            canonicalize: false,
        });
    cx.background_spawn(async move {
        request
            .await
            .ok()
            .map(|response| response.exists && !response.is_dir)
    })
}

/// Keeps the notification away from every folder from now on, also in later sessions.
fn dont_show_again(cx: &mut App) {
    MissingCompileCommands::set_dismissed(true, cx);
}

/// The notification shown for the folder `worktree_id`.
pub fn notification_id(worktree_id: WorktreeId) -> NotificationId {
    NotificationId::composite::<MissingCompileCommands>(worktree_id.to_usize())
}

fn show_notification(
    workspace: &mut Workspace,
    worktree_id: WorktreeId,
    file_name: String,
    cx: &mut Context<Workspace>,
) {
    let message = i18n::t!(
        "clangd found no compile_commands.json or compile_flags.txt for {file_name}, so it may not find definitions in libraries or in other files. Generate one, for example with `cmake -B build -DCMAKE_EXPORT_COMPILE_COMMANDS=ON` or `bear -- make`. To keep it out of the project, generate it in another folder, such as one under your home folder, and add `--compile-commands-dir=<that folder>` to `lsp.clangd.binary.arguments`. In the project's folder or its `build` folder, clangd finds it without that, and `.git/info/exclude` keeps it out of git. With a cross compiler, also add `--query-driver=<path to the compiler>` to `lsp.clangd.binary.arguments`."
    );
    workspace.show_notification(notification_id(worktree_id), cx, move |cx| {
        cx.new(|cx| {
            MessageNotification::new(message, cx)
                .with_title(i18n::t!("No compile commands for clangd"))
                .secondary_message(i18n::t!("Don't Show Again"))
                .secondary_on_click(|_, cx| dont_show_again(cx))
        })
    });
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use gpui::{TestAppContext, UpdateGlobal as _, VisualTestContext};
    use language::{FakeLspAdapter, Language, LanguageConfig, LanguageMatcher};
    use project::{FakeFs, Project};
    use serde_json::json;
    use settings::SettingsStore;
    use util::path;
    use workspace::{MultiWorkspace, Workspace, notifications::NotificationId};

    use super::MissingCompileCommands;

    fn language(name: &'static str, suffixes: &[&str]) -> std::sync::Arc<Language> {
        std::sync::Arc::new(Language::new(
            LanguageConfig {
                name: name.into(),
                matcher: LanguageMatcher {
                    path_suffixes: suffixes
                        .iter()
                        .map(|suffix| suffix.to_string().into())
                        .collect(),
                    ..LanguageMatcher::default()
                }
                .into(),
                ..LanguageConfig::default()
            },
            None,
        ))
    }

    async fn workspace_for(
        tree: serde_json::Value,
        cx: &mut TestAppContext,
    ) -> (gpui::Entity<Workspace>, &mut VisualTestContext) {
        workspace_with_server_for(tree, "clangd", cx).await
    }

    async fn workspace_with_server_for<'a>(
        tree: serde_json::Value,
        c_language_server: &'static str,
        cx: &'a mut TestAppContext,
    ) -> (gpui::Entity<Workspace>, &'a mut VisualTestContext) {
        cx.update(|cx| {
            workspace::AppState::test(cx);
            editor::init(cx);
            super::init(cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/"), tree).await;
        let project = Project::test(fs, [Path::new(path!("/work/project"))], cx).await;
        project.read_with(cx, |project, _| {
            project.languages().add(language("C", &["c", "h"]));
            project.languages().add(language("Rust", &["rs"]));
            project.languages().register_fake_lsp(
                "C",
                FakeLspAdapter {
                    name: c_language_server,
                    ..FakeLspAdapter::default()
                },
            );
        });
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace =
            multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
        cx.run_until_parked();
        (workspace, cx)
    }

    async fn open(workspace: &gpui::Entity<Workspace>, path: &str, cx: &mut VisualTestContext) {
        open_in(workspace, 0, path, cx).await
    }

    async fn open_in(
        workspace: &gpui::Entity<Workspace>,
        worktree_index: usize,
        path: &str,
        cx: &mut VisualTestContext,
    ) {
        let worktree_id = workspace.read_with(cx, |workspace, cx| {
            workspace
                .project()
                .read(cx)
                .worktrees(cx)
                .nth(worktree_index)
                .expect("a worktree")
                .read(cx)
                .id()
        });
        workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.open_path(
                    (worktree_id, util::rel_path::rel_path(path)),
                    None,
                    true,
                    window,
                    cx,
                )
            })
            .await
            .expect("the file opens");
        cx.run_until_parked();
    }

    fn notified(workspace: &gpui::Entity<Workspace>, cx: &mut VisualTestContext) -> usize {
        workspace.read_with(cx, |workspace, _| {
            workspace
                .notification_ids()
                .into_iter()
                .filter(|id| {
                    matches!(id, NotificationId::Composite(type_id, _)
                        if *type_id == std::any::TypeId::of::<MissingCompileCommands>())
                })
                .count()
        })
    }

    #[gpui::test]
    async fn test_a_c_file_without_compile_commands_is_noted_once(cx: &mut TestAppContext) {
        let (workspace, cx) = workspace_for(
            json!({ "work": { "project": {
                "src": { "main.c": "int main(void) { return 0; }\n", "other.c": "" },
                "lib.rs": "",
            } } }),
            cx,
        )
        .await;

        open(&workspace, "lib.rs", cx).await;
        assert_eq!(
            notified(&workspace, cx),
            0,
            "not for a file clangd does not serve"
        );

        open(&workspace, "src/main.c", cx).await;
        assert_eq!(notified(&workspace, cx), 1);

        workspace.update(cx, |workspace, cx| {
            let worktree_id = workspace
                .project()
                .read(cx)
                .worktrees(cx)
                .next()
                .expect("a worktree")
                .read(cx)
                .id();
            workspace.dismiss_notification(
                &NotificationId::composite::<MissingCompileCommands>(worktree_id.to_usize()),
                cx,
            )
        });
        open(&workspace, "src/other.c", cx).await;
        assert_eq!(notified(&workspace, cx), 0, "once per folder in a session");
    }

    async fn assert_quiet(tree: serde_json::Value, cx: &mut TestAppContext) {
        let (workspace, cx) = workspace_for(tree, cx).await;
        open(&workspace, "src/main.c", cx).await;
        assert_eq!(notified(&workspace, cx), 0);
    }

    #[gpui::test]
    async fn test_compile_commands_at_the_root_keep_it_quiet(cx: &mut TestAppContext) {
        assert_quiet(
            json!({ "work": { "project": {
                "src": { "main.c": "" }, "compile_commands.json": "[]",
            } } }),
            cx,
        )
        .await;
    }

    #[gpui::test]
    async fn test_compile_flags_next_to_the_file_keep_it_quiet(cx: &mut TestAppContext) {
        assert_quiet(
            json!({ "work": { "project": {
                "src": { "main.c": "", "compile_flags.txt": "-I." },
            } } }),
            cx,
        )
        .await;
    }

    #[gpui::test]
    async fn test_compile_commands_in_an_ignored_build_folder_keep_it_quiet(
        cx: &mut TestAppContext,
    ) {
        assert_quiet(
            json!({ "work": { "project": {
                ".git": {},
                ".gitignore": "build/\n",
                "src": { "main.c": "" },
                "build": { "compile_commands.json": "[]" },
            } } }),
            cx,
        )
        .await;
    }

    #[gpui::test]
    async fn test_compile_commands_above_the_project_keep_it_quiet(cx: &mut TestAppContext) {
        assert_quiet(
            json!({ "work": {
                "compile_commands.json": "[]",
                "project": { "src": { "main.c": "" } },
            } }),
            cx,
        )
        .await;
    }

    #[gpui::test]
    async fn test_a_compile_commands_dir_in_the_settings_keeps_it_quiet(cx: &mut TestAppContext) {
        let (workspace, cx) = workspace_for(
            json!({ "work": { "project": { "src": { "main.c": "" } } } }),
            cx,
        )
        .await;
        cx.update(|_, cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.project.lsp.0.insert(
                        "clangd".into(),
                        settings::LspSettings {
                            binary: Some(settings::BinarySettings {
                                arguments: Some(vec![
                                    "--compile-commands-dir=/elsewhere".to_string(),
                                ]),
                                ..settings::BinarySettings::default()
                            }),
                            ..settings::LspSettings::default()
                        },
                    );
                });
            });
        });
        open(&workspace, "src/main.c", cx).await;
        assert_eq!(notified(&workspace, cx), 0);
    }

    #[gpui::test]
    async fn test_a_compilation_database_path_in_the_settings_keeps_it_quiet(
        cx: &mut TestAppContext,
    ) {
        let (workspace, cx) = workspace_for(
            json!({ "work": { "project": { "src": { "main.c": "" } } } }),
            cx,
        )
        .await;
        cx.update(|_, cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.project.lsp.0.insert(
                        "clangd".into(),
                        settings::LspSettings {
                            initialization_options: Some(
                                json!({ "compilationDatabasePath": "/elsewhere" }),
                            ),
                            ..settings::LspSettings::default()
                        },
                    );
                });
            });
        });
        open(&workspace, "src/main.c", cx).await;
        assert_eq!(notified(&workspace, cx), 0);
    }

    #[gpui::test]
    async fn test_another_language_server_keeps_it_quiet(cx: &mut TestAppContext) {
        let (workspace, cx) = workspace_with_server_for(
            json!({ "work": { "project": { "src": { "main.c": "" } } } }),
            "ccls",
            cx,
        )
        .await;
        open(&workspace, "src/main.c", cx).await;
        assert_eq!(notified(&workspace, cx), 0);
    }

    #[gpui::test]
    async fn test_a_clangd_file_in_the_project_keeps_it_quiet(cx: &mut TestAppContext) {
        assert_quiet(
            json!({ "work": { "project": {
                "src": { "main.c": "" }, ".clangd": "CompileFlags:
  Add: [-Iinclude]
",
            } } }),
            cx,
        )
        .await;
    }

    #[gpui::test]
    async fn test_a_clangd_file_above_the_project_keeps_it_quiet(cx: &mut TestAppContext) {
        assert_quiet(
            json!({ "work": {
                ".clangd": "CompileFlags:
  CompilationDatabase: out
",
                "project": { "src": { "main.c": "" } },
            } }),
            cx,
        )
        .await;
    }

    #[gpui::test]
    async fn test_dont_show_again_keeps_it_away_from_other_folders(cx: &mut TestAppContext) {
        let (workspace, cx) = workspace_for(
            json!({ "work": {
                "project": { "src": { "main.c": "" } },
                "second": { "main.c": "" },
            } }),
            cx,
        )
        .await;
        open(&workspace, "src/main.c", cx).await;
        assert_eq!(notified(&workspace, cx), 1);

        cx.update(|_, cx| super::dont_show_again(cx));
        cx.run_until_parked();
        let project = workspace.read_with(cx, |workspace, _| workspace.project().clone());
        project
            .update(cx, |project, cx| {
                project.find_or_create_worktree(path!("/work/second"), true, cx)
            })
            .await
            .expect("the second folder opens");
        cx.run_until_parked();
        open_in(&workspace, 1, "main.c", cx).await;
        let second_noted = workspace.read_with(cx, |workspace, cx| {
            let worktree_id = workspace
                .project()
                .read(cx)
                .worktrees(cx)
                .nth(1)
                .expect("the second folder")
                .read(cx)
                .id();
            workspace.has_notification(&super::notification_id(worktree_id))
        });
        assert!(
            !second_noted,
            "not for another folder after Don't Show Again"
        );
    }
}
