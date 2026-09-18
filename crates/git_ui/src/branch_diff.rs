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
use collections::HashSet;
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
        workspace::register_serializable_item::<RefComparison>(cx);
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
            RefComparisonState::Changed {
                entries,
                omitted_count,
            } => {
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].path.as_unix_str(), "a.txt");
                assert!(matches!(entries[0].status, TreeDiffStatus::Modified { .. }));
                assert_eq!(*omitted_count, 0);
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
        // Picking a file asks the workspace to serialize the tab, and its
        // serialization loop keeps a handle to the item until it drains.
        cx.executor()
            .advance_clock(workspace::SERIALIZATION_THROTTLE_TIME);
        cx.run_until_parked();

        released_comparison.assert_released();
        released_editor.assert_released();
    }

    #[gpui::test]
    async fn test_ref_comparison_deserialize_restores_repository_refs_and_selection(
        cx: &mut TestAppContext,
    ) {
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
        let expected_repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });

        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(&*cx, |multi, _| multi.workspace().clone());
        let workspace_weak = workspace.downgrade();

        let item_id = workspace::ItemId::from(9001_u64);
        let workspace_db = cx.read(|cx| workspace::WorkspaceDb::global(cx));
        let workspace_id = workspace_db
            .next_id()
            .await
            .expect("should create workspace id");
        let db = cx.read(|cx| persistence::RefComparisonsDb::global(cx));
        db.save_ref_comparison(
            item_id,
            workspace_id,
            path!("/project").to_string(),
            "base".to_string(),
            "head".to_string(),
            Some("a.txt".to_string()),
        )
        .await
        .expect("save should succeed");

        let restored = cx
            .update(|window, cx| {
                <RefComparison as SerializableItem>::deserialize(
                    project.clone(),
                    workspace_weak,
                    workspace_id,
                    item_id,
                    window,
                    cx,
                )
            })
            .await
            .expect("deserialization should succeed");
        cx.run_until_parked();

        restored.read_with(cx, |comparison, _| {
            assert_eq!(
                comparison.repository.entity_id(),
                expected_repository.entity_id(),
                "the repository must be resolved back from its working path"
            );
            assert_eq!(comparison.base.as_ref(), "base");
            assert_eq!(comparison.head.as_ref(), "head");
            assert_eq!(
                comparison
                    .selected_path
                    .as_ref()
                    .map(|path| path.as_unix_str().to_string()),
                Some("a.txt".to_string())
            );
        });

        let file_diff_editor =
            restored.read_with(cx, |comparison, _| comparison.file_diff_editor.clone());
        assert!(
            file_diff_editor.is_some(),
            "the previously selected file should have reopened its diff"
        );
    }

    #[gpui::test]
    async fn test_ref_comparison_caps_the_file_list_and_reports_the_omitted_count(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "unrelated.txt": "kept out of the merge-base/head fakes below\n",
            }),
        )
        .await;

        // One past the cap, so an off-by-one in the cutoff (`>` vs `>=`, or the
        // truncate length) shows up as a wrong `entries.len()` or `omitted_count`.
        let file_count = MAX_REF_COMPARISON_ENTRIES + 1;
        let paths: Vec<String> = (0..file_count)
            .map(|index| format!("file-{index:05}.txt"))
            .collect();
        let base_entries: Vec<(&str, String)> = paths
            .iter()
            .map(|path| (path.as_str(), "old\n".to_string()))
            .collect();
        let head_entries: Vec<(&str, String)> = paths
            .iter()
            .map(|path| (path.as_str(), "new\n".to_string()))
            .collect();
        fs.set_merge_base_content_for_repo(path!("/project/.git").as_ref(), &base_entries);
        fs.set_head_and_index_for_repo(path!("/project/.git").as_ref(), &head_entries);

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

        comparison.read_with(cx, |comparison, _| match &comparison.state {
            RefComparisonState::Changed {
                entries,
                omitted_count,
            } => {
                assert_eq!(entries.len(), MAX_REF_COMPARISON_ENTRIES);
                assert_eq!(*omitted_count, file_count - MAX_REF_COMPARISON_ENTRIES);
            }
            _ => panic!("a large ref comparison must still report a changed-files list"),
        });
    }

    #[gpui::test]
    async fn test_ref_comparison_refuses_a_file_diff_over_the_size_guard(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "big.txt": "base\n",
            }),
        )
        .await;
        fs.set_head_and_index_for_repo(
            path!("/project/.git").as_ref(),
            &[("big.txt", "base\n".to_string())],
        );
        fs.set_merge_base_content_for_repo(
            path!("/project/.git").as_ref(),
            &[("big.txt", "different\n".to_string())],
        );
        let oversized_head_text = "a".repeat(MAX_REF_COMPARISON_FILE_BYTES + 1);
        for (args, stdout) in [
            ("show base:big.txt", "base\n".to_string()),
            ("show head:big.txt", oversized_head_text),
        ] {
            fs.with_git_state(path!("/project/.git").as_ref(), false, |state| {
                state.command_outputs.insert(
                    args.to_owned(),
                    git::repository::GitCommandOutput {
                        stdout,
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
        let big_file_path = RepoPath::from_rel_path(rel_path("big.txt"));
        comparison.update_in(cx, |comparison, window, cx| {
            comparison.open_file_diff(big_file_path.clone(), window, cx);
        });
        cx.run_until_parked();

        comparison.read_with(cx, |comparison, _| {
            assert_eq!(
                comparison.file_diff_error.as_deref(),
                Some("大きすぎるため表示できません。")
            );
            assert!(
                comparison.file_diff_editor.is_none(),
                "an oversized file must not build a diff editor"
            );
            assert!(
                comparison.unavailable_paths.contains(&big_file_path),
                "the file list should be able to mark the row once it's known to be unavailable"
            );
        });
    }

    /// The write side of the round trip: the previous test starts from rows a
    /// test wrote itself, so it cannot see `serialize` putting `head` in the
    /// `base` column, nor a picked file that never reaches the database.
    #[gpui::test]
    async fn test_ref_comparison_serialization_round_trips_the_picked_file(
        cx: &mut TestAppContext,
    ) {
        use std::{cell::RefCell, rc::Rc};

        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                // A multi-byte name under a directory: it goes through the
                // database as a unix path and comes back as a `RepoPath`.
                "資料": {
                    "設計メモ.txt": "base\n",
                },
            }),
        )
        .await;
        fs.set_head_and_index_for_repo(
            path!("/project/.git").as_ref(),
            &[("資料/設計メモ.txt", "head\n".to_string())],
        );
        fs.set_merge_base_content_for_repo(
            path!("/project/.git").as_ref(),
            &[("資料/設計メモ.txt", "base\n".to_string())],
        );
        for (args, stdout) in [
            ("show release-1.0:資料/設計メモ.txt", "base\n"),
            ("show topic:資料/設計メモ.txt", "head\n"),
        ] {
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
        let workspace_weak = workspace.downgrade();

        workspace.update_in(cx, |workspace, window, cx| {
            RefComparison::open(
                workspace,
                repository.clone(),
                "release-1.0".into(),
                "topic".into(),
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

        let events: Rc<RefCell<Vec<ItemEvent>>> = Rc::new(RefCell::new(Vec::new()));
        let event_subscription = cx.update(|_, cx| {
            cx.subscribe(&comparison, {
                let events = events.clone();
                move |_, event: &ItemEvent, _| events.borrow_mut().push(*event)
            })
        });
        comparison.update_in(cx, |comparison, window, cx| {
            comparison.open_file_diff(
                RepoPath::from_rel_path(rel_path("資料/設計メモ.txt")),
                window,
                cx,
            );
        });
        cx.run_until_parked();
        let emitted_events = events.borrow().clone();
        let asks_for_serialization = emitted_events.iter().any(|event| {
            comparison.read_with(cx, |comparison, _| comparison.should_serialize(event))
        });
        assert!(
            asks_for_serialization,
            "picking a file has to emit an event the workspace serializes on, or the pick is never saved"
        );
        drop(event_subscription);

        let item_id = workspace::ItemId::from(9101_u64);
        let workspace_db = cx.read(|cx| workspace::WorkspaceDb::global(cx));
        let workspace_id = workspace_db
            .next_id()
            .await
            .expect("should create workspace id");
        comparison
            .read_with(cx, |comparison, cx| {
                comparison.write_to_db(item_id, workspace_id, cx)
            })
            .await
            .expect("saving the comparison should succeed");

        let restored = cx
            .update(|window, cx| {
                <RefComparison as SerializableItem>::deserialize(
                    project.clone(),
                    workspace_weak,
                    workspace_id,
                    item_id,
                    window,
                    cx,
                )
            })
            .await
            .expect("deserialization should succeed");
        cx.run_until_parked();

        restored.read_with(cx, |restored, _| {
            assert_eq!(
                restored.base.as_ref(),
                "release-1.0",
                "the base ref must come back as the base ref, not as the head"
            );
            assert_eq!(restored.head.as_ref(), "topic");
            assert_eq!(
                restored
                    .selected_path
                    .as_ref()
                    .map(|path| path.as_unix_str().to_string()),
                Some("資料/設計メモ.txt".to_string()),
                "the file that was picked after the tab opened has to be saved too"
            );
        });
    }

    #[gpui::test]
    async fn test_ref_comparison_reports_a_file_whose_contents_cannot_be_read(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "modified.txt": "base\n",
            }),
        )
        .await;
        fs.set_merge_base_content_for_repo(
            path!("/project/.git").as_ref(),
            &[("modified.txt", "base\n".to_string())],
        );
        fs.set_head_and_index_for_repo(
            path!("/project/.git").as_ref(),
            &[
                ("modified.txt", "head\n".to_string()),
                ("added.txt", "added\n".to_string()),
            ],
        );
        // `show base:added.txt` stays unanswered on purpose: an added file has no
        // base side, and that must keep reading as an empty side.
        fs.with_git_state(path!("/project/.git").as_ref(), false, |state| {
            state.command_outputs.insert(
                "show head:added.txt".to_owned(),
                git::repository::GitCommandOutput {
                    stdout: "added\n".to_owned(),
                    stderr: String::new(),
                    exit_code: Some(0),
                },
            );
        })
        .expect("fake repository should exist");

        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        cx.run_until_parked();
        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });

        let (_multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
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

        let modified_path = RepoPath::from_rel_path(rel_path("modified.txt"));
        comparison.update_in(cx, |comparison, window, cx| {
            comparison.open_file_diff(modified_path.clone(), window, cx);
        });
        cx.run_until_parked();
        comparison.read_with(cx, |comparison, _| {
            assert_eq!(
                comparison.file_diff_error.as_deref(),
                Some("ファイルの中身を取得できませんでした。"),
                "a side git refused to print must not be shown as an empty file"
            );
            assert!(comparison.file_diff_editor.is_none());
            assert!(comparison.unavailable_paths.contains(&modified_path));
        });

        comparison.update_in(cx, |comparison, window, cx| {
            comparison.open_file_diff(RepoPath::from_rel_path(rel_path("added.txt")), window, cx);
        });
        cx.run_until_parked();
        comparison.read_with(cx, |comparison, _| {
            assert_eq!(comparison.file_diff_error, None);
            assert!(
                comparison.file_diff_editor.is_some(),
                "an added file still has to open with an empty base side"
            );
        });
    }

    #[gpui::test]
    async fn test_ref_comparison_opens_one_tab_per_repository(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/first"),
            json!({
                ".git": {},
                "a.txt": "first\n",
            }),
        )
        .await;
        fs.insert_tree(
            path!("/second"),
            json!({
                ".git": {},
                "a.txt": "second\n",
            }),
        )
        .await;
        for (dot_git, head) in [
            (path!("/first/.git"), "first head\n"),
            (path!("/second/.git"), "second head\n"),
        ] {
            fs.set_head_and_index_for_repo(dot_git.as_ref(), &[("a.txt", head.to_string())]);
            fs.set_merge_base_content_for_repo(
                dot_git.as_ref(),
                &[("a.txt", "base\n".to_string())],
            );
        }

        let project = Project::test(
            fs.clone(),
            [path!("/first").as_ref(), path!("/second").as_ref()],
            cx,
        )
        .await;
        cx.run_until_parked();
        let repositories = project.read_with(cx, |project, cx| {
            project
                .git_store()
                .read(cx)
                .repositories()
                .values()
                .cloned()
                .collect::<Vec<_>>()
        });
        assert_eq!(
            repositories.len(),
            2,
            "the project should hold one repository per worktree"
        );

        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(&*cx, |multi, _| multi.workspace().clone());

        for repository in &repositories {
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
        }
        let repositories_of_open_tabs = workspace.read_with(cx, |workspace, cx| {
            workspace
                .items_of_type::<RefComparison>(cx)
                .map(|item| item.read(cx).repository.entity_id())
                .collect::<HashSet<_>>()
        });
        assert_eq!(
            repositories_of_open_tabs.len(),
            2,
            "the same pair of ref names in another repository must open its own tab"
        );

        workspace.update_in(cx, |workspace, window, cx| {
            RefComparison::open(
                workspace,
                repositories[0].clone(),
                "base".into(),
                "head".into(),
                window,
                cx,
            );
        });
        cx.run_until_parked();
        let tab_count = workspace.read_with(cx, |workspace, cx| {
            workspace.items_of_type::<RefComparison>(cx).count()
        });
        assert_eq!(
            tab_count, 2,
            "asking for a comparison that is already open must reuse its tab"
        );
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

/// Above this many changed files, the list is truncated: rendering every row
/// (no virtualization) would otherwise stall the UI on a huge comparison.
const MAX_REF_COMPARISON_ENTRIES: usize = 2_000;

/// Above this size, a side of a file diff is not read into a buffer: both
/// sides are read whole into a `String`, so an unbounded file would stall
/// the UI thread building its diff.
const MAX_REF_COMPARISON_FILE_BYTES: usize = 2 * 1024 * 1024;

enum RefComparisonState {
    Loading,
    /// The two refs have the same tree, which is the answer the user is
    /// usually after: everything was ported.
    Identical,
    Changed {
        entries: Vec<RefComparisonEntry>,
        /// How many additional changed files were left out past
        /// [`MAX_REF_COMPARISON_ENTRIES`], so the list can say so instead of
        /// silently truncating.
        omitted_count: usize,
    },
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
    /// Files found (only once opened — `diff-tree` doesn't report size or
    /// binary-ness up front) to be binary or too large to show, so their row
    /// in the list can carry a marker.
    unavailable_paths: HashSet<RepoPath>,
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
        // The repository has to match too: a workspace with two repositories can
        // hold the same pair of ref names twice, and reusing the other
        // repository's tab would show its contents under this repository's name.
        let existing = workspace.items_of_type::<RefComparison>(cx).find(|item| {
            let item = item.read(cx);
            item.repository == repository && item.base == base && item.head == head
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
                    let omitted_count = entries.len().saturating_sub(MAX_REF_COMPARISON_ENTRIES);
                    entries.truncate(MAX_REF_COMPARISON_ENTRIES);
                    RefComparisonState::Changed {
                        entries,
                        omitted_count,
                    }
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
            unavailable_paths: HashSet::default(),
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
        self.unavailable_paths.remove(&path);
        cx.notify();
        // The pick is part of what gets serialized, and the workspace only asks
        // for that on an item event.
        cx.emit(ItemEvent::UpdateTab);

        // `git show <ref>:<path>` exits non-zero both for the side an added or a
        // deleted file is missing from and for a ref or a blob that cannot be
        // read at all. Only the file list says which of the two it is, so the
        // status is taken here rather than guessed from an empty side.
        let listed_status = match &self.state {
            RefComparisonState::Changed { entries, .. } => entries
                .iter()
                .find(|entry| entry.path == path)
                .map(|entry| entry.status.clone()),
            _ => None,
        };
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
                Ok(Ok(output)) if output.succeeded() => Some(output.stdout),
                _ => None,
            };
            let base_output = side(base_text.await);
            let head_output = side(head_text.await);
            let base_may_be_missing =
                listed_status.is_none() || listed_status == Some(TreeDiffStatus::Added);
            let head_may_be_missing = listed_status.is_none()
                || matches!(listed_status, Some(TreeDiffStatus::Deleted { .. }));
            if (base_output.is_none() && !base_may_be_missing)
                || (head_output.is_none() && !head_may_be_missing)
                // Whatever the status is, a file that is listed as changed has
                // contents on at least one of the two sides.
                || (base_output.is_none() && head_output.is_none())
            {
                this.update(cx, |this, cx| {
                    this.file_diff_editor = None;
                    this.file_diff_error = Some("ファイルの中身を取得できませんでした。".into());
                    this.unavailable_paths.insert(path);
                    cx.notify();
                })
                .ok();
                return;
            }
            let base_text = base_output.unwrap_or_default();
            let head_text = head_output.unwrap_or_default();
            if base_text.len() > MAX_REF_COMPARISON_FILE_BYTES
                || head_text.len() > MAX_REF_COMPARISON_FILE_BYTES
            {
                let path = path.clone();
                this.update(cx, |this, cx| {
                    this.file_diff_editor = None;
                    this.file_diff_error = Some("大きすぎるため表示できません。".into());
                    this.unavailable_paths.insert(path);
                    cx.notify();
                })
                .ok();
                return;
            }
            if base_text.contains('\0') || head_text.contains('\0') {
                let path = path.clone();
                this.update(cx, |this, cx| {
                    this.file_diff_editor = None;
                    this.file_diff_error = Some("バイナリファイルは並べて表示できません。".into());
                    this.unavailable_paths.insert(path);
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

    /// The write behind [`SerializableItem::serialize`], split out so that a
    /// test can drive the real column mapping with a workspace id of its own
    /// (a test workspace has no database id, so `serialize` would do nothing).
    fn write_to_db(
        &self,
        item_id: workspace::ItemId,
        workspace_id: workspace::WorkspaceId,
        cx: &App,
    ) -> Task<Result<()>> {
        let repo_working_path = self
            .repository
            .read(cx)
            .snapshot()
            .work_directory_abs_path
            .to_string_lossy()
            .to_string();
        let base = self.base.to_string();
        let head = self.head.to_string();
        let selected_path = self
            .selected_path
            .as_ref()
            .map(|path| path.as_unix_str().to_string());

        let db = persistence::RefComparisonsDb::global(cx);
        cx.background_spawn(async move {
            db.save_ref_comparison(
                item_id,
                workspace_id,
                repo_working_path,
                base,
                head,
                selected_path,
            )
            .await
        })
    }

    fn render_file_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let RefComparisonState::Changed {
            entries,
            omitted_count,
        } = &self.state
        else {
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
                let is_unavailable = self.unavailable_paths.contains(&entry.path);
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
                        h_flex()
                            .gap_1()
                            .when(is_unavailable, |this| {
                                this.child(
                                    Icon::new(IconName::Warning)
                                        .size(IconSize::XSmall)
                                        .color(Color::Muted),
                                )
                            })
                            .child(
                                Label::new(entry.status_label())
                                    .size(LabelSize::XSmall)
                                    .color(entry.status_color()),
                            ),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_file_diff(path.clone(), window, cx);
                    }))
            }))
            .when(*omitted_count > 0, |this| {
                this.child(
                    div().px_2().py_1().child(
                        Label::new(format!("ほか {omitted_count} 件は省略しました"))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
                )
            })
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
            RefComparisonState::Changed {
                entries,
                omitted_count,
            } => {
                let count = entries.len() + omitted_count;
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

impl SerializableItem for RefComparison {
    fn serialized_item_kind() -> &'static str {
        "RefComparison"
    }

    fn cleanup(
        workspace_id: workspace::WorkspaceId,
        alive_items: Vec<workspace::ItemId>,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<()>> {
        workspace::delete_unloaded_items(
            alive_items,
            workspace_id,
            "ref_comparisons",
            &persistence::RefComparisonsDb::global(cx),
            cx,
        )
    }

    fn deserialize(
        project: Entity<Project>,
        _workspace: WeakEntity<Workspace>,
        workspace_id: workspace::WorkspaceId,
        item_id: workspace::ItemId,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Entity<Self>>> {
        let db = persistence::RefComparisonsDb::global(cx);
        let Some((repo_working_path, base, head, selected_path)) =
            db.get_ref_comparison(item_id, workspace_id).ok().flatten()
        else {
            return Task::ready(Err(anyhow!("復元する比較タブが見つかりませんでした。")));
        };

        let window_handle = window.window_handle();
        let project_snapshot = project.read(cx);
        let git_store = project_snapshot.git_store().clone();
        let wait = project_snapshot.wait_for_initial_scan(cx);

        cx.spawn(async move |cx| {
            wait.await;

            cx.update_window(window_handle, |_, window, cx| {
                let path = repo_working_path.as_path();
                let repository = git_store
                    .read(cx)
                    .repositories()
                    .values()
                    .find(|repository| {
                        repository
                            .read(cx)
                            .snapshot()
                            .work_directory_abs_path
                            .as_ref()
                            == path
                    })
                    .cloned();
                let Some(repository) = repository else {
                    anyhow::bail!("リポジトリが見つかりませんでした: {path:?}");
                };

                let comparison = cx.new(|cx| {
                    RefComparison::new(repository, project.clone(), base.into(), head.into(), cx)
                });

                if let Some(selected_path) =
                    selected_path.and_then(|value| RepoPath::new(&value).ok())
                {
                    comparison.update(cx, |comparison, cx| {
                        comparison.open_file_diff(selected_path, window, cx);
                    });
                }

                Ok(comparison)
            })?
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
        Some(self.write_to_db(item_id, workspace.database_id()?, cx))
    }

    fn should_serialize(&self, event: &Self::Event) -> bool {
        matches!(event, ItemEvent::UpdateTab | ItemEvent::Edit)
    }
}

/// Persistence for [`RefComparison`]. Kept apart from [`project_diff::persistence`]
/// because a ref comparison identifies its repository and selection directly
/// (`repo_working_path` / `base` / `head` / `selected_path`), which doesn't fit
/// the `DiffBase`-shaped `project_diffs` table BranchDiff uses.
mod persistence {
    use std::path::PathBuf;

    use db::{
        query,
        sqlez::{domain::Domain, thread_safe_connection::ThreadSafeConnection},
        sqlez_macros::sql,
    };
    use workspace::WorkspaceDb;

    pub struct RefComparisonsDb(ThreadSafeConnection);

    impl Domain for RefComparisonsDb {
        const NAME: &str = stringify!(RefComparisonsDb);

        const MIGRATIONS: &[&str] = &[sql!(
            CREATE TABLE ref_comparisons(
                workspace_id INTEGER,
                item_id INTEGER UNIQUE,

                repo_working_path TEXT,
                base TEXT,
                head TEXT,
                selected_path TEXT,

                PRIMARY KEY(workspace_id, item_id),
                FOREIGN KEY(workspace_id) REFERENCES workspaces(workspace_id)
                ON DELETE CASCADE
            ) STRICT;
        )];
    }

    db::static_connection!(RefComparisonsDb, [WorkspaceDb]);

    impl RefComparisonsDb {
        query! {
            pub async fn save_ref_comparison(
                item_id: workspace::ItemId,
                workspace_id: workspace::WorkspaceId,
                repo_working_path: String,
                base: String,
                head: String,
                selected_path: Option<String>
            ) -> Result<()> {
                INSERT OR REPLACE INTO ref_comparisons(
                    item_id, workspace_id, repo_working_path, base, head, selected_path
                )
                VALUES (?, ?, ?, ?, ?, ?)
            }
        }

        query! {
            pub fn get_ref_comparison(
                item_id: workspace::ItemId,
                workspace_id: workspace::WorkspaceId
            ) -> Result<Option<(PathBuf, String, String, Option<String>)>> {
                SELECT repo_working_path, base, head, selected_path
                FROM ref_comparisons
                WHERE item_id = ? AND workspace_id = ?
            }
        }
    }
}
