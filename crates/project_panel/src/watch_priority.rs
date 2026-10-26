//! ADR-F104: the directories the panel shows expanded get native watches on
//! the host that holds the files before the rest of the worktree.

use std::{collections::BTreeSet, sync::Arc};

use gpui::Context;
use project::{GitEntry, ProjectEntryId, WorktreeId};
use util::rel_path::RelPath;

use crate::ProjectPanel;

impl ProjectPanel {
    pub(crate) fn update_watch_priority(&mut self, cx: &mut Context<Self>) {
        let mut changed = Vec::new();
        for visible in &self.state.visible_entries {
            let expanded_dir_ids = self
                .state
                .expanded_dir_ids
                .get(&visible.worktree_id)
                .map_or(&[][..], Vec::as_slice);
            let directories = expanded_visible_directories(&visible.entries, expanded_dir_ids);
            if self.watch_priority_directories.get(&visible.worktree_id) != Some(&directories) {
                self.watch_priority_directories
                    .insert(visible.worktree_id, directories.clone());
                changed.push((visible.worktree_id, directories));
            }
        }
        let shown = self
            .state
            .visible_entries
            .iter()
            .map(|visible| visible.worktree_id)
            .collect::<Vec<WorktreeId>>();
        self.watch_priority_directories
            .retain(|worktree_id, directories| {
                let keep = shown.contains(worktree_id);
                if !keep && !directories.is_empty() {
                    changed.push((*worktree_id, Vec::new()));
                }
                keep
            });
        if changed.is_empty() {
            return;
        }
        self.project.update(cx, |project, cx| {
            for (worktree_id, directories) in changed {
                project.set_panel_watch_priority(worktree_id, directories, cx);
            }
        });
    }
}

fn expanded_visible_directories(
    entries: &[GitEntry],
    expanded_dir_ids: &[ProjectEntryId],
) -> Vec<Arc<RelPath>> {
    let mut directories = BTreeSet::<Arc<RelPath>>::new();
    let mut last_parent = None;
    for entry in entries {
        if entry.is_dir() && expanded_dir_ids.binary_search(&entry.id).is_ok() {
            directories.insert(entry.path.clone());
        }
        // Every ancestor of a shown entry is expanded: shown as a row of its
        // own, folded into this entry's row, or the hidden root.
        let parent = entry.path.parent();
        if parent == last_parent {
            continue;
        }
        last_parent = parent;
        let mut ancestor = parent;
        while let Some(directory) = ancestor {
            if directories.contains(directory) {
                break;
            }
            directories.insert(directory.into_arc());
            ancestor = directory.parent();
        }
    }
    directories.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use gpui::{Entity, TestAppContext, VisualTestContext};
    use project::{FakeFs, Project, watch_priority::WATCH_PRIORITY_DEBOUNCE};
    use serde_json::json;
    use settings::Settings as _;
    use util::path;
    use workspace::MultiWorkspace;

    use crate::{
        ProjectPanel, ProjectPanelSettings,
        project_panel_tests::{init_test_with_editor, toggle_expand_dir},
    };

    fn watch_priority(project: &Entity<Project>, cx: &mut VisualTestContext) -> Vec<String> {
        cx.executor().advance_clock(WATCH_PRIORITY_DEBOUNCE);
        cx.run_until_parked();
        cx.update(|_, cx| {
            let worktree = project.read(cx).worktrees(cx).next().expect("a worktree");
            worktree
                .read(cx)
                .as_local()
                .expect("a local worktree")
                .watch_priority_paths()
                .iter()
                .map(|path| path.as_unix_str().to_owned())
                .collect()
        })
    }

    #[gpui::test]
    async fn test_expanded_visible_directories_get_watch_priority(cx: &mut TestAppContext) {
        init_test_with_editor(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                "a": { "b": { "file.txt": "" }, "other.txt": "" },
                "c": { "d": {} },
                "e.txt": "",
            }),
        )
        .await;
        let project = Project::test(fs, [path!("/root").as_ref()], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("a workspace");
        let cx = &mut VisualTestContext::from_window(window.into(), cx);
        let panel = workspace.update_in(cx, ProjectPanel::new);
        cx.run_until_parked();

        assert_eq!(watch_priority(&project, cx), [""]);

        toggle_expand_dir(&panel, "root/a", cx);
        assert_eq!(watch_priority(&project, cx), ["", "a"]);

        toggle_expand_dir(&panel, "root/a/b", cx);
        toggle_expand_dir(&panel, "root/c", cx);
        assert_eq!(watch_priority(&project, cx), ["", "a", "a/b", "c"]);

        // `a/b` stays expanded but is no longer shown.
        toggle_expand_dir(&panel, "root/a", cx);
        assert_eq!(watch_priority(&project, cx), ["", "c"]);

        toggle_expand_dir(&panel, "root", cx);
        assert_eq!(watch_priority(&project, cx), Vec::<String>::new());
    }

    #[gpui::test]
    async fn test_hidden_root_and_folded_directories_get_watch_priority(cx: &mut TestAppContext) {
        init_test_with_editor(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                "c": { "d": {} },
                "x": { "y.txt": "" },
            }),
        )
        .await;
        let project = Project::test(fs, [path!("/root").as_ref()], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("a workspace");
        let cx = &mut VisualTestContext::from_window(window.into(), cx);
        cx.update(|_, cx| {
            let settings = *ProjectPanelSettings::get_global(cx);
            ProjectPanelSettings::override_global(
                ProjectPanelSettings {
                    auto_fold_dirs: true,
                    hide_root: true,
                    ..settings
                },
                cx,
            );
        });
        let _panel = workspace.update_in(cx, ProjectPanel::new);
        cx.run_until_parked();

        // Neither the hidden root nor `c`, folded into the `c/d` row, has a row
        // of its own, but both show what is in them.
        assert_eq!(watch_priority(&project, cx), ["", "c"]);
    }
}
