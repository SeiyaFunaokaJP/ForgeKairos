use crate::{
    branch_picker,
    diff_multibuffer::DiffMultibuffer,
    project_diff::{
        self, CompareWithBranch, DeployBranchDiff, ProjectDiff, ReviewDiff,
        render_send_review_to_agent_button,
    },
};
use agent_settings::AgentSettings;
use anyhow::{Context as _, Result, anyhow};
use editor::{
    Addon, Editor, EditorEvent, RestoreOnlyDiffHunkDelegate, SplittableEditor,
    actions::SendReviewToAgent,
};
use git::{
    repository::{DiffType, RepoPath},
    status::{DiffTreeType, FileStatus, TreeDiffStatus},
};
use gpui::{
    Action, AnyElement, App, AppContext as _, Empty, Entity, EventEmitter, FocusHandle, Focusable,
    Render, SharedString, Subscription, Task, WeakEntity, rems,
};
use language::{Buffer, BufferId, Capability};
use multi_buffer::MultiBuffer;
use project::{
    Project, ProjectPath,
    git_store::{
        Repository,
        diff_buffer_list::{self, DiffBase},
    },
};
use settings::{GitDiffBaseSetting, Settings};
use std::{
    any::{Any, TypeId},
    sync::Arc,
};
use ui::{
    DiffStat, Divider, IconName, ListItem, ListItemSpacing, PopoverMenu, Tooltip, prelude::*,
};
use workspace::{
    ItemHandle, ItemNavHistory, SerializableItem, ToolbarItemEvent, ToolbarItemLocation,
    ToolbarItemView, Workspace,
    item::{Item, ItemEvent, SaveOptions, TabContentParams},
    notifications::NotifyTaskExt,
    searchable::SearchableItemHandle,
};
use zed_actions::agent::ReviewBranchDiff;

/// The workspace item for a branch (merge-base) diff: "Changes since {branch}".
/// It wraps a single [`DiffMultibuffer`] over [`DiffBase::Merge`] and delegates
/// the [`Item`] surface to it. The merge base can be changed in place via the
/// [`BranchDiffToolbar`]'s branch picker, which reloads without reconfiguring
/// the editor (the merge styling is identical for every base ref).
pub struct BranchDiff {
    diff: Entity<DiffMultibuffer>,
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    _diff_event_subscription: Subscription,
}

struct BranchDiffAddon {
    branch_diff: Entity<diff_buffer_list::DiffBufferList>,
}

impl Addon for BranchDiffAddon {
    fn to_any(&self) -> &dyn std::any::Any {
        self
    }

    fn override_status_for_buffer_id(&self, buffer_id: BufferId, cx: &App) -> Option<FileStatus> {
        self.branch_diff
            .read(cx)
            .status_for_buffer_id(buffer_id, cx)
    }
}

impl BranchDiff {
    pub(crate) fn register(workspace: &mut Workspace, cx: &mut Context<Workspace>) {
        workspace.register_action(|workspace, _: &DeployBranchDiff, window, cx| {
            Self::deploy_branch_diff(workspace, window, cx)
        });
        workspace.register_action(Self::compare_with_branch);
        workspace::register_serializable_item::<Self>(cx);
    }

    pub(crate) fn deploy_branch_diff(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        telemetry::event!("Git Branch Diff Opened");
        let project = workspace.project().clone();
        let Some(intended_repo) = project.read(cx).active_repository(cx) else {
            let workspace = cx.entity().downgrade();
            window
                .spawn(cx, async |_cx| {
                    let result: Result<()> = Err(anyhow!("アクティブなリポジトリがありません。"));
                    result
                })
                .detach_and_notify_err(workspace, window, cx);
            return;
        };

        let default_branch = intended_repo.update(cx, |repo, _| repo.default_branch(true));
        let workspace = cx.entity();
        let workspace_weak = workspace.downgrade();
        window
            .spawn(cx, async move |cx| {
                let base_ref = default_branch.await??;
                workspace.update_in(cx, |workspace, window, cx| {
                    let git_store = project.read(cx).git_store().clone();
                    let Some(base_ref) = base_ref else {
                        ProjectDiff::deploy_at(workspace, None, window, cx);
                        return;
                    };
                    let branch_diff =
                        if git_store.read(cx).diff_base() == GitDiffBaseSetting::DefaultBranch {
                            Some(git_store.update(cx, |git_store, cx| {
                                git_store.ensure_display_diff(
                                    intended_repo.clone(),
                                    base_ref.clone(),
                                    cx,
                                )
                            }))
                        } else {
                            None
                        };
                    Self::deploy_branch_diff_with_base_ref(
                        workspace,
                        project,
                        intended_repo,
                        base_ref,
                        branch_diff,
                        window,
                        cx,
                    );
                })?;

                anyhow::Ok(())
            })
            .detach_and_notify_err(workspace_weak, window, cx);
    }

    fn compare_with_branch(
        workspace: &mut Workspace,
        _: &CompareWithBranch,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let project = workspace.project().clone();
        let Some(repository) = project.read(cx).active_repository(cx) else {
            let workspace = cx.entity().downgrade();
            window
                .spawn(cx, async |_cx| {
                    let result: Result<()> = Err(anyhow!("アクティブなリポジトリがありません。"));
                    result
                })
                .detach_and_notify_err(workspace, window, cx);
            return;
        };
        let selected_branch = workspace.active_item_as::<Self>(cx).and_then(|item| {
            match item.read(cx).diff_base(cx) {
                DiffBase::Merge { base_ref } => Some(base_ref.clone()),
                DiffBase::Head | DiffBase::Index | DiffBase::Staged => None,
            }
        });
        let workspace_handle = workspace.weak_handle();
        let on_select = Arc::new({
            let repository = repository.clone();
            let workspace = workspace_handle.clone();
            move |branch: git::repository::Branch, window: &mut Window, cx: &mut App| {
                let base_ref: SharedString = branch.name().to_owned().into();
                workspace
                    .update(cx, |workspace, cx| {
                        Self::deploy_branch_diff_with_base_ref(
                            workspace,
                            project.clone(),
                            repository.clone(),
                            base_ref,
                            None,
                            window,
                            cx,
                        );
                    })
                    .ok();
            }
        });

        workspace.toggle_modal(window, cx, |window, cx| {
            branch_picker::select_modal(
                workspace_handle,
                Some(repository),
                selected_branch,
                on_select,
                window,
                cx,
            )
        });
    }

    pub(crate) fn deploy_branch_diff_with_base_ref(
        workspace: &mut Workspace,
        project: Entity<Project>,
        intended_repo: Entity<Repository>,
        base_ref: SharedString,
        branch_diff: Option<Entity<diff_buffer_list::DiffBufferList>>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let existing = workspace.items_of_type::<Self>(cx).find(|item| {
            let item = item.read(cx);
            matches!(
                item.diff_base(cx),
                DiffBase::Merge { base_ref: existing_base_ref } if existing_base_ref == &base_ref
            ) && item
                .repo(cx)
                .is_some_and(|repo| repo.read(cx).id == intended_repo.read(cx).id)
                && branch_diff.as_ref().is_none_or(|expected| {
                    item.diff.read(cx).branch_diff().entity_id() == expected.entity_id()
                })
        });
        if let Some(existing) = existing {
            workspace.activate_item(&existing, true, true, window, cx);
            return;
        }

        let workspace = cx.entity();
        let workspace_weak = workspace.downgrade();
        window
            .spawn(cx, async move |cx| {
                let this = cx
                    .update(|window, cx| {
                        Self::new_with_branch_base(
                            project,
                            workspace.clone(),
                            base_ref,
                            intended_repo,
                            branch_diff,
                            window,
                            cx,
                        )
                    })?
                    .await?;
                workspace
                    .update_in(cx, |workspace, window, cx| {
                        workspace.add_item_to_active_pane(Box::new(this), None, true, window, cx);
                    })
                    .ok();
                anyhow::Ok(())
            })
            .detach_and_notify_err(workspace_weak, window, cx);
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn new_with_default_branch(
        project: Entity<Project>,
        workspace: Entity<Workspace>,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Entity<Self>>> {
        let Some(repo) = project.read(cx).git_store().read(cx).active_repository() else {
            return Task::ready(Err(anyhow!("アクティブなリポジトリがありません。")));
        };
        let main_branch = repo.update(cx, |repo, _| repo.default_branch(true));
        window.spawn(cx, async move |cx| {
            let base_ref = main_branch
                .await??
                .context("Could not determine default branch")?;
            cx.update(|window, cx| {
                let git_store = project.read(cx).git_store().clone();
                let branch_diff =
                    if git_store.read(cx).diff_base() == GitDiffBaseSetting::DefaultBranch {
                        Some(git_store.update(cx, |git_store, cx| {
                            git_store.ensure_display_diff(repo.clone(), base_ref.clone(), cx)
                        }))
                    } else {
                        None
                    };
                cx.new(|cx| {
                    Self::new_with_base_ref(
                        project,
                        workspace,
                        base_ref,
                        Some(repo),
                        branch_diff,
                        window,
                        cx,
                    )
                })
            })
        })
    }

    pub(crate) fn new_with_branch_base(
        project: Entity<Project>,
        workspace: Entity<Workspace>,
        base_ref: SharedString,
        repo: Entity<Repository>,
        branch_diff: Option<Entity<diff_buffer_list::DiffBufferList>>,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Entity<Self>>> {
        window.spawn(cx, async move |cx| {
            cx.update(|window, cx| {
                cx.new(|cx| {
                    Self::new_with_base_ref(
                        project,
                        workspace,
                        base_ref,
                        Some(repo),
                        branch_diff,
                        window,
                        cx,
                    )
                })
            })
        })
    }

    pub(crate) fn new_with_base_ref(
        project: Entity<Project>,
        workspace: Entity<Workspace>,
        base_ref: SharedString,
        repo: Option<Entity<Repository>>,
        branch_diff: Option<Entity<diff_buffer_list::DiffBufferList>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let branch_diff = branch_diff.unwrap_or_else(|| {
            let git_store = project.read(cx).git_store().clone();
            cx.new(|cx| {
                diff_buffer_list::DiffBufferList::new(
                    DiffBase::Merge { base_ref },
                    git_store,
                    repo,
                    cx,
                )
            })
        });
        let branch_diff_for_addon = branch_diff.clone();
        let diff = cx.new(|cx| {
            DiffMultibuffer::new(
                branch_diff,
                Capability::ReadWrite,
                "変更はありません",
                move |editor, cx| {
                    editor.set_diff_hunk_delegate(Some(Arc::new(RestoreOnlyDiffHunkDelegate)), cx);
                    editor.rhs_editor().update(cx, move |rhs_editor, _cx| {
                        rhs_editor.set_read_only(false);
                        rhs_editor.register_addon(BranchDiffAddon {
                            branch_diff: branch_diff_for_addon,
                        });
                    });
                },
                project.clone(),
                workspace.clone(),
                window,
                cx,
            )
        });
        Self::from_diff(diff, project, workspace, cx)
    }

    fn from_diff(
        diff: Entity<DiffMultibuffer>,
        project: Entity<Project>,
        workspace: Entity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        let diff_event_subscription = cx.subscribe(&diff, |_, _, event: &EditorEvent, cx| {
            cx.emit(event.clone())
        });
        Self {
            diff,
            project,
            workspace: workspace.downgrade(),
            _diff_event_subscription: diff_event_subscription,
        }
    }

    pub(crate) fn diff_base<'a>(&'a self, cx: &'a App) -> &'a DiffBase {
        self.diff.read(cx).diff_base(cx)
    }

    pub(crate) fn repo(&self, cx: &App) -> Option<Entity<Repository>> {
        self.diff.read(cx).repo(cx)
    }

    fn set_merge_base(&mut self, base_ref: SharedString, cx: &mut Context<Self>) {
        self.diff.update(cx, |diff, cx| {
            diff.branch_diff().update(cx, |branch_diff, cx| {
                branch_diff.set_diff_base(DiffBase::Merge { base_ref }, cx);
            });
        });
    }

    fn review_diff(&mut self, _: &ReviewDiff, window: &mut Window, cx: &mut Context<Self>) {
        let DiffBase::Merge { base_ref } = self.diff_base(cx).clone() else {
            return;
        };
        let Some(repo) = self.repo(cx) else {
            return;
        };

        let diff_receiver = repo.update(cx, |repo, cx| {
            repo.diff(
                DiffType::MergeBase {
                    base_ref: base_ref.clone(),
                },
                cx,
            )
        });

        let workspace = self.workspace.clone();
        window
            .spawn(cx, {
                let workspace = workspace.clone();
                async move |cx| {
                    let diff_text = diff_receiver.await??;

                    if let Some(workspace) = workspace.upgrade() {
                        workspace.update_in(cx, |_workspace, window, cx| {
                            window.dispatch_action(
                                ReviewBranchDiff {
                                    diff_text: diff_text.into(),
                                    base_ref,
                                }
                                .boxed_clone(),
                                cx,
                            );
                        })?;
                    }

                    anyhow::Ok(())
                }
            })
            .detach_and_notify_err(workspace, window, cx);
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn editor(&self, cx: &App) -> Entity<SplittableEditor> {
        self.diff.read(cx).editor().clone()
    }
}

impl EventEmitter<EditorEvent> for BranchDiff {}

impl Focusable for BranchDiff {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.diff.read(cx).focus_handle(cx)
    }
}

impl Item for BranchDiff {
    type Event = EditorEvent;

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::GitBranch).color(Color::Muted))
    }

    fn to_item_events(event: &EditorEvent, f: &mut dyn FnMut(ItemEvent)) {
        Editor::to_item_events(event, f)
    }

    fn deactivated(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.diff
            .update(cx, |diff, cx| diff.deactivated(window, cx));
    }

    fn navigate(
        &mut self,
        data: Arc<dyn Any + Send>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.diff
            .update(cx, |diff, cx| diff.navigate(data, window, cx))
    }

    fn tab_tooltip_text(&self, cx: &App) -> Option<SharedString> {
        Some(self.tab_content_text(0, cx))
    }

    fn tab_content(&self, params: TabContentParams, _window: &Window, cx: &App) -> AnyElement {
        Label::new(self.tab_content_text(0, cx))
            .color(if params.selected {
                Color::Default
            } else {
                Color::Muted
            })
            .into_any_element()
    }

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        match self.diff_base(cx) {
            DiffBase::Merge { base_ref } => format!("{} 以降の変更", base_ref).into(),
            DiffBase::Head | DiffBase::Index | DiffBase::Staged => "変更".into(),
        }
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("Branch Diff Opened")
    }

    fn as_searchable(&self, _: &Entity<Self>, cx: &App) -> Option<Box<dyn SearchableItemHandle>> {
        Some(Box::new(self.diff.read(cx).editor().clone()))
    }

    fn for_each_project_item(
        &self,
        cx: &App,
        f: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem),
    ) {
        self.diff.read(cx).for_each_project_item(cx, f);
    }

    fn active_project_path(&self, cx: &App) -> Option<ProjectPath> {
        self.diff.read(cx).active_project_path(cx)
    }

    fn set_nav_history(
        &mut self,
        nav_history: ItemNavHistory,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.diff
            .update(cx, |diff, cx| diff.set_nav_history(nav_history, cx));
    }

    fn can_split(&self) -> bool {
        true
    }

    fn clone_on_split(
        &self,
        _workspace_id: Option<workspace::WorkspaceId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Option<Entity<Self>>>
    where
        Self: Sized,
    {
        let Some(workspace) = self.workspace.upgrade() else {
            return Task::ready(None);
        };
        let DiffBase::Merge { base_ref } = self.diff_base(cx).clone() else {
            return Task::ready(None);
        };
        let repo = self.repo(cx);
        let project = self.project.clone();
        let branch_diff = self.diff.read(cx).branch_diff().clone();
        Task::ready(Some(cx.new(|cx| {
            Self::new_with_base_ref(
                project,
                workspace,
                base_ref,
                repo,
                Some(branch_diff),
                window,
                cx,
            )
        })))
    }

    fn is_dirty(&self, cx: &App) -> bool {
        self.diff.read(cx).is_dirty(cx)
    }

    fn has_conflict(&self, cx: &App) -> bool {
        self.diff.read(cx).has_conflict(cx)
    }

    fn can_save(&self, _cx: &App) -> bool {
        true
    }

    fn save(
        &mut self,
        options: SaveOptions,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.diff
            .update(cx, |diff, cx| diff.save(options, project, window, cx))
    }

    fn save_as(
        &mut self,
        _: Entity<Project>,
        _: ProjectPath,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Task<Result<()>> {
        unreachable!()
    }

    fn reload(
        &mut self,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.diff
            .update(cx, |diff, cx| diff.reload(project, window, cx))
    }

    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        self_handle: &'a Entity<Self>,
        cx: &'a App,
    ) -> Option<gpui::AnyEntity> {
        if type_id == TypeId::of::<Self>() {
            Some(self_handle.clone().into())
        } else if type_id == TypeId::of::<DiffMultibuffer>() {
            Some(self.diff.clone().into())
        } else if type_id == TypeId::of::<Editor>() {
            Some(
                self.diff
                    .read(cx)
                    .editor()
                    .read(cx)
                    .rhs_editor()
                    .clone()
                    .into(),
            )
        } else if type_id == TypeId::of::<SplittableEditor>() {
            Some(self.diff.read(cx).editor().clone().into())
        } else if type_id == TypeId::of::<diff_buffer_list::DiffBufferList>() {
            Some(self.diff.read(cx).branch_diff().clone().into())
        } else {
            None
        }
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.diff.update(cx, |diff, cx| {
            diff.added_to_workspace(workspace, window, cx)
        });
    }
}

impl Render for BranchDiff {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .on_action(cx.listener(Self::review_diff))
            .child(self.diff.clone())
    }
}

impl SerializableItem for BranchDiff {
    fn serialized_item_kind() -> &'static str {
        "BranchDiff"
    }

    fn cleanup(
        _: workspace::WorkspaceId,
        _: Vec<workspace::ItemId>,
        _: &mut Window,
        _: &mut App,
    ) -> Task<Result<()>> {
        Task::ready(Ok(()))
    }

    fn deserialize(
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        workspace_id: workspace::WorkspaceId,
        item_id: workspace::ItemId,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Entity<Self>>> {
        let db = project_diff::persistence::ProjectDiffDb::global(cx);
        window.spawn(cx, async move |cx| {
            let diff_base = db.get_project_diff_base(item_id, workspace_id)?;
            let DiffBase::Merge { base_ref } = diff_base else {
                anyhow::bail!("expected a merge base for a branch diff");
            };
            let workspace = workspace.upgrade().context("workspace gone")?;
            cx.update(|window, cx| {
                cx.new(|cx| {
                    Self::new_with_base_ref(project, workspace, base_ref, None, None, window, cx)
                })
            })
        })
    }

    fn serialize(
        &mut self,
        workspace: &mut Workspace,
        item_id: workspace::ItemId,
        _closing: bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Task<Result<()>>> {
        let workspace_id = workspace.database_id()?;
        let DiffBase::Merge { base_ref } = self.diff_base(cx).clone() else {
            return None;
        };
        let diff_base = DiffBase::Merge { base_ref };
        let db = project_diff::persistence::ProjectDiffDb::global(cx);
        Some(cx.background_spawn(async move {
            db.save_project_diff_base(item_id, workspace_id, diff_base)
                .await
        }))
    }

    fn should_serialize(&self, _: &Self::Event) -> bool {
        false
    }
}

pub struct BranchDiffToolbar {
    branch_diff: Option<WeakEntity<BranchDiff>>,
}

impl BranchDiffToolbar {
    pub fn new(_cx: &mut Context<Self>) -> Self {
        Self { branch_diff: None }
    }

    fn branch_diff(&self, _: &App) -> Option<Entity<BranchDiff>> {
        self.branch_diff.as_ref()?.upgrade()
    }

    fn dispatch_action(&self, action: &dyn Action, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(branch_diff) = self.branch_diff(cx) {
            branch_diff.focus_handle(cx).focus(window, cx);
        }
        let action = action.boxed_clone();
        cx.defer(move |cx| {
            cx.dispatch_action(action.as_ref());
        })
    }
}

impl EventEmitter<ToolbarItemEvent> for BranchDiffToolbar {}

impl ToolbarItemView for BranchDiffToolbar {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> ToolbarItemLocation {
        self.branch_diff = active_pane_item
            .and_then(|item| item.act_as::<BranchDiff>(cx))
            .map(|entity| entity.downgrade());
        if self.branch_diff.is_some() {
            ToolbarItemLocation::PrimaryRight
        } else {
            ToolbarItemLocation::Hidden
        }
    }

    fn pane_focus_update(
        &mut self,
        _pane_focused: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }
}

impl Render for BranchDiffToolbar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(branch_diff) = self.branch_diff(cx) else {
            return div();
        };
        let focus_handle = branch_diff.focus_handle(cx);
        let review_count = branch_diff
            .read(cx)
            .diff
            .read(cx)
            .total_review_comment_count();
        let (additions, deletions) = branch_diff
            .read(cx)
            .diff
            .read(cx)
            .calculate_changed_lines(cx);
        let diff_base = branch_diff.read(cx).diff_base(cx).clone();
        let DiffBase::Merge { base_ref } = diff_base else {
            return div();
        };
        let selected_base_ref = base_ref.clone();
        let base_ref_label = format!("基準: {base_ref}");
        let repository = branch_diff.read(cx).repo(cx);
        let workspace = branch_diff.read(cx).workspace.clone();
        let project = branch_diff.read(cx).project.clone();
        let buffers = branch_diff.read(cx).diff.read(cx).branch_diff().clone();
        let uses_display_diff = repository.as_ref().is_some_and(|repository| {
            project
                .read(cx)
                .git_store()
                .read(cx)
                .display_diff_for_repo(repository.read(cx).id)
                .is_some_and(|display_diff| display_diff.entity_id() == buffers.entity_id())
        });
        let view_for_picker = branch_diff.downgrade();

        let is_multibuffer_empty = branch_diff
            .read(cx)
            .diff
            .read(cx)
            .multibuffer()
            .read(cx)
            .is_empty();
        let is_ai_enabled = AgentSettings::get_global(cx).enabled(cx);

        let show_review_button = !is_multibuffer_empty && is_ai_enabled;

        h_flex()
            .my_neg_1()
            .py_1()
            .gap_1p5()
            .flex_wrap()
            .justify_between()
            .when(!is_multibuffer_empty, |this| {
                this.child(DiffStat::new(
                    "branch-diff-stat",
                    additions as usize,
                    deletions as usize,
                ))
            })
            .child(Divider::vertical().ml_1())
            .child(
                PopoverMenu::new("branch-diff-base-branch-picker")
                    .menu(move |window, cx| {
                        let view_for_picker = view_for_picker.clone();
                        let workspace_for_select = workspace.clone();
                        let repository_for_select = repository.clone();
                        let project = project.clone();
                        let on_select = Arc::new(
                            move |branch: git::repository::Branch,
                                  window: &mut Window,
                                  cx: &mut App| {
                                let base_ref: SharedString = branch.name().to_owned().into();
                                if uses_display_diff {
                                    let Some(repository) = repository_for_select.clone() else {
                                        return;
                                    };
                                    workspace_for_select
                                        .update(cx, |workspace, cx| {
                                            BranchDiff::deploy_branch_diff_with_base_ref(
                                                workspace,
                                                project.clone(),
                                                repository,
                                                base_ref,
                                                None,
                                                window,
                                                cx,
                                            );
                                        })
                                        .ok();
                                } else {
                                    view_for_picker
                                        .update(cx, |branch_diff, cx| {
                                            branch_diff.set_merge_base(base_ref, cx);
                                            cx.notify();
                                        })
                                        .ok();
                                }
                            },
                        );

                        Some(branch_picker::select_popover(
                            workspace.clone(),
                            repository.clone(),
                            Some(selected_base_ref.clone()),
                            on_select,
                            window,
                            cx,
                        ))
                    })
                    .trigger_with_tooltip(
                        Button::new("branch-diff-base-branch", base_ref_label).end_icon(
                            Icon::new(IconName::ChevronDown)
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                        ),
                        Tooltip::text("基準ブランチを選択"),
                    ),
            )
            .when(show_review_button, |this| {
                let focus_handle = focus_handle.clone();
                this.child(Divider::vertical()).child(
                    Button::new("review-diff", "差分をレビュー")
                        .start_icon(
                            Icon::new(IconName::ZedAssistant)
                                .size(IconSize::Small)
                                .color(Color::Muted),
                        )
                        .tooltip(move |_, cx| {
                            Tooltip::with_meta_in(
                                "差分をレビュー",
                                Some(&ReviewDiff),
                                "この差分を直前のエージェントに送ってレビューしてもらいます。",
                                &focus_handle,
                                cx,
                            )
                        })
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.dispatch_action(&ReviewDiff, window, cx);
                        })),
                )
            })
            .when(review_count > 0, |this| {
                this.child(Divider::vertical()).child(
                    render_send_review_to_agent_button(review_count, &focus_handle).on_click(
                        cx.listener(|this, _, window, cx| {
                            this.dispatch_action(&SendReviewToAgent, window, cx)
                        }),
                    ),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use anyhow::anyhow;
    use collections::HashMap;
    use editor::test::editor_test_context::assert_state_with_diff;
    use git::status::{FileStatus, TrackedStatus, UnmergedStatus, UnmergedStatusCode};
    use gpui::TestAppContext;
    use project::FakeFs;
    use serde_json::json;
    use settings::{DiffViewStyle, SettingsStore};
    use std::path::Path;
    use std::sync::Arc;
    use unindent::Unindent as _;
    use util::{
        path,
        rel_path::{RelPath, rel_path},
    };
    use workspace::MultiWorkspace;

    use super::*;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.editor.diff_view_style = Some(DiffViewStyle::Unified);
                });
            });
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            crate::init(cx);
        });
    }

    #[gpui::test(iterations = 50)]
    async fn test_split_diff_conflict_path_transition_with_dirty_buffer_invalid_anchor_panics(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);

        cx.update(|cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.editor.diff_view_style = Some(DiffViewStyle::Split);
                });
            });
        });

        let build_conflict_text: fn(usize) -> String = |tag: usize| {
            let mut lines = (0..80)
                .map(|line_index| format!("line {line_index}"))
                .collect::<Vec<_>>();
            for offset in [5usize, 20, 37, 61] {
                lines[offset] = format!("base-{tag}-line-{offset}");
            }
            format!("{}\n", lines.join("\n"))
        };
        let initial_conflict_text = build_conflict_text(0);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "helper.txt": "same\n",
                "conflict.txt": initial_conflict_text,
            }),
        )
        .await;
        fs.with_git_state(path!("/project/.git").as_ref(), true, |state| {
            state
                .refs
                .insert("MERGE_HEAD".into(), "conflict-head".into());
        })
        .unwrap();
        fs.set_status_for_repo(
            path!("/project/.git").as_ref(),
            &[(
                "conflict.txt",
                FileStatus::Unmerged(UnmergedStatus {
                    first_head: UnmergedStatusCode::Updated,
                    second_head: UnmergedStatusCode::Updated,
                }),
            )],
        );
        fs.set_merge_base_content_for_repo(
            path!("/project/.git").as_ref(),
            &[
                ("conflict.txt", build_conflict_text(1)),
                ("helper.txt", "same\n".to_string()),
            ],
        );

        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let _branch_diff = cx
            .update(|window, cx| {
                BranchDiff::new_with_default_branch(project.clone(), workspace, window, cx)
            })
            .await
            .unwrap();
        cx.run_until_parked();

        let buffer = project
            .update(cx, |project, cx| {
                project.open_local_buffer(path!("/project/conflict.txt"), cx)
            })
            .await
            .unwrap();
        buffer.update(cx, |buffer, cx| buffer.edit([(0..0, "dirty\n")], None, cx));
        assert!(buffer.read_with(cx, |buffer, _| buffer.is_dirty()));
        cx.run_until_parked();

        cx.update(|window, cx| {
            let fs = fs.clone();
            window
                .spawn(cx, async move |cx| {
                    cx.background_executor().simulate_random_delay().await;
                    fs.with_git_state(path!("/project/.git").as_ref(), true, |state| {
                        state.refs.insert("HEAD".into(), "head-1".into());
                        state.refs.remove("MERGE_HEAD");
                    })
                    .unwrap();
                    fs.set_status_for_repo(
                        path!("/project/.git").as_ref(),
                        &[
                            (
                                "conflict.txt",
                                FileStatus::Tracked(TrackedStatus {
                                    index_status: git::status::StatusCode::Modified,
                                    worktree_status: git::status::StatusCode::Modified,
                                }),
                            ),
                            (
                                "helper.txt",
                                FileStatus::Tracked(TrackedStatus {
                                    index_status: git::status::StatusCode::Modified,
                                    worktree_status: git::status::StatusCode::Modified,
                                }),
                            ),
                        ],
                    );
                    // FakeFs assigns deterministic OIDs by entry position; flipping order churns
                    // conflict diff identity without reaching into view internals.
                    fs.set_merge_base_content_for_repo(
                        path!("/project/.git").as_ref(),
                        &[
                            ("helper.txt", "helper-base\n".to_string()),
                            ("conflict.txt", build_conflict_text(2)),
                        ],
                    );
                })
                .detach();
        });

        cx.update(|window, cx| {
            let buffer = buffer.clone();
            window
                .spawn(cx, async move |cx| {
                    cx.background_executor().simulate_random_delay().await;
                    for edit_index in 0..10 {
                        if edit_index > 0 {
                            cx.background_executor().simulate_random_delay().await;
                        }
                        buffer.update(cx, |buffer, cx| {
                            let len = buffer.len();
                            if edit_index % 2 == 0 {
                                buffer.edit(
                                    [(0..0, format!("status-burst-head-{edit_index}\n"))],
                                    None,
                                    cx,
                                );
                            } else {
                                buffer.edit(
                                    [(len..len, format!("status-burst-tail-{edit_index}\n"))],
                                    None,
                                    cx,
                                );
                            }
                        });
                    }
                })
                .detach();
        });

        cx.run_until_parked();
    }

    #[gpui::test]
    async fn test_branch_diff(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "a.txt": "C",
                "b.txt": "new",
                "c.txt": "in-merge-base-and-work-tree",
                "d.txt": "created-in-head",
            }),
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let diff = cx
            .update(|window, cx| {
                BranchDiff::new_with_default_branch(project.clone(), workspace, window, cx)
            })
            .await
            .unwrap();
        cx.run_until_parked();

        fs.set_head_for_repo(
            Path::new(path!("/project/.git")),
            &[("a.txt", "B".into()), ("d.txt", "created-in-head".into())],
            "sha",
        );
        fs.set_merge_base_content_for_repo(
            Path::new(path!("/project/.git")),
            &[
                ("a.txt", "A".into()),
                ("c.txt", "in-merge-base-and-work-tree".into()),
            ],
        );
        cx.run_until_parked();

        let editor = diff.read_with(cx, |diff, cx| diff.editor(cx).read(cx).rhs_editor().clone());

        let expected_diff = "
                - A
                + C
                + ˇnew
                + created-in-head"
            .unindent();
        assert_state_with_diff(&editor, cx, &expected_diff);

        cx.update(|window, cx| {
            editor.focus_handle(cx).focus(window, cx);
            window.dispatch_action(git::Restore.boxed_clone(), cx);
        });
        cx.run_until_parked();
        assert_state_with_diff(&editor, cx, &expected_diff);

        let statuses: HashMap<Arc<RelPath>, Option<FileStatus>> =
            editor.update(cx, |editor, cx| {
                editor
                    .buffer()
                    .read(cx)
                    .all_buffers()
                    .iter()
                    .map(|buffer| {
                        (
                            buffer.read(cx).file().unwrap().path().clone(),
                            editor.status_for_buffer_id(buffer.read(cx).remote_id(), cx),
                        )
                    })
                    .collect()
            });

        assert_eq!(
            statuses,
            HashMap::from_iter([
                (
                    rel_path("a.txt").into_arc(),
                    Some(FileStatus::Tracked(TrackedStatus {
                        index_status: git::status::StatusCode::Modified,
                        worktree_status: git::status::StatusCode::Modified
                    }))
                ),
                (rel_path("b.txt").into_arc(), Some(FileStatus::Untracked)),
                (
                    rel_path("d.txt").into_arc(),
                    Some(FileStatus::Tracked(TrackedStatus {
                        index_status: git::status::StatusCode::Added,
                        worktree_status: git::status::StatusCode::Added
                    }))
                )
            ])
        );
    }

    #[gpui::test]
    async fn test_branch_diff_action_matches_existing_item_by_base_ref(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "a.txt": "changed",
            }),
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        let target_branch_diff = cx
            .update(|window, cx| {
                let Some(repository) = project.read(cx).active_repository(cx) else {
                    return Task::ready(Err(anyhow!("アクティブなリポジトリがありません。")));
                };
                BranchDiff::new_with_branch_base(
                    project.clone(),
                    workspace.clone(),
                    "topic".into(),
                    repository,
                    None,
                    window,
                    cx,
                )
            })
            .await
            .unwrap();
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(
                Box::new(target_branch_diff.clone()),
                None,
                true,
                window,
                cx,
            );
        });
        cx.run_until_parked();

        cx.focus(&workspace);
        cx.update(|window, cx| {
            window.dispatch_action(DeployBranchDiff.boxed_clone(), cx);
        });
        cx.run_until_parked();

        let (active_base_ref, mut base_refs) = workspace.update(cx, |workspace, cx| {
            let active_item = workspace.active_item_as::<BranchDiff>(cx).unwrap();
            let active_base_ref = match active_item.read(cx).diff_base(cx) {
                DiffBase::Merge { base_ref } => base_ref.to_string(),
                DiffBase::Head | DiffBase::Index | DiffBase::Staged => {
                    panic!("expected active item to be a branch diff")
                }
            };
            let base_refs = workspace
                .items_of_type::<BranchDiff>(cx)
                .filter_map(|item| match item.read(cx).diff_base(cx) {
                    DiffBase::Merge { base_ref } => Some(base_ref.to_string()),
                    DiffBase::Head | DiffBase::Index | DiffBase::Staged => None,
                })
                .collect::<Vec<_>>();
            (active_base_ref, base_refs)
        });
        base_refs.sort();

        assert_eq!(active_base_ref, "origin/main");
        assert_eq!(base_refs, vec!["origin/main", "topic"]);
    }

    #[gpui::test]
    async fn test_ref_comparison_reports_identical_and_changed_trees(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "a.txt": "same
            ",
            }),
        )
        .await;
        fs.set_head_and_index_for_repo(
            path!("/project/.git").as_ref(),
            &[(
                "a.txt",
                "same
"
                .to_string(),
            )],
        );
        fs.set_merge_base_content_for_repo(
            path!("/project/.git").as_ref(),
            &[(
                "a.txt",
                "same
"
                .to_string(),
            )],
        );

        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        cx.run_until_parked();
        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });

        let comparison = cx.new(|cx| {
            RefComparison::new(
                repository.clone(),
                project.clone(),
                "base".into(),
                "head".into(),
                cx,
            )
        });
        cx.run_until_parked();
        comparison.read_with(cx, |comparison, _| {
            assert!(
                matches!(comparison.state, RefComparisonState::Identical),
                "two refs with the same tree must report that everything was ported"
            );
        });

        fs.set_merge_base_content_for_repo(
            path!("/project/.git").as_ref(),
            &[(
                "a.txt",
                "different
"
                .to_string(),
            )],
        );
        let comparison = cx.new(|cx| {
            RefComparison::new(
                repository.clone(),
                project.clone(),
                "base".into(),
                "head".into(),
                cx,
            )
        });
        cx.run_until_parked();
        comparison.read_with(cx, |comparison, _| match &comparison.state {
            RefComparisonState::Changed(entries) => {
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].path.as_unix_str(), "a.txt");
                assert!(matches!(entries[0].status, TreeDiffStatus::Modified { .. }));
            }
            _ => panic!("a file that differs between the refs must be listed"),
        });
    }

    #[gpui::test]
    async fn test_ref_comparison_is_released_when_its_tab_closes(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "a.txt": "base\n",
            }),
        )
        .await;
        fs.set_head_and_index_for_repo(
            path!("/project/.git").as_ref(),
            &[("a.txt", "base\n".to_string())],
        );
        fs.set_merge_base_content_for_repo(
            path!("/project/.git").as_ref(),
            &[("a.txt", "different\n".to_string())],
        );
        for (args, stdout) in [("show base:a.txt", "base\n"), ("show head:a.txt", "head\n")] {
            fs.with_git_state(path!("/project/.git").as_ref(), false, |state| {
                state.command_outputs.insert(
                    args.to_owned(),
                    git::repository::GitCommandOutput {
                        stdout: stdout.to_owned(),
                        stderr: String::new(),
                        exit_code: Some(0),
                    },
                );
            })
            .expect("fake repository should exist");
        }

        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        cx.run_until_parked();
        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });

        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(&*cx, |multi, _| multi.workspace().clone());

        workspace.update_in(cx, |workspace, window, cx| {
            RefComparison::open(
                workspace,
                repository.clone(),
                "base".into(),
                "head".into(),
                window,
                cx,
            );
        });
        cx.run_until_parked();

        let comparison = workspace.read_with(cx, |workspace, cx| {
            workspace
                .items_of_type::<RefComparison>(cx)
                .next()
                .expect("the comparison should have opened as an item")
        });
        comparison.update_in(cx, |comparison, window, cx| {
            comparison.open_file_diff(RepoPath::from_rel_path(rel_path("a.txt")), window, cx);
        });
        cx.run_until_parked();

        let released_editor = comparison.read_with(cx, |comparison, _| {
            comparison
                .file_diff_editor
                .as_ref()
                .expect("picking a file should have built an editor")
                .downgrade()
        });
        let released_comparison = comparison.downgrade();
        drop(comparison);

        let close = workspace.update_in(cx, |workspace, window, cx| {
            workspace.active_pane().update(cx, |pane, cx| {
                pane.close_active_item(&workspace::CloseActiveItem::default(), window, cx)
            })
        });
        close.await.expect("closing the comparison tab should work");
        cx.run_until_parked();

        released_comparison.assert_released();
        released_editor.assert_released();
    }
}

/// One changed file between the two refs.
struct RefComparisonEntry {
    path: RepoPath,
    status: TreeDiffStatus,
}

impl RefComparisonEntry {
    fn status_label(&self) -> &'static str {
        match self.status {
            TreeDiffStatus::Added => "追加",
            TreeDiffStatus::Modified { .. } => "変更",
            TreeDiffStatus::Deleted { .. } => "削除",
        }
    }

    fn status_color(&self) -> Color {
        match self.status {
            TreeDiffStatus::Added => Color::Created,
            TreeDiffStatus::Modified { .. } => Color::Modified,
            TreeDiffStatus::Deleted { .. } => Color::Deleted,
        }
    }
}

enum RefComparisonState {
    Loading,
    /// The two refs have the same tree, which is the answer the user is
    /// usually after: everything was ported.
    Identical,
    Changed(Vec<RefComparisonEntry>),
    Failed(SharedString),
}

/// Compares the contents of two refs — "did everything make it across?".
///
/// This cannot go through [`diff_buffer_list::DiffBufferList`], which always
/// diffs a ref against the working tree; here both sides are commits, so the
/// file list comes from `diff-tree <base> <head>` and each side of a file is
/// read out of git directly.
pub struct RefComparison {
    focus_handle: FocusHandle,
    repository: Entity<Repository>,
    project: Entity<Project>,
    base: SharedString,
    head: SharedString,
    state: RefComparisonState,
    selected_path: Option<RepoPath>,
    file_diff_editor: Option<Entity<Editor>>,
    /// Why the picked file could not be shown. Kept apart from `state` so that
    /// one unshowable file does not take the whole file list down with it.
    file_diff_error: Option<SharedString>,
    _load_task: Task<()>,
    _file_task: Option<Task<()>>,
}

impl RefComparison {
    pub fn open(
        workspace: &mut Workspace,
        repository: Entity<Repository>,
        base: SharedString,
        head: SharedString,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        if base == head {
            return;
        }
        let existing = workspace.items_of_type::<RefComparison>(cx).find(|item| {
            let item = item.read(cx);
            item.base == base && item.head == head
        });
        if let Some(existing) = existing {
            workspace.activate_item(&existing, true, true, window, cx);
            return;
        }
        let project = workspace.project().clone();
        let comparison = cx.new(|cx| RefComparison::new(repository, project, base, head, cx));
        workspace.add_item_to_active_pane(Box::new(comparison), None, true, window, cx);
    }

    fn new(
        repository: Entity<Repository>,
        project: Entity<Project>,
        base: SharedString,
        head: SharedString,
        cx: &mut Context<Self>,
    ) -> Self {
        let tree = repository.update(cx, |repository, cx| {
            repository.diff_tree(
                DiffTreeType::Since {
                    base: base.clone(),
                    head: head.clone(),
                },
                cx,
            )
        });
        let load_task = cx.spawn(async move |this, cx| {
            let state = match tree.await {
                Ok(Ok(tree)) if tree.entries.is_empty() => RefComparisonState::Identical,
                Ok(Ok(tree)) => {
                    let mut entries = tree
                        .entries
                        .into_iter()
                        .map(|(path, status)| RefComparisonEntry { path, status })
                        .collect::<Vec<_>>();
                    entries.sort_by(|left, right| left.path.cmp(&right.path));
                    RefComparisonState::Changed(entries)
                }
                Ok(Err(error)) => RefComparisonState::Failed(format!("{error:#}").into()),
                Err(_) => return,
            };
            this.update(cx, |this, cx| {
                this.state = state;
                cx.notify();
            })
            .ok();
        });

        Self {
            focus_handle: cx.focus_handle(),
            repository,
            project,
            base,
            head,
            state: RefComparisonState::Loading,
            selected_path: None,
            file_diff_editor: None,
            file_diff_error: None,
            _load_task: load_task,
            _file_task: None,
        }
    }

    /// Loads both sides of one file and shows them as a diff. A side that does
    /// not exist at its ref (an added or deleted file) reads as empty, which is
    /// exactly how git presents it.
    fn open_file_diff(&mut self, path: RepoPath, window: &mut Window, cx: &mut Context<Self>) {
        self.selected_path = Some(path.clone());
        self.file_diff_editor = None;
        self.file_diff_error = None;
        cx.notify();

        let path_text = path.as_unix_str().to_string();
        let (base_text, head_text) = self.repository.update(cx, |repository, _cx| {
            (
                repository.file_at_ref(self.base.to_string(), path_text.clone()),
                repository.file_at_ref(self.head.to_string(), path_text.clone()),
            )
        });
        let project = self.project.clone();
        self._file_task = Some(cx.spawn_in(window, async move |this, cx| {
            let side = |output: Result<Result<git::repository::GitCommandOutput>, _>| match output {
                Ok(Ok(output)) if output.succeeded() => output.stdout,
                _ => String::new(),
            };
            let base_text = side(base_text.await);
            let head_text = side(head_text.await);
            if base_text.contains('\0') || head_text.contains('\0') {
                this.update(cx, |this, cx| {
                    this.file_diff_editor = None;
                    this.file_diff_error = Some("バイナリファイルは並べて表示できません。".into());
                    cx.notify();
                })
                .ok();
                return;
            }

            let language_registry =
                project.read_with(cx, |project, _cx| project.languages().clone());
            let language = language_registry
                .load_language_for_file_path(path.as_std_path())
                .await
                .ok();

            this.update_in(cx, |this, window, cx| {
                let build_buffer = |text: String, cx: &mut App| {
                    cx.new(|cx| {
                        let mut buffer = Buffer::local(text, cx);
                        buffer.set_language_registry(language_registry.clone());
                        buffer.set_language(language.clone(), cx);
                        buffer.set_capability(Capability::ReadOnly, cx);
                        buffer
                    })
                };
                let base_buffer = build_buffer(base_text.clone(), cx);
                let head_buffer = build_buffer(head_text, cx);
                let head_snapshot = head_buffer.read(cx).snapshot();
                let base_snapshot = base_buffer.read(cx).snapshot();
                let diff = cx.new(|cx| {
                    buffer_diff::BufferDiff::new_with_base_text_buffer(
                        &head_snapshot.text,
                        base_buffer,
                        buffer_diff::DiffBaseKind::Custom,
                        cx,
                    )
                });
                let multibuffer = cx.new(|cx| {
                    let mut multibuffer = MultiBuffer::singleton(head_buffer, cx);
                    multibuffer.add_diff(diff.clone(), cx);
                    multibuffer
                });
                let project = this.project.clone();
                let editor = cx.new(|cx| {
                    let mut editor =
                        Editor::for_multibuffer(multibuffer, Some(project), window, cx);
                    editor.set_read_only(true);
                    editor.set_expand_all_diff_hunks(cx);
                    editor
                });
                this.file_diff_editor = Some(editor);
                cx.spawn(async move |_this, cx| {
                    let update = diff.update(cx, |diff, cx| {
                        diff.update_diff(
                            head_snapshot.text.clone(),
                            &base_snapshot,
                            Some(Arc::<str>::from(base_text)),
                            cx,
                        )
                    });
                    let update = update.await;
                    diff.update(cx, |diff, cx| diff.set_snapshot(update, cx));
                })
                .detach();
                cx.notify();
            })
            .ok();
        }));
    }

    fn render_file_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let RefComparisonState::Changed(entries) = &self.state else {
            return Empty.into_any_element();
        };
        v_flex()
            .id("ref-comparison-files")
            .h_full()
            .w(rems(20.))
            .flex_none()
            .overflow_y_scroll()
            .border_r_1()
            .border_color(cx.theme().colors().border_variant)
            .children(entries.iter().enumerate().map(|(index, entry)| {
                let path = entry.path.clone();
                let is_selected = self.selected_path.as_ref() == Some(&entry.path);
                let label = entry
                    .path
                    .file_name()
                    .map(|name| name.to_string())
                    .unwrap_or_else(|| entry.path.as_unix_str().to_string());
                ListItem::new(("ref-comparison-file", index))
                    .spacing(ListItemSpacing::Sparse)
                    .toggle_state(is_selected)
                    .tooltip({
                        let full_path = entry.path.as_unix_str().to_string();
                        move |_, cx| Tooltip::simple(full_path.clone(), cx)
                    })
                    .child(Label::new(label).truncate())
                    .end_slot(
                        Label::new(entry.status_label())
                            .size(LabelSize::XSmall)
                            .color(entry.status_color()),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_file_diff(path.clone(), window, cx);
                    }))
            }))
            .into_any_element()
    }
}

impl EventEmitter<ItemEvent> for RefComparison {}

impl Focusable for RefComparison {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for RefComparison {
    type Event = ItemEvent;

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<ui::Icon> {
        Some(ui::Icon::new(IconName::Diff))
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        format!("{} と {} の比較", self.base, self.head).into()
    }

    fn show_toolbar(&self) -> bool {
        false
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

impl Render for RefComparison {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let header = v_flex()
            .w_full()
            .p_2()
            .gap_0p5()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(Label::new(format!(
                "{} (移植元) と {} (移植先) の比較",
                self.base, self.head
            )))
            .child(
                Label::new(
                    "2 つのブランチ / タグの中身そのものを比べます。差分が無ければ、すべて移植できています。",
                )
                .size(LabelSize::Small)
                .color(Color::Muted),
            );

        let body = match &self.state {
            RefComparisonState::Loading => h_flex()
                .size_full()
                .justify_center()
                .child(
                    Label::new("比較を読み込み中…")
                        .color(Color::Muted)
                        .size(LabelSize::Large),
                )
                .into_any_element(),
            RefComparisonState::Identical => v_flex()
                .size_full()
                .justify_center()
                .items_center()
                .gap_1()
                .child(
                    Label::new("差分はありません (全部移植できています)")
                        .color(Color::Success)
                        .size(LabelSize::Large),
                )
                .child(
                    Label::new(format!("{} と {} は同じ中身です。", self.base, self.head))
                        .color(Color::Muted)
                        .size(LabelSize::Small),
                )
                .into_any_element(),
            RefComparisonState::Failed(error) => v_flex()
                .size_full()
                .justify_center()
                .items_center()
                .gap_1()
                .child(
                    Label::new("比較できませんでした")
                        .color(Color::Error)
                        .size(LabelSize::Large),
                )
                .child(
                    Label::new(error.clone())
                        .color(Color::Muted)
                        .size(LabelSize::Small),
                )
                .into_any_element(),
            RefComparisonState::Changed(entries) => {
                let count = entries.len();
                h_flex()
                    .size_full()
                    .child(self.render_file_list(cx))
                    .child(match (self.file_diff_editor.clone(), &self.file_diff_error) {
                        (_, Some(error)) => h_flex()
                            .flex_1()
                            .size_full()
                            .justify_center()
                            .child(Label::new(error.clone()).color(Color::Muted))
                            .into_any_element(),
                        (Some(editor), None) => {
                            div().flex_1().size_full().child(editor).into_any_element()
                        }
                        (None, None) => v_flex()
                            .flex_1()
                            .size_full()
                            .justify_center()
                            .items_center()
                            .gap_1()
                            .child(
                                Label::new(format!(
                                    "{count} 個のファイルに差分があります。左の一覧から選ぶと中身を並べて表示します。"
                                ))
                                .color(Color::Muted),
                            )
                            // `diff-tree` runs with `--no-renames`, so a renamed
                            // file shows up twice and the count looks inflated.
                            .child(
                                Label::new(
                                    "名前を変えたファイルは「削除」と「追加」の 2 行に分かれます。",
                                )
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                            )
                            .into_any_element(),
                    })
                    .into_any_element()
            }
        };

        v_flex()
            .key_context("RefComparison")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(header)
            .child(div().flex_1().min_h_0().child(body))
    }
}
