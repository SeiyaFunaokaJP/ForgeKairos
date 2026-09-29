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
use collections::{HashMap, HashSet};
use editor::{
    Addon, Editor, EditorEvent, HiddenDiffHunkRenderer, SplittableEditor,
    actions::SendReviewToAgent,
};
use futures::{FutureExt as _, future::Shared};
use git::{
    repository::{DiffType, RepoPath},
    status::{DiffTreeType, FileStatus, TreeDiff, TreeDiffStatus},
};
use gpui::{
    Action, AnyElement, App, AppContext as _, AsyncApp, Empty, Entity, EventEmitter, FocusHandle,
    Focusable, Render, SharedString, Subscription, Task, WeakEntity, rems,
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
use util::ResultExt as _;
use workspace::{
    ItemHandle, ItemNavHistory, SerializableItem, ToolbarItemEvent, ToolbarItemLocation,
    ToolbarItemView, Workspace,
    item::{Item, ItemEvent, SaveOptions},
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
                    let result: Result<()> = Err(anyhow!(i18n::t!("No active repository")));
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
                    let result: Result<()> = Err(anyhow!(i18n::t!("No active repository")));
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
            return Task::ready(Err(anyhow!(i18n::t!("No active repository"))));
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
                i18n::t!("No changes"),
                move |editor, cx| {
                    editor.set_diff_hunk_renderer(Some(Arc::new(HiddenDiffHunkRenderer)), cx);
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

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        match self.diff_base(cx) {
            DiffBase::Merge { base_ref } => i18n::t!("Changes since {}", base_ref).into(),
            DiffBase::Head | DiffBase::Index | DiffBase::Staged => i18n::t!("Changes").into(),
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
        let base_ref_label = i18n::t!("Base: {base_ref}");
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
                        Tooltip::text(i18n::t!("Select Base Branch")),
                    ),
            )
            .when(show_review_button, |this| {
                let focus_handle = focus_handle.clone();
                this.child(Divider::vertical()).child(
                    Button::new("review-diff", i18n::t!("Review Diff"))
                        .start_icon(
                            Icon::new(IconName::ZedAssistant)
                                .size(IconSize::Small)
                                .color(Color::Muted),
                        )
                        .tooltip(move |_, cx| {
                            Tooltip::with_meta_in(
                                i18n::t!("Review Diff"),
                                Some(&ReviewDiff),
                                i18n::t!("Send this diff for your last agent to review."),
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
            // Without a database of its own, every test in the process shares one, and two
            // tests saving comparisons at once fail with "database table is locked".
            cx.set_global(db::AppDatabase::test_new());
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
                    return Task::ready(Err(anyhow!("No active repository")));
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
        // The last handle now goes away in a task rather than inside an update
        // (upstream v1.20), and a dropped entity is only removed at the end of
        // the next effect cycle.
        cx.update(|_, _| {});

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
                Some("Too large to show.")
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
                Some("Could not get the file's contents."),
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

    fn tree_diff(entries: &[(&str, TreeDiffStatus)]) -> TreeDiff {
        TreeDiff {
            entries: entries
                .iter()
                .map(|(path, status)| (git::repository::repo_path(path), status.clone()))
                .collect(),
        }
    }

    fn test_oid(digit: char) -> git::Oid {
        std::iter::repeat_n(digit, 40)
            .collect::<String>()
            .parse()
            .expect("a 40-digit hex string is a SHA-1")
    }

    fn row_verdicts(rows: &[MergeRequestCheckRow]) -> Vec<(String, MergeRequestFileVerdict)> {
        rows.iter()
            .map(|row| (row.path.as_unix_str().to_string(), row.verdict))
            .collect()
    }

    #[test]
    fn test_classify_merge_request_files_compares_only_what_the_source_changed() {
        let modified = || TreeDiffStatus::Modified { old: test_oid('1') };
        let deleted = || TreeDiffStatus::Deleted { old: test_oid('2') };
        let source_changes = tree_diff(&[
            ("matched.txt", modified()),
            ("gone.txt", deleted()),
            ("missing.txt", TreeDiffStatus::Added),
            ("base_too.txt", modified()),
            ("excluded.txt", TreeDiffStatus::Added),
            ("both.txt", modified()),
            // Both branches changed these two, and the merge request has the source's version.
            ("dropped.txt", modified()),
            ("agreed.txt", modified()),
            // Both branches changed it, and the merge request has the base's version.
            ("base_version.txt", modified()),
        ]);
        let base_changes = tree_diff(&[
            ("base_too.txt", modified()),
            ("both.txt", modified()),
            ("base_only.txt", TreeDiffStatus::Added),
            ("dropped.txt", modified()),
            ("agreed.txt", modified()),
            ("base_version.txt", modified()),
        ]);
        let source_to_base_version = TreeDiffStatus::Modified { old: test_oid('4') };
        let source_to_merge_request = tree_diff(&[
            ("missing.txt", deleted()),
            ("base_too.txt", modified()),
            ("excluded.txt", deleted()),
            // Absent from `base_to_merge_request` as well, but an excluded commit touched it.
            ("both.txt", modified()),
            ("base_version.txt", source_to_base_version.clone()),
            // The base's own change, which the merge request has and the source does not.
            ("base_only.txt", TreeDiffStatus::Added),
        ]);
        let base_to_dropped = TreeDiffStatus::Modified { old: test_oid('3') };
        let base_to_merge_request = tree_diff(&[
            ("matched.txt", modified()),
            ("base_too.txt", modified()),
            ("resolved.txt", modified()),
            // The base's version is not the source's, so the merge request would undo it;
            // `agreed.txt` is absent: the base made the same change as the source.
            ("dropped.txt", base_to_dropped.clone()),
        ]);
        let excluded_paths = ["excluded.txt", "both.txt"]
            .into_iter()
            .map(git::repository::repo_path)
            .collect::<HashSet<_>>();

        let (rows, counts) = classify_merge_request_files(
            &source_changes,
            &base_changes,
            &source_to_merge_request,
            &base_to_merge_request,
            &excluded_paths,
        );

        let explained = |changed_on_base, changed_by_excluded| MergeRequestFileVerdict::Explained {
            changed_on_base,
            changed_by_excluded,
        };
        assert_eq!(
            row_verdicts(&rows),
            vec![
                (
                    "base_version.txt".to_string(),
                    MergeRequestFileVerdict::SuspectedMissing
                ),
                (
                    "missing.txt".to_string(),
                    MergeRequestFileVerdict::SuspectedMissing
                ),
                (
                    "dropped.txt".to_string(),
                    MergeRequestFileVerdict::BaseChangeMaybeDropped
                ),
                ("base_too.txt".to_string(), explained(true, false)),
                ("both.txt".to_string(), explained(true, true)),
                ("excluded.txt".to_string(), explained(false, true)),
                (
                    "resolved.txt".to_string(),
                    MergeRequestFileVerdict::OnlyInMergeRequest
                ),
                (
                    "agreed.txt".to_string(),
                    MergeRequestFileVerdict::Matched { deleted: false }
                ),
                (
                    "gone.txt".to_string(),
                    MergeRequestFileVerdict::Matched { deleted: true }
                ),
                (
                    "matched.txt".to_string(),
                    MergeRequestFileVerdict::Matched { deleted: false }
                ),
            ],
            "the base's own changes must not be listed, and each verdict must group together"
        );
        assert_eq!(
            counts,
            MergeRequestCheckCounts {
                source_files: 9,
                matched: 3,
                mismatched: 5,
                suspected_missing: 2,
                changed_on_base: 2,
                changed_by_excluded: 2,
                base_change_maybe_dropped: 1,
                only_in_merge_request: 1,
            }
        );
        let status_of = |path: &str| {
            rows.iter()
                .find(|row| row.path.as_unix_str() == path)
                .and_then(|row| row.status.clone())
        };
        assert_eq!(
            status_of("missing.txt"),
            Some(deleted()),
            "a mismatched row opens the development branch against the merge request"
        );
        assert_eq!(
            status_of("resolved.txt"),
            Some(modified()),
            "a merge-request-only row opens the base against the merge request"
        );
        assert_eq!(
            status_of("dropped.txt"),
            Some(base_to_dropped),
            "a row that may drop the base's change opens the base against the merge request"
        );
        assert_eq!(
            status_of("base_version.txt"),
            Some(source_to_base_version),
            "a file whose change is missing opens the development branch against the merge request"
        );
        assert_eq!(status_of("matched.txt"), None);
        assert_eq!(
            rows.iter()
                .filter(|row| row.merge_request_has_base_version)
                .map(|row| row.path.as_unix_str().to_string())
                .collect::<Vec<_>>(),
            vec!["base_version.txt".to_string()],
            "only the file left at the base's version carries the note"
        );
    }

    #[test]
    fn test_merge_request_check_summary_and_verdict() {
        let all_ported = MergeRequestCheckCounts {
            source_files: 12,
            matched: 11,
            mismatched: 1,
            changed_by_excluded: 1,
            ..MergeRequestCheckCounts::default()
        };
        assert_eq!(
            all_ported.summary(),
            "12 files changed on the development branch: 11 matching / 1 not matching (1 excluded)"
        );
        assert_eq!(
            all_ported.verdict(),
            ("Nothing is missing".to_string(), Color::Success)
        );

        let missing = MergeRequestCheckCounts {
            source_files: 5,
            matched: 3,
            mismatched: 2,
            suspected_missing: 1,
            changed_on_base: 1,
            only_in_merge_request: 2,
            ..MergeRequestCheckCounts::default()
        };
        assert_eq!(
            missing.summary(),
            "5 files changed on the development branch: 3 matching / 2 not matching (1 possibly missing, 1 also changed on the base). 2 changed files not from the development branch"
        );
        assert_eq!(
            missing.verdict(),
            ("1 file possibly missing".to_string(), Color::Error)
        );

        let only_in_merge_request = MergeRequestCheckCounts {
            only_in_merge_request: 1,
            ..MergeRequestCheckCounts::default()
        };
        assert_eq!(
            only_in_merge_request.verdict(),
            (
                "Nothing is missing, but 1 file needs a look".to_string(),
                Color::Warning
            ),
            "a change the development branch never had is not 'nothing missing'"
        );

        let base_change_maybe_dropped = MergeRequestCheckCounts {
            source_files: 3,
            matched: 2,
            base_change_maybe_dropped: 1,
            only_in_merge_request: 2,
            ..MergeRequestCheckCounts::default()
        };
        assert_eq!(
            base_change_maybe_dropped.summary(),
            "3 files changed on the development branch: 2 matching / 0 not matching / 1 base change possibly missing. 2 changed files not from the development branch"
        );
        assert_eq!(
            base_change_maybe_dropped.verdict(),
            (
                "Nothing is missing, but 3 files need a look".to_string(),
                Color::Warning
            ),
            "a merge request that may undo the base's change is not 'nothing missing'"
        );
        assert_eq!(
            MergeRequestCheckCounts {
                base_change_maybe_dropped: 1,
                ..MergeRequestCheckCounts::default()
            }
            .verdict()
            .1,
            Color::Warning
        );
        assert_eq!(
            MergeRequestCheckCounts {
                suspected_missing: 1,
                base_change_maybe_dropped: 1,
                ..MergeRequestCheckCounts::default()
            }
            .verdict(),
            ("1 file possibly missing".to_string(), Color::Error),
            "a file suspected missing outranks the warnings"
        );
    }

    #[test]
    fn test_merge_request_behind_its_base_is_counted_and_explained() {
        assert_eq!(
            base_commits_ahead_args("origin/main", "mr"),
            vec![
                "log".to_string(),
                "--format=%H".to_string(),
                format!("--max-count={}", MAX_COUNTED_BASE_COMMITS + 1),
                "mr..origin/main".to_string(),
                "--".to_string(),
            ],
            "the commits the base has and the merge request lacks, not the other way round"
        );
        git::repository::validate_git_command(&base_commits_ahead_args("origin/main", "mr"))
            .expect("the count has to pass the allow-list that a remote server applies");
        assert_eq!(count_listed_commits(""), 0);
        assert_eq!(
            count_listed_commits(&format!("{}\n{}\n", "a".repeat(40), "b".repeat(40))),
            2
        );

        assert_eq!(merge_request_behind_notice(0), None);
        assert_eq!(
            merge_request_behind_notice(3).as_deref(),
            Some(
                "The MR branch was not created from the tip of its base (the base is 3 commits ahead)"
            )
        );
        assert_eq!(
            merge_request_behind_notice(MAX_COUNTED_BASE_COMMITS).as_deref(),
            Some(
                "The MR branch was not created from the tip of its base (the base is 1000 commits ahead)"
            )
        );
        assert_eq!(
            merge_request_behind_notice(MAX_COUNTED_BASE_COMMITS + 1).as_deref(),
            Some(
                "The MR branch was not created from the tip of its base (the base is at least 1000 commits ahead)"
            )
        );

        let behind = MergeRequestFileVerdict::OnlyInMergeRequest.description(true);
        let up_to_date = MergeRequestFileVerdict::OnlyInMergeRequest.description(false);
        assert!(
            behind.contains("not created from the tip of its base"),
            "{behind}"
        );
        assert!(
            !up_to_date.contains("not created from the tip of its base"),
            "a merge request built from the base's tip must not be blamed for lagging: {up_to_date}"
        );
    }

    /// The base of a merge-request check is asked for even when only one branch qualifies: taken
    /// without asking, the user would not know which branch the check measured against.
    #[gpui::test]
    async fn test_merge_request_base_prompt_asks_even_for_one(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), json!({ ".git": {}, "a.txt": "a" }))
            .await;
        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let picker_open = |cx: &mut gpui::VisualTestContext| {
            workspace.read_with(cx, |workspace, cx| {
                workspace
                    .active_modal::<crate::picker_prompt::PickerPrompt>(cx)
                    .is_some()
            })
        };

        let taken = cx.update(|window, cx| {
            crate::picker_prompt::prompt(
                "Base",
                vec!["main".into()],
                workspace.downgrade(),
                window,
                cx,
            )
        });
        cx.run_until_parked();
        assert!(!picker_open(cx));
        assert_eq!(
            taken.await,
            Some(0),
            "the plain prompt takes the only option"
        );

        let _asked = cx.update(|window, cx| {
            crate::picker_prompt::prompt_even_for_one(
                "MR's Base (Merge Target) Branch",
                vec!["main".into()],
                workspace.downgrade(),
                window,
                cx,
            )
        });
        cx.run_until_parked();
        assert!(picker_open(cx), "one candidate is still asked about");
    }

    #[test]
    fn test_parse_commits_with_paths_reads_git_log_z_output() {
        let newest = "a".repeat(40);
        let merge = "b".repeat(40);
        let oldest = "c".repeat(40);
        let empty = "d".repeat(40);
        // What git 2.47 prints, subjects holding the separators of the old format included.
        let output = format!(
            "\0{newest}\0dev \u{1e}two\u{1f}\0\nd1.txt\0資料/日本 語.txt\0\
             \0{merge}\0Merge branch 'main' into dev\0\
             \0{empty}\0\0\
             \0{oldest}\0dev one\0\nd1.txt\0"
        );
        let commits = parse_commits_with_paths(&output);
        assert_eq!(
            commits
                .iter()
                .map(|commit| (
                    commit.commit.sha.to_string(),
                    commit.commit.subject.to_string(),
                    commit
                        .paths
                        .iter()
                        .map(|path| path.as_unix_str().to_string())
                        .collect::<Vec<_>>()
                ))
                .collect::<Vec<_>>(),
            vec![
                (
                    newest,
                    "dev \u{1e}two\u{1f}".to_string(),
                    vec!["d1.txt".to_string(), "資料/日本 語.txt".to_string()]
                ),
                (
                    merge,
                    "Merge branch 'main' into dev".to_string(),
                    Vec::new()
                ),
                (empty, String::new(), Vec::new()),
                (oldest, "dev one".to_string(), vec!["d1.txt".to_string()]),
            ]
        );
        assert!(parse_commits_with_paths("").is_empty());
    }

    #[test]
    fn test_parse_single_merge_base() {
        let merge_base = "d".repeat(40);
        let tips = format!("{}\n{}\n", "e".repeat(40), "f".repeat(40));
        assert_eq!(
            parse_single_merge_base(&format!("{tips}^{merge_base}\n"), "main", "dev")
                .expect("one merge base")
                .to_string(),
            merge_base
        );
        let unrelated = parse_single_merge_base(&tips, "main", "dev")
            .expect_err("unrelated histories have no merge base");
        assert!(
            unrelated.to_string().contains("no common ancestor"),
            "{unrelated}"
        );
        let criss_cross = parse_single_merge_base(
            &format!("{tips}^{merge_base}\n^{}\n", "0".repeat(40)),
            "main",
            "dev",
        )
        .expect_err("two merge bases leave the starting point ambiguous");
        assert!(
            criss_cross.to_string().contains("have 2 common ancestors"),
            "{criss_cross}"
        );
    }

    #[test]
    fn test_attach_commits_lists_the_commits_behind_each_missing_file() {
        let (mut rows, _) = classify_merge_request_files(
            &tree_diff(&[
                ("missing.txt", TreeDiffStatus::Added),
                ("explained.txt", TreeDiffStatus::Added),
            ]),
            &tree_diff(&[("explained.txt", TreeDiffStatus::Added)]),
            &tree_diff(&[
                (
                    "missing.txt",
                    TreeDiffStatus::Deleted { old: test_oid('1') },
                ),
                (
                    "explained.txt",
                    TreeDiffStatus::Deleted { old: test_oid('2') },
                ),
            ]),
            // The merge request's `explained.txt` is not the base's either, so the base moving
            // on explains the difference.
            &tree_diff(&[(
                "explained.txt",
                TreeDiffStatus::Modified { old: test_oid('3') },
            )]),
            &HashSet::default(),
        );
        let history = ['1', '2', '3', '4', '5', '6', '7']
            .into_iter()
            .map(|digit| CommitWithPaths {
                commit: MergeRequestCommit {
                    sha: test_oid(digit),
                    subject: format!("commit {digit}").into(),
                },
                paths: vec![
                    git::repository::repo_path("missing.txt"),
                    git::repository::repo_path("explained.txt"),
                ],
            })
            .collect::<Vec<_>>();
        attach_commits_to_missing_files(&mut rows, &history);

        let missing = rows
            .iter()
            .find(|row| row.path.as_unix_str() == "missing.txt")
            .expect("the missing file should be listed");
        assert_eq!(
            missing
                .commits
                .iter()
                .map(|commit| commit.subject.to_string())
                .collect::<Vec<_>>(),
            ["1", "2", "3", "4", "5"].map(|digit| format!("commit {digit}")),
            "the newest commits come first, up to the cap"
        );
        assert_eq!(missing.omitted_commit_count, 2);
        let explained = rows
            .iter()
            .find(|row| row.path.as_unix_str() == "explained.txt")
            .expect("the explained file should be listed");
        assert!(
            explained.commits.is_empty(),
            "only a file suspected missing names its commits"
        );
    }

    #[test]
    fn test_paths_changed_by_excluded_reports_commits_it_could_not_find() {
        let history = vec![
            CommitWithPaths {
                commit: MergeRequestCommit {
                    sha: test_oid('1'),
                    subject: "kept".into(),
                },
                paths: vec![git::repository::repo_path("kept.txt")],
            },
            CommitWithPaths {
                commit: MergeRequestCommit {
                    sha: test_oid('2'),
                    subject: "left out".into(),
                },
                paths: vec![git::repository::repo_path("left-out.txt")],
            },
        ];
        let (paths, not_found) =
            paths_changed_by_excluded(&history, &[test_oid('2'), test_oid('9')]);
        assert_eq!(
            paths,
            HashSet::from_iter([git::repository::repo_path("left-out.txt")])
        );
        assert_eq!(not_found, vec![test_oid('9')]);
        assert_eq!(
            normalize_excluded_commits(vec![test_oid('9'), test_oid('2'), test_oid('9')]),
            vec![test_oid('2'), test_oid('9')]
        );
    }

    #[test]
    fn test_old_server_refusal_is_explained() {
        let refused = explain_old_server_refusal(anyhow!("unsupported git command: rev-parse"));
        assert!(
            refused.to_string().contains("too old for the MR check"),
            "{refused}"
        );
        let other = explain_old_server_refusal(anyhow!("fatal: bad revision"));
        assert_eq!(other.to_string(), "fatal: bad revision");
    }

    #[test]
    fn test_merge_request_base_candidates_leave_out_the_compared_refs() {
        let branch = |ref_name: &str| git::repository::Branch {
            is_head: false,
            ref_name: ref_name.to_string().into(),
            upstream: None,
            most_recent_commit: None,
        };
        let branches = [
            branch("refs/remotes/origin/main"),
            branch("refs/heads/dev"),
            branch("refs/heads/main"),
            branch("refs/heads/mr"),
        ];
        assert_eq!(
            merge_request_base_candidates(&branches, "dev", "mr"),
            vec![
                SharedString::from("main"),
                SharedString::from("origin/main")
            ]
        );
    }

    #[gpui::test]
    async fn test_merge_request_check_persists_its_mode_and_reloads(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "a.txt": "a\n",
            }),
        )
        .await;
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

        let excluded = vec![test_oid('b'), test_oid('a')];
        let comparison = cx.new(|cx| {
            RefComparison::new_with_mode(
                repository.clone(),
                project.clone(),
                "dev".into(),
                "mr".into(),
                RefComparisonMode::MergeRequestCheck {
                    merge_request_base: "origin/main".into(),
                    excluded: normalize_excluded_commits(excluded.clone()),
                },
                cx,
            )
        });
        cx.run_until_parked();

        let item_id = workspace::ItemId::from(9201_u64);
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
            .expect("saving the check should succeed");
        let merge_base_lookups = || {
            fs.with_git_state(path!("/project/.git").as_ref(), false, |state| {
                state
                    .run_command_log
                    .iter()
                    .filter(|(args, _)| {
                        args.first().map(String::as_str) == Some("rev-parse")
                            && args.get(1).map(String::as_str) == Some("origin/main...dev")
                    })
                    .count()
            })
            .expect("fake repository should exist")
        };
        let lookups_before = merge_base_lookups();
        assert_eq!(lookups_before, 1);

        let restored = cx
            .update(|window, cx| {
                <RefComparison as SerializableItem>::deserialize(
                    project.clone(),
                    workspace.downgrade(),
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
            assert_eq!(restored.base.as_ref(), "dev");
            assert_eq!(restored.head.as_ref(), "mr");
            assert_eq!(
                restored.mode,
                RefComparisonMode::MergeRequestCheck {
                    merge_request_base: "origin/main".into(),
                    excluded: normalize_excluded_commits(excluded),
                },
                "the base and the excluded commits must come back, or what was left out on purpose turns into suspects"
            );
        });
        assert_eq!(
            merge_base_lookups(),
            lookups_before + 1,
            "the restored check must be recomputed from git, not read back from the database"
        );

        let plain = cx.new(|cx| {
            RefComparison::new(
                repository.clone(),
                project.clone(),
                "dev".into(),
                "mr".into(),
                cx,
            )
        });
        plain
            .read_with(cx, |plain, cx| plain.write_to_db(item_id, workspace_id, cx))
            .await
            .expect("saving the plain comparison should succeed");
        let restored_plain = cx
            .update(|window, cx| {
                <RefComparison as SerializableItem>::deserialize(
                    project.clone(),
                    workspace.downgrade(),
                    workspace_id,
                    item_id,
                    window,
                    cx,
                )
            })
            .await
            .expect("deserialization should succeed");
        restored_plain.read_with(cx, |restored, _| {
            assert_eq!(
                restored.mode,
                RefComparisonMode::Refs,
                "saving a plain comparison over the same item must not leave it a merge-request check"
            );
        });
    }

    #[allow(clippy::disallowed_methods)]
    #[track_caller]
    fn git_in(directory: &Path, arguments: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(arguments)
            .current_dir(directory)
            .env("GIT_CONFIG_GLOBAL", "")
            .env("GIT_CONFIG_SYSTEM", "")
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@zed.dev")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@zed.dev")
            .output()
            .expect("git should run");
        assert!(
            output.status.success(),
            "git {arguments:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("git output should be UTF-8")
            .trim()
            .to_string()
    }

    #[track_caller]
    fn commit_files(directory: &Path, files: &[(&str, Option<&str>)], message: &str) -> String {
        for (path, contents) in files {
            let full_path = directory.join(path);
            match contents {
                Some(contents) => std::fs::write(&full_path, contents).expect("write file"),
                None => std::fs::remove_file(&full_path).expect("remove file"),
            }
        }
        git_in(directory, &["add", "-A"]);
        git_in(directory, &["commit", "-q", "-m", message]);
        git_in(directory, &["rev-parse", "HEAD"])
    }

    /// `main` gains two commits after `dev` leaves it (one of them touching the same file as
    /// `dev`), `dev` gains three, and the merge-request branches carry some of them.
    struct MergeRequestFixture {
        _directory: util::test::TempTree,
        work_directory: std::path::PathBuf,
        /// On `dev`, touching `dev1.txt` and `shared.txt`.
        first_dev_commit: String,
        third_dev_commit: String,
        /// On `main` after `dev` left it, touching `shared.txt`.
        second_main_commit: String,
    }

    fn merge_request_fixture() -> MergeRequestFixture {
        let directory = util::test::TempTree::new(json!({ "project": {} }));
        let work_directory = directory.path().join("project");
        git_in(&work_directory, &["init", "-q"]);
        git_in(
            &work_directory,
            &["symbolic-ref", "HEAD", "refs/heads/main"],
        );
        let lines = |first: &str, last: &str| format!("{first}\n2\n3\n4\n5\n6\n7\n{last}\n");
        commit_files(
            &work_directory,
            &[
                ("a.txt", Some("a\n")),
                ("shared.txt", Some(&lines("1", "8"))),
                ("old.txt", Some("old\n")),
            ],
            "initial",
        );
        git_in(&work_directory, &["branch", "dev"]);
        commit_files(
            &work_directory,
            &[("main1.txt", Some("main one\n"))],
            "main one",
        );
        let second_main_commit = commit_files(
            &work_directory,
            &[
                ("main2.txt", Some("main two\n")),
                ("shared.txt", Some(&lines("main", "8"))),
            ],
            "main two",
        );
        git_in(&work_directory, &["checkout", "-q", "dev"]);
        let first = commit_files(
            &work_directory,
            &[
                ("dev1.txt", Some("dev one\n")),
                ("shared.txt", Some(&lines("1", "dev"))),
            ],
            "dev one",
        );
        let second = commit_files(
            &work_directory,
            &[("dev2.txt", Some("dev two\n")), ("old.txt", None)],
            "dev two",
        );
        let third = commit_files(
            &work_directory,
            &[("dev3.txt", Some("dev three\n"))],
            "dev three",
        );

        git_in(&work_directory, &["checkout", "-q", "-b", "mr", "main"]);
        git_in(&work_directory, &["cherry-pick", &first, &second]);
        git_in(
            &work_directory,
            &["checkout", "-q", "-b", "mr-full", "main"],
        );
        git_in(&work_directory, &["cherry-pick", &first, &second, &third]);
        git_in(
            &work_directory,
            &["checkout", "-q", "-b", "mr-extra", "mr-full"],
        );
        commit_files(
            &work_directory,
            &[("main1.txt", Some("resolved differently\n"))],
            "resolve a conflict",
        );
        let lonely = git_in(
            &work_directory,
            &["commit-tree", "HEAD^{tree}", "-m", "unrelated history"],
        );
        git_in(&work_directory, &["branch", "lonely", &lonely]);
        // Ported by copying the development branch's files over the base's, which throws
        // away `main two`'s change to `shared.txt`.
        git_in(
            &work_directory,
            &["checkout", "-q", "-b", "mr-dropped", "main"],
        );
        git_in(
            &work_directory,
            &[
                "checkout",
                "dev",
                "--",
                "dev1.txt",
                "dev2.txt",
                "dev3.txt",
                "shared.txt",
            ],
        );
        commit_files(&work_directory, &[("old.txt", None)], "port by copying");
        // Built from `main` before `main two`.
        git_in(
            &work_directory,
            &["checkout", "-q", "-b", "mr-stale", "main~1"],
        );
        git_in(&work_directory, &["cherry-pick", &first, &second, &third]);
        // Without `dev one`, so `shared.txt` stays exactly as `main` has it.
        git_in(
            &work_directory,
            &["checkout", "-q", "-b", "mr-base-version", "main"],
        );
        git_in(&work_directory, &["cherry-pick", &second, &third]);
        git_in(&work_directory, &["checkout", "-q", "dev"]);
        MergeRequestFixture {
            _directory: directory,
            work_directory,
            first_dev_commit: first,
            third_dev_commit: third,
            second_main_commit,
        }
    }

    async fn open_merge_request_check_and_wait(
        workspace: &Entity<Workspace>,
        repository: &Entity<Repository>,
        merge_request: &str,
        excluded: Vec<git::Oid>,
        cx: &mut gpui::VisualTestContext,
    ) -> Entity<RefComparison> {
        workspace.update_in(cx, |workspace, window, cx| {
            RefComparison::open_merge_request_check(
                workspace,
                repository.clone(),
                "main".into(),
                "dev".into(),
                merge_request.to_string().into(),
                excluded,
                window,
                cx,
            );
        });
        let comparison = workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_item_as::<RefComparison>(cx)
                .expect("the check should have opened as the active item")
        });
        let load_task = comparison.read_with(cx, |comparison, _| comparison.load_task.clone());
        load_task.await;
        comparison
    }

    #[track_caller]
    fn checked(
        comparison: &Entity<RefComparison>,
        cx: &mut gpui::VisualTestContext,
    ) -> (
        MergeRequestCheckCounts,
        Vec<(String, MergeRequestFileVerdict)>,
    ) {
        comparison.read_with(cx, |comparison, _| match &comparison.state {
            RefComparisonState::MergeRequestChecked(check) => {
                (check.counts.clone(), row_verdicts(&check.rows))
            }
            RefComparisonState::Failed(error) => panic!("the check failed: {error}"),
            _ => panic!("the check should have finished loading"),
        })
    }

    #[gpui::test]
    async fn test_merge_request_check_against_a_real_repository(cx: &mut TestAppContext) {
        init_test(cx);
        cx.executor().allow_parking();
        let fixture = merge_request_fixture();

        let project = Project::test(
            project::RealFs::new(None, cx.executor()),
            [fixture.work_directory.as_path()],
            cx,
        )
        .await;
        project
            .update(cx, |project, cx| project.git_scans_complete(cx))
            .await;
        cx.run_until_parked();
        let repository = project.read_with(cx, |project, cx| {
            project
                .repositories(cx)
                .values()
                .next()
                .expect("the repository should have been found")
                .clone()
        });
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(&*cx, |multi, _| multi.workspace().clone());
        let explained_by_base = MergeRequestFileVerdict::Explained {
            changed_on_base: true,
            changed_by_excluded: false,
        };

        // (a) the base's own changes stay out, (b) the file of the commit left out is suspected
        // and names that commit, (e) a file both sides changed is explained by the base.
        let check =
            open_merge_request_check_and_wait(&workspace, &repository, "mr", Vec::new(), cx).await;
        let (counts, verdicts) = checked(&check, cx);
        assert_eq!(
            verdicts,
            vec![
                (
                    "dev3.txt".to_string(),
                    MergeRequestFileVerdict::SuspectedMissing
                ),
                ("shared.txt".to_string(), explained_by_base),
                (
                    "dev1.txt".to_string(),
                    MergeRequestFileVerdict::Matched { deleted: false }
                ),
                (
                    "dev2.txt".to_string(),
                    MergeRequestFileVerdict::Matched { deleted: false }
                ),
                (
                    "old.txt".to_string(),
                    MergeRequestFileVerdict::Matched { deleted: true }
                ),
            ]
        );
        assert_eq!(
            counts,
            MergeRequestCheckCounts {
                source_files: 5,
                matched: 3,
                mismatched: 2,
                suspected_missing: 1,
                changed_on_base: 1,
                changed_by_excluded: 0,
                base_change_maybe_dropped: 0,
                only_in_merge_request: 0,
            }
        );
        assert_eq!(
            base_commits_ahead(&check, cx),
            0,
            "a merge request built from the base's tip is not behind it"
        );
        check.read_with(cx, |check, _| {
            let RefComparisonState::MergeRequestChecked(result) = &check.state else {
                panic!("the check should have loaded");
            };
            let missing = &result.rows[0];
            assert_eq!(
                missing
                    .commits
                    .iter()
                    .map(|commit| (commit.sha.to_string(), commit.subject.to_string()))
                    .collect::<Vec<_>>(),
                vec![(fixture.third_dev_commit.clone(), "dev three".to_string())],
                "the commit that was left out has to be named under its file"
            );
        });

        // Picking the row of a file deleted on both sides says so instead of reading nothing.
        check.update_in(cx, |check, window, cx| {
            check.open_file_diff(git::repository::repo_path("old.txt"), window, cx);
        });
        cx.run_until_parked();
        check.read_with(cx, |check, _| {
            assert_eq!(
                check.file_diff_error.as_deref(),
                Some("Deleted on both the development branch and the MR branch.")
            );
        });

        // A plain comparison of the same pair is a different tab.
        workspace.update_in(cx, |workspace, window, cx| {
            RefComparison::open(
                workspace,
                repository.clone(),
                "dev".into(),
                "mr".into(),
                window,
                cx,
            );
        });
        cx.run_until_parked();
        let (plain_count, check_count) = workspace.read_with(cx, |workspace, cx| {
            let items = workspace
                .items_of_type::<RefComparison>(cx)
                .collect::<Vec<_>>();
            let plain = items
                .iter()
                .filter(|item| item.read(cx).mode == RefComparisonMode::Refs)
                .count();
            (plain, items.len() - plain)
        });
        assert_eq!(
            (plain_count, check_count),
            (1, 1),
            "a merge-request check must not stand in for a plain comparison of the same refs"
        );

        // (c) Excluding that commit explains the file, in the same tab.
        let third: git::Oid = fixture
            .third_dev_commit
            .parse()
            .expect("git prints full SHAs");
        let excluded_check =
            open_merge_request_check_and_wait(&workspace, &repository, "mr", vec![third], cx).await;
        assert_eq!(excluded_check.entity_id(), check.entity_id());
        let (counts, verdicts) = checked(&excluded_check, cx);
        assert_eq!(
            verdicts.get(..2),
            Some(
                [
                    (
                        "dev3.txt".to_string(),
                        MergeRequestFileVerdict::Explained {
                            changed_on_base: false,
                            changed_by_excluded: true,
                        }
                    ),
                    ("shared.txt".to_string(), explained_by_base),
                ]
                .as_slice()
            )
        );
        assert_eq!(counts.suspected_missing, 0);
        assert_eq!(counts.verdict().0, "Nothing is missing");

        // The mode, the base and the excluded commits survive a restart; the diff is taken
        // again, and the picked row is only reopened once it is known what kind of row it is.
        excluded_check.update_in(cx, |check, window, cx| {
            check.open_file_diff(git::repository::repo_path("old.txt"), window, cx);
        });
        let item_id = workspace::ItemId::from(9301_u64);
        let workspace_db = cx.read(|cx| workspace::WorkspaceDb::global(cx));
        let workspace_id = workspace_db
            .next_id()
            .await
            .expect("should create workspace id");
        excluded_check
            .read_with(cx, |check, cx| check.write_to_db(item_id, workspace_id, cx))
            .await
            .expect("saving the check should succeed");
        let restored = cx
            .update(|window, cx| {
                <RefComparison as SerializableItem>::deserialize(
                    project.clone(),
                    workspace.downgrade(),
                    workspace_id,
                    item_id,
                    window,
                    cx,
                )
            })
            .await
            .expect("deserialization should succeed");
        let load_task = restored.read_with(cx, |restored, _| restored.load_task.clone());
        load_task.await;
        cx.run_until_parked();
        let (restored_counts, _) = checked(&restored, cx);
        assert_eq!(restored_counts, counts);
        restored.read_with(cx, |restored, _| {
            assert_eq!(
                restored.mode,
                RefComparisonMode::MergeRequestCheck {
                    merge_request_base: "main".into(),
                    excluded: vec![third],
                }
            );
            assert_eq!(
                restored.file_diff_error.as_deref(),
                Some("Deleted on both the development branch and the MR branch."),
                "the restored pick must be opened as the row it is, not before the list is known"
            );
        });

        // An excluded commit outside the development branch's history is looked up on its own.
        let from_main: git::Oid = fixture
            .second_main_commit
            .parse()
            .expect("git prints full SHAs");
        let looked_up = open_merge_request_check_and_wait(
            &workspace,
            &repository,
            "mr",
            vec![third, from_main],
            cx,
        )
        .await;
        let (_, verdicts) = checked(&looked_up, cx);
        assert!(
            verdicts.contains(&(
                "shared.txt".to_string(),
                MergeRequestFileVerdict::Explained {
                    changed_on_base: true,
                    changed_by_excluded: true,
                }
            )),
            "{verdicts:?}"
        );

        // A commit git does not know is refused, not quietly dropped from the excluded set.
        let refused = open_merge_request_check_and_wait(
            &workspace,
            &repository,
            "mr",
            vec![test_oid('7')],
            cx,
        )
        .await;
        refused.read_with(cx, |refused, _| match &refused.state {
            RefComparisonState::Failed(error) => {
                assert!(
                    error.contains("Could not read the excluded commits"),
                    "{error}"
                )
            }
            _ => panic!("an unknown excluded commit must fail the check"),
        });

        // (d) Everything ported.
        let full =
            open_merge_request_check_and_wait(&workspace, &repository, "mr-full", Vec::new(), cx)
                .await;
        let (counts, verdicts) = checked(&full, cx);
        assert_eq!(counts.suspected_missing, 0);
        assert_eq!(counts.only_in_merge_request, 0);
        assert_eq!(counts.verdict().0, "Nothing is missing");
        assert_eq!(
            verdicts.first(),
            Some(&("shared.txt".to_string(), explained_by_base))
        );

        // A change only the merge request has is listed apart and opens against the base.
        let extra =
            open_merge_request_check_and_wait(&workspace, &repository, "mr-extra", Vec::new(), cx)
                .await;
        let (counts, verdicts) = checked(&extra, cx);
        assert_eq!(counts.only_in_merge_request, 1);
        assert!(verdicts.contains(&(
            "main1.txt".to_string(),
            MergeRequestFileVerdict::OnlyInMergeRequest
        )));
        assert_eq!(counts.verdict().1, Color::Warning);
        let sides = extra.read_with(cx, |extra, _| {
            extra
                .file_diff_sides(&git::repository::repo_path("main1.txt"))
                .map(|sides| (sides.left.to_string(), sides.right.to_string()))
        });
        assert_eq!(
            sides.ok(),
            Some(("main".to_string(), "mr-extra".to_string()))
        );

        // Unrelated histories are refused with the reason rather than compared tree to tree.
        workspace.update_in(cx, |workspace, window, cx| {
            RefComparison::open_merge_request_check(
                workspace,
                repository.clone(),
                "lonely".into(),
                "dev".into(),
                "mr".into(),
                Vec::new(),
                window,
                cx,
            );
        });
        let unrelated = workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_item_as::<RefComparison>(cx)
                .expect("the check should have opened")
        });
        let load_task = unrelated.read_with(cx, |unrelated, _| unrelated.load_task.clone());
        load_task.await;
        unrelated.read_with(cx, |unrelated, _| match &unrelated.state {
            RefComparisonState::Failed(error) => {
                assert!(error.contains("no common ancestor"), "{error}")
            }
            _ => panic!("a check against an unrelated history must fail with the reason"),
        });

        // The development branch's version of a file the base changed too is not a match: the
        // merge request would undo the base's change. It opens against the base, since the
        // development branch's side is exactly what the merge request has.
        let dropped = open_merge_request_check_and_wait(
            &workspace,
            &repository,
            "mr-dropped",
            Vec::new(),
            cx,
        )
        .await;
        let (counts, verdicts) = checked(&dropped, cx);
        assert_eq!(
            verdicts.first(),
            Some(&(
                "shared.txt".to_string(),
                MergeRequestFileVerdict::BaseChangeMaybeDropped
            ))
        );
        assert_eq!(
            (
                counts.matched,
                counts.mismatched,
                counts.base_change_maybe_dropped,
                counts.suspected_missing,
                counts.only_in_merge_request
            ),
            (4, 0, 1, 0, 0)
        );
        assert_eq!(
            counts.verdict(),
            (
                "Nothing is missing, but 1 file needs a look".to_string(),
                Color::Warning
            )
        );
        let sides = dropped.read_with(cx, |dropped, _| {
            dropped
                .file_diff_sides(&git::repository::repo_path("shared.txt"))
                .map(|sides| (sides.left.to_string(), sides.right.to_string()))
        });
        assert_eq!(
            sides.ok(),
            Some(("main".to_string(), "mr-dropped".to_string()))
        );
        assert_eq!(base_commits_ahead(&dropped, cx), 0);

        // A merge request built from an older base says so, and so do its rows.
        let stale =
            open_merge_request_check_and_wait(&workspace, &repository, "mr-stale", Vec::new(), cx)
                .await;
        let (counts, verdicts) = checked(&stale, cx);
        assert_eq!(base_commits_ahead(&stale, cx), 1);
        assert_eq!(counts.suspected_missing, 0);
        assert!(
            verdicts.contains(&(
                "main2.txt".to_string(),
                MergeRequestFileVerdict::OnlyInMergeRequest
            )),
            "{verdicts:?}"
        );
        assert!(
            verdicts.contains(&(
                "shared.txt".to_string(),
                MergeRequestFileVerdict::BaseChangeMaybeDropped
            )),
            "{verdicts:?}"
        );

        // A file both branches changed, left exactly as the base has it, lacks the development
        // branch's change: the base moving on does not explain that away.
        let base_version = open_merge_request_check_and_wait(
            &workspace,
            &repository,
            "mr-base-version",
            Vec::new(),
            cx,
        )
        .await;
        let (counts, verdicts) = checked(&base_version, cx);
        assert_eq!(
            verdicts.get(..2),
            Some(
                [
                    (
                        "dev1.txt".to_string(),
                        MergeRequestFileVerdict::SuspectedMissing
                    ),
                    (
                        "shared.txt".to_string(),
                        MergeRequestFileVerdict::SuspectedMissing
                    ),
                ]
                .as_slice()
            ),
            "{verdicts:?}"
        );
        assert_eq!(
            (
                counts.matched,
                counts.mismatched,
                counts.suspected_missing,
                counts.changed_on_base
            ),
            (3, 2, 2, 0)
        );
        assert_eq!(counts.verdict().1, Color::Error);
        let (flagged, shared_commits, sides) = base_version.read_with(cx, |base_version, _| {
            let RefComparisonState::MergeRequestChecked(check) = &base_version.state else {
                panic!("the check should have loaded");
            };
            let flagged = check
                .rows
                .iter()
                .filter(|row| row.merge_request_has_base_version)
                .map(|row| row.path.as_unix_str().to_string())
                .collect::<Vec<_>>();
            let shared_commits = check
                .rows
                .iter()
                .find(|row| row.path.as_unix_str() == "shared.txt")
                .map(|row| {
                    row.commits
                        .iter()
                        .map(|commit| commit.sha.to_string())
                        .collect::<Vec<_>>()
                });
            let sides = base_version
                .file_diff_sides(&git::repository::repo_path("shared.txt"))
                .map(|sides| (sides.left.to_string(), sides.right.to_string()))
                .ok();
            (flagged, shared_commits, sides)
        });
        assert_eq!(flagged, vec!["shared.txt".to_string()]);
        assert_eq!(
            shared_commits,
            Some(vec![fixture.first_dev_commit.clone()]),
            "the commit whose change is missing has to be named under the file"
        );
        assert_eq!(
            sides,
            Some(("dev".to_string(), "mr-base-version".to_string())),
            "the development branch's side shows what is missing"
        );

        // Asking again after the merge-request branch was rebuilt recomputes the check, even
        // with the same excluded commits, and keeps the picked file.
        let before =
            open_merge_request_check_and_wait(&workspace, &repository, "mr", Vec::new(), cx).await;
        assert_eq!(before.entity_id(), check.entity_id());
        assert_eq!(checked(&before, cx).0.suspected_missing, 1);
        before.update_in(cx, |before, window, cx| {
            before.open_file_diff(git::repository::repo_path("dev3.txt"), window, cx);
        });
        git_in(&fixture.work_directory, &["branch", "-f", "mr", "mr-full"]);
        let after =
            open_merge_request_check_and_wait(&workspace, &repository, "mr", Vec::new(), cx).await;
        assert_eq!(after.entity_id(), check.entity_id());
        let (counts, _) = checked(&after, cx);
        assert_eq!(
            counts.suspected_missing, 0,
            "the check of the branch before it was rebuilt must not answer for the new one"
        );
        after.read_with(cx, |after, _| {
            assert_eq!(
                after.selected_path,
                Some(git::repository::repo_path("dev3.txt"))
            );
        });
    }

    fn base_commits_ahead(
        comparison: &Entity<RefComparison>,
        cx: &mut gpui::VisualTestContext,
    ) -> usize {
        comparison.read_with(cx, |comparison, _| match &comparison.state {
            RefComparisonState::MergeRequestChecked(check) => check.base_commits_ahead,
            _ => panic!("the check should have finished loading"),
        })
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
            TreeDiffStatus::Added => i18n::tc!("file status", "Added"),
            TreeDiffStatus::Modified { .. } => i18n::tc!("file status", "Modified"),
            TreeDiffStatus::Deleted { .. } => i18n::tc!("file status", "Deleted"),
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
    MergeRequestChecked(MergeRequestCheck),
    Failed(SharedString),
}

/// What a [`RefComparison`] compares its two refs for.
#[derive(Clone, Debug, PartialEq)]
enum RefComparisonMode {
    /// The two trees as they are.
    Refs,
    /// Whether the merge-request branch (`head`) carries everything the development branch
    /// (`base`) changed since it left `merge_request_base`. Comparing the two trees directly
    /// would also list every change `merge_request_base` gained after the development branch
    /// left it, which buries the files that were actually left out.
    MergeRequestCheck {
        /// The branch the merge request goes into, usually the latest `origin/main`.
        merge_request_base: SharedString,
        /// Development-branch commits deliberately left out of the merge request, kept sorted
        /// and deduplicated so that two requests for the same set compare equal.
        excluded: Vec<git::Oid>,
    },
}

fn normalize_excluded_commits(mut excluded: Vec<git::Oid>) -> Vec<git::Oid> {
    excluded.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    excluded.dedup();
    excluded
}

/// Above this many commits, the development-branch history read to name the commits behind a
/// file suspected missing is cut off, so that a long-lived branch does not hand the UI an
/// unbounded log.
const MAX_MERGE_REQUEST_HISTORY_COMMITS: usize = 2_000;

/// How many commits are listed under one file suspected missing from a merge request.
const MAX_COMMITS_PER_MISSING_FILE: usize = 5;

/// Excluded commits outside the development branch's recent history are looked up with one
/// `git log --no-walk`, and every one of them goes on its command line.
const MAX_EXCLUDED_COMMIT_LOOKUPS: usize = 200;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MergeRequestFileVerdict {
    /// The merge request differs from the development branch, and neither the merge-request
    /// base moving on nor an excluded commit explains why.
    SuspectedMissing,
    /// The merge request differs from the development branch for a reason the user already
    /// knows about.
    Explained {
        changed_on_base: bool,
        changed_by_excluded: bool,
    },
    /// The merge request has the development branch's version of a file the base changed as
    /// well, and that version is not the base's: merging it may undo the base's change.
    BaseChangeMaybeDropped,
    /// The development branch never touched the file, yet the merge request changes it
    /// relative to its base (typically a conflict resolution).
    OnlyInMergeRequest,
    Matched {
        /// Deleted on both sides, so there is nothing to show side by side.
        deleted: bool,
    },
}

impl MergeRequestFileVerdict {
    /// The order the file list shows the verdicts in, which is also the order the list is cut
    /// off in once it grows past [`MAX_REF_COMPARISON_ENTRIES`].
    fn rank(self) -> u8 {
        match self {
            Self::SuspectedMissing => 0,
            Self::BaseChangeMaybeDropped => 1,
            Self::Explained { .. } => 2,
            Self::OnlyInMergeRequest => 3,
            Self::Matched { .. } => 4,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::SuspectedMissing => i18n::t!("Possibly missing"),
            Self::BaseChangeMaybeDropped => i18n::t!("A base change may be missing from the MR"),
            Self::Explained {
                changed_on_base: true,
                changed_by_excluded: true,
            } => i18n::t!("Also changed on the base · changed by an excluded commit"),
            Self::Explained {
                changed_on_base: true,
                changed_by_excluded: false,
            } => i18n::t!("Also changed on the base: the diff includes the base's changes"),
            Self::Explained { .. } => i18n::t!("Changed by an excluded commit"),
            Self::OnlyInMergeRequest => i18n::t!("Not from the development branch"),
            Self::Matched { .. } => i18n::t!("Matches"),
        }
    }

    fn color(self) -> Color {
        match self {
            Self::SuspectedMissing => Color::Error,
            Self::BaseChangeMaybeDropped | Self::OnlyInMergeRequest => Color::Warning,
            Self::Explained { .. } => Color::Muted,
            Self::Matched { .. } => Color::Success,
        }
    }

    /// `merge_request_is_behind` is whether the merge-request branch lacks commits of its
    /// base's tip, which is then the likelier reason for a change only the merge request has.
    fn description(self, merge_request_is_behind: bool) -> &'static str {
        match self {
            Self::SuspectedMissing => {
                i18n::t!(
                    "Changed on the development branch, but different on the MR branch. The commits below may be missing."
                )
            }
            Self::BaseChangeMaybeDropped => {
                i18n::t!(
                    "The MR branch matches the development branch, but the base changed this file too and differs from it. Merging the MR may undo the base's change. The base is on the left, the MR branch on the right."
                )
            }
            Self::Explained {
                changed_on_base: true,
                changed_by_excluded: true,
            } => {
                i18n::t!(
                    "This file changed on the base too, and an excluded commit touched it. The diff mixes both."
                )
            }
            Self::Explained {
                changed_on_base: true,
                ..
            } => {
                i18n::t!(
                    "This file changed on the base too, so the diff includes the base's changes (usually fine)."
                )
            }
            Self::Explained { .. } => i18n::t!("A file an excluded commit touched (as intended)."),
            Self::OnlyInMergeRequest if merge_request_is_behind => {
                i18n::t!(
                    "Not changed on the development branch, yet the MR branch differs from the base. The MR branch was not created from the tip of its base, so it may only lack the base's newer changes (or the change came in while resolving a conflict). The base is on the left, the MR branch on the right."
                )
            }
            Self::OnlyInMergeRequest => {
                i18n::t!(
                    "Not changed on the development branch, yet the MR branch differs from the base (a change that came in while resolving a conflict, for example). The base is on the left, the MR branch on the right."
                )
            }
            Self::Matched { .. } => i18n::t!("Same contents as on the development branch."),
        }
    }
}

/// A development-branch commit, as listed under a file suspected missing.
#[derive(Clone, Debug, PartialEq)]
struct MergeRequestCommit {
    sha: git::Oid,
    subject: SharedString,
}

/// One commit of `git log --name-only`, with the files it changed.
#[derive(Clone, Debug, PartialEq)]
struct CommitWithPaths {
    commit: MergeRequestCommit,
    paths: Vec<RepoPath>,
}

#[derive(Debug, PartialEq)]
struct MergeRequestCheckRow {
    path: RepoPath,
    verdict: MergeRequestFileVerdict,
    /// How the file differs between the two sides the row opens (the development branch and
    /// the merge request, or for [`MergeRequestFileVerdict::OnlyInMergeRequest`] and
    /// [`MergeRequestFileVerdict::BaseChangeMaybeDropped`] the base and the merge request);
    /// `None` when they match.
    status: Option<TreeDiffStatus>,
    /// For a file suspected missing, the development-branch commits that touched it, newest
    /// first.
    commits: Vec<MergeRequestCommit>,
    omitted_commit_count: usize,
    /// A file suspected missing that both branches changed, where the merge request has the
    /// base's version untouched.
    merge_request_has_base_version: bool,
}

/// The counts behind the check's headline. They are taken before the file list is cut off, so
/// the headline stays true for a check too large to list in full.
#[derive(Clone, Debug, Default, PartialEq)]
struct MergeRequestCheckCounts {
    /// Files the development branch changed since it left the base: `matched`, `mismatched`
    /// and `base_change_maybe_dropped` add up to it.
    source_files: usize,
    matched: usize,
    mismatched: usize,
    suspected_missing: usize,
    changed_on_base: usize,
    changed_by_excluded: usize,
    /// Files whose contents match the development branch's but not the base's, although the
    /// base changed them too. Not counted as matched: the match is what may undo the base.
    base_change_maybe_dropped: usize,
    only_in_merge_request: usize,
}

impl MergeRequestCheckCounts {
    fn summary(&self) -> String {
        let mut summary = i18n::tn!(
            self.source_files,
            "{count} file changed on the development branch: {matched} matching / {mismatched} not matching",
            "{count} files changed on the development branch: {matched} matching / {mismatched} not matching",
            matched = self.matched,
            mismatched = self.mismatched
        );
        let reasons = [
            (
                self.suspected_missing,
                i18n::t!("{} possibly missing", self.suspected_missing),
            ),
            (
                self.changed_on_base,
                i18n::t!("{} also changed on the base", self.changed_on_base),
            ),
            (
                self.changed_by_excluded,
                i18n::t!("{} excluded", self.changed_by_excluded),
            ),
        ]
        .into_iter()
        .filter(|(count, _)| *count > 0)
        .map(|(_, reason)| reason)
        .collect::<Vec<_>>();
        if !reasons.is_empty() {
            summary.push_str(&format!(
                " ({})",
                reasons.join(i18n::tc!("list separator", ", "))
            ));
        }
        if self.base_change_maybe_dropped > 0 {
            summary.push_str(" / ");
            summary.push_str(&i18n::tn!(
                self.base_change_maybe_dropped,
                "{count} base change possibly missing",
                "{count} base changes possibly missing"
            ));
        }
        if self.only_in_merge_request > 0 {
            summary.push_str(&i18n::tn!(
                self.only_in_merge_request,
                ". {count} changed file not from the development branch",
                ". {count} changed files not from the development branch"
            ));
        }
        summary
    }

    fn verdict(&self) -> (String, Color) {
        if self.suspected_missing > 0 {
            (
                i18n::tn!(
                    self.suspected_missing,
                    "{count} file possibly missing",
                    "{count} files possibly missing"
                ),
                Color::Error,
            )
        } else if self.base_change_maybe_dropped + self.only_in_merge_request > 0 {
            (
                i18n::tn!(
                    self.base_change_maybe_dropped + self.only_in_merge_request,
                    "Nothing is missing, but {count} file needs a look",
                    "Nothing is missing, but {count} files need a look"
                ),
                Color::Warning,
            )
        } else {
            (i18n::t!("Nothing is missing").to_string(), Color::Success)
        }
    }
}

#[derive(Debug)]
struct MergeRequestCheck {
    merge_base: git::Oid,
    counts: MergeRequestCheckCounts,
    /// Ordered by [`MergeRequestFileVerdict::rank`], then by path.
    rows: Vec<MergeRequestCheckRow>,
    /// Rows left out past [`MAX_REF_COMPARISON_ENTRIES`].
    omitted_count: usize,
    /// The development-branch history stopped at [`MAX_MERGE_REQUEST_HISTORY_COMMITS`], so a
    /// suspected file without commits may just have been touched further back.
    history_truncated: bool,
    /// How many commits of the base's tip the merge-request branch lacks, counted up to
    /// [`MAX_COUNTED_BASE_COMMITS`] and one past it.
    base_commits_ahead: usize,
}

const MERGE_REQUEST_HAS_BASE_VERSION_NOTE: &str = i18n::msg!(
    "The MR has the same contents as the base (fine if the base already took the same change)"
);

/// Counting how far the merge-request branch lags its base stops here: past it, the headline
/// only needs to say "at least".
const MAX_COUNTED_BASE_COMMITS: usize = 1_000;

/// `git log` arguments listing the commits of `merge_request_base` that `merge_request` lacks,
/// one full SHA per line. None of them means the merge request starts from the base's tip.
fn base_commits_ahead_args(merge_request_base: &str, merge_request: &str) -> Vec<String> {
    vec![
        "log".to_string(),
        "--format=%H".to_string(),
        format!("--max-count={}", MAX_COUNTED_BASE_COMMITS + 1),
        format!("{merge_request}..{merge_request_base}"),
        "--".to_string(),
    ]
}

fn count_listed_commits(output: &str) -> usize {
    output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count()
}

/// The headline's warning for a merge-request branch that was not built from its base's tip.
fn merge_request_behind_notice(base_commits_ahead: usize) -> Option<String> {
    match base_commits_ahead {
        0 => None,
        count if count > MAX_COUNTED_BASE_COMMITS => Some(i18n::t!(
            "The MR branch was not created from the tip of its base (the base is at least {MAX_COUNTED_BASE_COMMITS} commits ahead)"
        )),
        count => Some(i18n::tn!(
            count,
            "The MR branch was not created from the tip of its base (the base is {count} commit ahead)",
            "The MR branch was not created from the tip of its base (the base is {count} commits ahead)"
        )),
    }
}

/// Sorts the files the merge-request check looks at into verdicts.
///
/// `source_changes` is what the development branch changed since the merge base, and only
/// those files are compared: `source_to_merge_request` says which of them the merge request
/// has differently, `base_changes` (merge base → base) and `excluded_paths` say why. Files the
/// merge request changes relative to its base that the development branch never touched come
/// from `base_to_merge_request`, which also tells whether a file both branches changed, and the
/// merge request has as the development branch does, still differs from the base: since the
/// merge request then has the development branch's version, the base's version is not the
/// development branch's either, and no further git call is needed to know it.
fn classify_merge_request_files(
    source_changes: &TreeDiff,
    base_changes: &TreeDiff,
    source_to_merge_request: &TreeDiff,
    base_to_merge_request: &TreeDiff,
    excluded_paths: &HashSet<RepoPath>,
) -> (Vec<MergeRequestCheckRow>, MergeRequestCheckCounts) {
    let mut counts = MergeRequestCheckCounts {
        source_files: source_changes.entries.len(),
        ..MergeRequestCheckCounts::default()
    };
    let row = |path: &RepoPath,
               verdict: MergeRequestFileVerdict,
               status: Option<&TreeDiffStatus>| MergeRequestCheckRow {
        path: path.clone(),
        verdict,
        status: status.cloned(),
        commits: Vec::new(),
        omitted_commit_count: 0,
        merge_request_has_base_version: false,
    };
    let mut rows = Vec::new();
    for (path, source_status) in &source_changes.entries {
        let Some(status) = source_to_merge_request.entries.get(path) else {
            if base_changes.entries.contains_key(path)
                && let Some(base_status) = base_to_merge_request.entries.get(path)
            {
                counts.base_change_maybe_dropped += 1;
                rows.push(row(
                    path,
                    MergeRequestFileVerdict::BaseChangeMaybeDropped,
                    Some(base_status),
                ));
                continue;
            }
            counts.matched += 1;
            let deleted = matches!(source_status, TreeDiffStatus::Deleted { .. });
            rows.push(row(
                path,
                MergeRequestFileVerdict::Matched { deleted },
                None,
            ));
            continue;
        };
        counts.mismatched += 1;
        let changed_on_base = base_changes.entries.contains_key(path);
        let changed_by_excluded = excluded_paths.contains(path);
        // The base moving on explains a difference only when the merge request carries
        // something of the development branch's change too. Having the base's version as it is
        // means that change is nowhere in the merge request, unless the base took the same
        // change in on its own.
        if changed_on_base
            && !changed_by_excluded
            && !base_to_merge_request.entries.contains_key(path)
        {
            counts.suspected_missing += 1;
            rows.push(MergeRequestCheckRow {
                merge_request_has_base_version: true,
                ..row(
                    path,
                    MergeRequestFileVerdict::SuspectedMissing,
                    Some(status),
                )
            });
            continue;
        }
        counts.changed_on_base += usize::from(changed_on_base);
        counts.changed_by_excluded += usize::from(changed_by_excluded);
        let verdict = if changed_on_base || changed_by_excluded {
            MergeRequestFileVerdict::Explained {
                changed_on_base,
                changed_by_excluded,
            }
        } else {
            counts.suspected_missing += 1;
            MergeRequestFileVerdict::SuspectedMissing
        };
        rows.push(row(path, verdict, Some(status)));
    }
    for (path, status) in &base_to_merge_request.entries {
        if source_changes.entries.contains_key(path) {
            continue;
        }
        counts.only_in_merge_request += 1;
        rows.push(row(
            path,
            MergeRequestFileVerdict::OnlyInMergeRequest,
            Some(status),
        ));
    }
    rows.sort_by(|left, right| {
        left.verdict
            .rank()
            .cmp(&right.verdict.rank())
            .then_with(|| left.path.cmp(&right.path))
    });
    (rows, counts)
}

/// Lists, under each file suspected missing, the commits of `history` (newest first) that
/// touched it.
fn attach_commits_to_missing_files(rows: &mut [MergeRequestCheckRow], history: &[CommitWithPaths]) {
    let missing_rows = rows
        .iter()
        .enumerate()
        .filter(|(_, row)| row.verdict == MergeRequestFileVerdict::SuspectedMissing)
        .map(|(index, row)| (row.path.clone(), index))
        .collect::<HashMap<_, _>>();
    if missing_rows.is_empty() {
        return;
    }
    for commit in history {
        for path in &commit.paths {
            let Some(row) = missing_rows
                .get(path)
                .and_then(|index| rows.get_mut(*index))
            else {
                continue;
            };
            if row.commits.len() < MAX_COMMITS_PER_MISSING_FILE {
                row.commits.push(commit.commit.clone());
            } else {
                row.omitted_commit_count += 1;
            }
        }
    }
}

/// The files the `excluded` commits of `history` changed, and the excluded commits `history`
/// does not contain.
fn paths_changed_by_excluded(
    history: &[CommitWithPaths],
    excluded: &[git::Oid],
) -> (HashSet<RepoPath>, Vec<git::Oid>) {
    let mut paths = HashSet::default();
    let mut not_found = Vec::new();
    for sha in excluded {
        match history.iter().find(|commit| commit.commit.sha == *sha) {
            Some(commit) => paths.extend(commit.paths.iter().cloned()),
            None => not_found.push(*sha),
        }
    }
    (paths, not_found)
}

/// `git log` arguments listing commits with the files each changed, in the layout
/// [`parse_commits_with_paths`] reads. `-z` keeps paths unquoted whatever `core.quotePath` says.
fn commits_with_paths_args(
    leading: &[&str],
    revisions: impl IntoIterator<Item = String>,
) -> Vec<String> {
    let mut args = vec!["log".to_string()];
    args.extend(leading.iter().map(|argument| argument.to_string()));
    args.extend(["--no-renames", "--name-only", "-z", "--format=%x00%H%x00%s"].map(String::from));
    args.extend(revisions);
    args
}

/// Reads the output of [`commits_with_paths_args`]: each commit is `\0<sha>\0<subject>\0`
/// followed by its paths, each ending in `\0` (the first after a newline that separates them
/// from the subject). A merge commit lists no paths.
///
/// NUL is the one byte neither a subject nor a path can hold, so every field ends at one: a
/// commit starts at an empty field, since a path is never empty, and the field after its SHA is
/// its subject even when that is empty.
fn parse_commits_with_paths(output: &str) -> Vec<CommitWithPaths> {
    let mut commits = Vec::new();
    let mut fields = output.split('\0').peekable();
    while let Some(field) = fields.next() {
        if !field.is_empty() {
            continue;
        }
        let Some(sha) = fields.next() else {
            break;
        };
        let Ok(sha) = sha.trim().parse::<git::Oid>() else {
            continue;
        };
        let subject = fields.next().unwrap_or_default();
        let mut paths = Vec::new();
        while let Some(path) = fields.next_if(|field| !field.is_empty()) {
            if let Ok(path) = RepoPath::new(path.trim_start_matches('\n')) {
                paths.push(path);
            }
        }
        commits.push(CommitWithPaths {
            commit: MergeRequestCommit {
                sha,
                subject: subject.trim().to_string().into(),
            },
            paths,
        });
    }
    commits
}

/// The merge base out of `git rev-parse <base>...<source>`, which prints both tips and then
/// every merge base prefixed with `^`.
fn parse_single_merge_base(
    output: &str,
    merge_request_base: &str,
    source: &str,
) -> Result<git::Oid> {
    let merge_bases = output
        .lines()
        .filter_map(|line| line.trim().strip_prefix('^'))
        .collect::<Vec<_>>();
    match merge_bases.as_slice() {
        [] => anyhow::bail!(
            "{}",
            i18n::t!(
                "{merge_request_base} and {source} have no common ancestor (unrelated histories). The MR check cannot run."
            )
        ),
        [merge_base] => merge_base
            .parse::<git::Oid>()
            .with_context(|| i18n::t!("Could not read the common ancestor: {merge_base}")),
        _ => anyhow::bail!(
            "{}",
            i18n::t!(
                "{merge_request_base} and {source} have {} common ancestors (a criss-cross merge). The MR check cannot tell where to compare from, so it cannot run.",
                merge_bases.len()
            )
        ),
    }
}

/// A remote server from before `rev-parse` was allow-listed refuses it by name only, which does
/// not tell the user what to do about it.
fn explain_old_server_refusal(error: anyhow::Error) -> anyhow::Error {
    if format!("{error:#}").contains("unsupported git command") {
        anyhow!(i18n::t!(
            "The Zed server on the remote host is too old for the MR check (it refuses git rev-parse). Update Zed on the remote host."
        ))
    } else {
        error
    }
}

/// Everything behind a merge-request check, all through `diff-tree` and allow-listed
/// `run_command` subcommands, so it works the same over SSH and on the git versions ADR-F47
/// covers.
async fn load_merge_request_check(
    repository: Entity<Repository>,
    merge_request_base: SharedString,
    source: SharedString,
    merge_request: SharedString,
    excluded: Vec<git::Oid>,
    cx: &mut AsyncApp,
) -> Result<MergeRequestCheck> {
    let merge_bases = repository
        .update(cx, |repository, _| {
            repository.run_git_command(
                vec![
                    "rev-parse".into(),
                    format!("{merge_request_base}...{source}"),
                ],
                None,
            )
        })
        .await?
        .map_err(explain_old_server_refusal)?;
    anyhow::ensure!(
        merge_bases.succeeded(),
        "{}",
        i18n::t!(
            "Could not read {merge_request_base} and {source}: {}",
            merge_bases.failure_message()
        )
    );
    let merge_base = parse_single_merge_base(&merge_bases.stdout, &merge_request_base, &source)?;
    let merge_base_ref = SharedString::from(merge_base.to_string());

    let tree_diff = |base: &SharedString, head: &SharedString| DiffTreeType::Since {
        base: base.clone(),
        head: head.clone(),
    };
    let history_limit = format!("--max-count={MAX_MERGE_REQUEST_HISTORY_COMMITS}");
    let history_args = commits_with_paths_args(
        &[history_limit.as_str()],
        [format!("{merge_base}..{source}")],
    );
    let base_ahead_args = base_commits_ahead_args(&merge_request_base, &merge_request);
    let (
        source_changes,
        base_changes,
        source_to_merge_request,
        base_to_merge_request,
        history,
        base_ahead,
    ) = repository.update(cx, |repository, cx| {
        (
            repository.diff_tree(tree_diff(&merge_base_ref, &source), cx),
            repository.diff_tree(tree_diff(&merge_base_ref, &merge_request_base), cx),
            repository.diff_tree(tree_diff(&source, &merge_request), cx),
            repository.diff_tree(tree_diff(&merge_request_base, &merge_request), cx),
            repository.run_git_command(history_args, None),
            repository.run_git_command(base_ahead_args, None),
        )
    });
    let source_changes = source_changes
        .await?
        .context(i18n::t!("Could not read the development branch's changes"))?;
    let base_changes = base_changes
        .await?
        .context(i18n::t!("Could not read the changes on the base"))?;
    let source_to_merge_request = source_to_merge_request.await?.context(i18n::t!(
        "Could not compare the development branch and the MR branch"
    ))?;
    let base_to_merge_request = base_to_merge_request
        .await?
        .context(i18n::t!("Could not compare the base and the MR branch"))?;
    let history = history.await??;
    anyhow::ensure!(
        history.succeeded(),
        "{}",
        i18n::t!(
            "Could not read the development branch's history: {}",
            history.failure_message()
        )
    );
    let base_ahead = base_ahead.await??;
    anyhow::ensure!(
        base_ahead.succeeded(),
        "{}",
        i18n::t!(
            "Could not compare the histories of the base and the MR branch: {}",
            base_ahead.failure_message()
        )
    );
    let base_commits_ahead = count_listed_commits(&base_ahead.stdout);

    // A long-lived branch's log and a large tree diff are too much to parse and sort on the UI
    // thread.
    let (history, mut excluded_paths, not_in_history) = cx
        .background_spawn(async move {
            let history = parse_commits_with_paths(&history.stdout);
            let (excluded_paths, not_in_history) = paths_changed_by_excluded(&history, &excluded);
            (history, excluded_paths, not_in_history)
        })
        .await;
    let history_truncated = history.len() >= MAX_MERGE_REQUEST_HISTORY_COMMITS;
    if !not_in_history.is_empty() {
        anyhow::ensure!(
            not_in_history.len() <= MAX_EXCLUDED_COMMIT_LOOKUPS,
            "{}",
            i18n::t!(
                "{} of the excluded commits are not in the recent history of the development branch (at most {MAX_EXCLUDED_COMMIT_LOOKUPS} can be looked up at once).",
                not_in_history.len()
            )
        );
        let lookup = repository
            .update(cx, |repository, _| {
                repository.run_git_command(
                    commits_with_paths_args(
                        &["--no-walk"],
                        not_in_history.iter().map(|sha| sha.to_string()),
                    ),
                    None,
                )
            })
            .await??;
        anyhow::ensure!(
            lookup.succeeded(),
            "{}",
            i18n::t!(
                "Could not read the excluded commits: {}",
                lookup.failure_message()
            )
        );
        let looked_up = parse_commits_with_paths(&lookup.stdout);
        let (paths, still_missing) = paths_changed_by_excluded(&looked_up, &not_in_history);
        if let Some(missing) = still_missing.first() {
            anyhow::bail!(
                "{}",
                i18n::t!(
                    "The excluded commit {} was not found.",
                    missing.display_short()
                )
            );
        }
        excluded_paths.extend(paths);
    }

    let (rows, counts, omitted_count) = cx
        .background_spawn(async move {
            let (mut rows, counts) = classify_merge_request_files(
                &source_changes,
                &base_changes,
                &source_to_merge_request,
                &base_to_merge_request,
                &excluded_paths,
            );
            let omitted_count = rows.len().saturating_sub(MAX_REF_COMPARISON_ENTRIES);
            rows.truncate(MAX_REF_COMPARISON_ENTRIES);
            attach_commits_to_missing_files(&mut rows, &history);
            (rows, counts, omitted_count)
        })
        .await;
    Ok(MergeRequestCheck {
        merge_base,
        counts,
        rows,
        omitted_count,
        history_truncated,
        base_commits_ahead,
    })
}

/// The branches a comparison can be reopened against as a merge-request check: every branch
/// but the two already being compared, local ones first.
fn merge_request_base_candidates(
    branches: &[git::repository::Branch],
    source: &str,
    merge_request: &str,
) -> Vec<SharedString> {
    let mut local = Vec::new();
    let mut remote = Vec::new();
    for branch in branches {
        let name = branch.name();
        if name == source || name == merge_request {
            continue;
        }
        let name = SharedString::from(name.to_string());
        if branch.is_remote() {
            remote.push(name);
        } else {
            local.push(name);
        }
    }
    local.extend(remote);
    local
}

/// The two refs one file of a comparison is read from, and which of them may lack the file.
struct FileDiffSides {
    left: SharedString,
    right: SharedString,
    left_may_be_missing: bool,
    right_may_be_missing: bool,
}

impl FileDiffSides {
    /// `status` is how the file differs from `left` to `right`. `None` (a file the list does
    /// not know) leaves either side free to be missing.
    fn listed(left: SharedString, right: SharedString, status: Option<&TreeDiffStatus>) -> Self {
        Self {
            left,
            right,
            left_may_be_missing: status.is_none_or(|status| *status == TreeDiffStatus::Added),
            right_may_be_missing: status
                .is_none_or(|status| matches!(status, TreeDiffStatus::Deleted { .. })),
        }
    }
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
    /// The left side: the ref things are ported from, or in a merge-request check the
    /// development branch.
    base: SharedString,
    /// The right side: the ref things are ported to, or in a merge-request check the
    /// merge-request branch.
    head: SharedString,
    mode: RefComparisonMode,
    state: RefComparisonState,
    /// Whether a merge-request check also lists the files that match.
    show_all_files: bool,
    selected_path: Option<RepoPath>,
    file_diff_editor: Option<Entity<Editor>>,
    /// Why the picked file could not be shown. Kept apart from `state` so that
    /// one unshowable file does not take the whole file list down with it.
    file_diff_error: Option<SharedString>,
    /// Files found (only once opened — `diff-tree` doesn't report size or
    /// binary-ness up front) to be binary or too large to show, so their row
    /// in the list can carry a marker.
    unavailable_paths: HashSet<RepoPath>,
    /// Shared so that restoring a picked file can wait for the file list: in a merge-request
    /// check the row decides which two refs the file is read from.
    load_task: Shared<Task<()>>,
    _file_task: Option<Task<()>>,
    _selection_task: Option<Task<()>>,
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
        // A merge-request check of the same pair answers a different question, so it is not
        // the tab asked for.
        let existing = workspace.items_of_type::<RefComparison>(cx).find(|item| {
            let item = item.read(cx);
            item.repository == repository
                && item.base == base
                && item.head == head
                && item.mode == RefComparisonMode::Refs
        });
        if let Some(existing) = existing {
            workspace.activate_item(&existing, true, true, window, cx);
            return;
        }
        let project = workspace.project().clone();
        let comparison = cx.new(|cx| RefComparison::new(repository, project, base, head, cx));
        workspace.add_item_to_active_pane(Box::new(comparison), None, true, window, cx);
    }

    /// Opens the check that the merge-request branch `merge_request` carries everything `source`
    /// changed since it left `base`: only the files `source` touched are compared, and differences
    /// that come from `base` moving on or from the commits deliberately left out of the merge
    /// request (`excluded`) are labelled as such instead of being counted as missing.
    pub fn open_merge_request_check(
        workspace: &mut Workspace,
        repository: Entity<Repository>,
        base: SharedString,
        source: SharedString,
        merge_request: SharedString,
        excluded: Vec<git::Oid>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let excluded = normalize_excluded_commits(excluded);
        let existing = workspace.items_of_type::<RefComparison>(cx).find(|item| {
            let item = item.read(cx);
            item.repository == repository
                && item.base == source
                && item.head == merge_request
                && matches!(
                    &item.mode,
                    RefComparisonMode::MergeRequestCheck { merge_request_base, .. }
                        if *merge_request_base == base
                )
        });
        if let Some(existing) = existing {
            existing.update(cx, |comparison, cx| {
                comparison.recheck(excluded, window, cx)
            });
            workspace.activate_item(&existing, true, true, window, cx);
            return;
        }
        let project = workspace.project().clone();
        let mode = RefComparisonMode::MergeRequestCheck {
            merge_request_base: base,
            excluded,
        };
        let comparison = cx.new(|cx| {
            RefComparison::new_with_mode(repository, project, source, merge_request, mode, cx)
        });
        workspace.add_item_to_active_pane(Box::new(comparison), None, true, window, cx);
    }

    fn new(
        repository: Entity<Repository>,
        project: Entity<Project>,
        base: SharedString,
        head: SharedString,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_with_mode(repository, project, base, head, RefComparisonMode::Refs, cx)
    }

    fn new_with_mode(
        repository: Entity<Repository>,
        project: Entity<Project>,
        base: SharedString,
        head: SharedString,
        mode: RefComparisonMode,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut comparison = Self {
            focus_handle: cx.focus_handle(),
            repository,
            project,
            base,
            head,
            mode,
            state: RefComparisonState::Loading,
            show_all_files: false,
            selected_path: None,
            file_diff_editor: None,
            file_diff_error: None,
            unavailable_paths: HashSet::default(),
            load_task: Task::ready(()).shared(),
            _file_task: None,
            _selection_task: None,
        };
        comparison.reload(cx);
        comparison
    }

    /// Reopening a merge-request check recomputes it in place rather than opening a second tab
    /// under the same title. It is recomputed even for the same excluded commits: the check is
    /// asked for again right after the merge-request branch is rebuilt, and the result from
    /// before the rebuild would silently answer for a branch that no longer exists.
    fn recheck(&mut self, excluded: Vec<git::Oid>, window: &mut Window, cx: &mut Context<Self>) {
        let RefComparisonMode::MergeRequestCheck {
            excluded: current, ..
        } = &mut self.mode
        else {
            return;
        };
        *current = excluded;
        self.file_diff_editor = None;
        self.file_diff_error = None;
        self.unavailable_paths.clear();
        self._file_task = None;
        self.reload(cx);
        if let Some(selected_path) = self.selected_path.clone() {
            self.open_file_diff_once_loaded(selected_path, window, cx);
        }
        cx.emit(ItemEvent::UpdateTab);
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        self.state = RefComparisonState::Loading;
        // A selection waiting on the previous load would otherwise keep that load alive
        // through its handle, and its result could land after this one's.
        self._selection_task = None;
        let load_task = match &self.mode {
            RefComparisonMode::Refs => self.load_trees(cx),
            RefComparisonMode::MergeRequestCheck {
                merge_request_base,
                excluded,
            } => {
                let repository = self.repository.clone();
                let merge_request_base = merge_request_base.clone();
                let excluded = excluded.clone();
                let source = self.base.clone();
                let merge_request = self.head.clone();
                cx.spawn(async move |this, cx| {
                    let state = match load_merge_request_check(
                        repository,
                        merge_request_base,
                        source,
                        merge_request,
                        excluded,
                        cx,
                    )
                    .await
                    {
                        Ok(check) => RefComparisonState::MergeRequestChecked(check),
                        Err(error) => RefComparisonState::Failed(format!("{error:#}").into()),
                    };
                    this.update(cx, |this, cx| {
                        this.state = state;
                        cx.notify();
                    })
                    .ok();
                })
            }
        };
        self.load_task = load_task.shared();
        cx.notify();
    }

    fn load_trees(&mut self, cx: &mut Context<Self>) -> Task<()> {
        let tree = self.repository.update(cx, |repository, cx| {
            repository.diff_tree(
                DiffTreeType::Since {
                    base: self.base.clone(),
                    head: self.head.clone(),
                },
                cx,
            )
        });
        cx.spawn(async move |this, cx| {
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
        })
    }

    /// [`Self::open_file_diff`] once the file list is in, for a pick restored from the
    /// database before anything was loaded.
    fn open_file_diff_once_loaded(
        &mut self,
        path: RepoPath,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.selected_path = Some(path.clone());
        let load_task = self.load_task.clone();
        self._selection_task = Some(cx.spawn_in(window, async move |this, cx| {
            load_task.await;
            this.update_in(cx, |this, window, cx| {
                if this.selected_path.as_ref() == Some(&path) {
                    this.open_file_diff(path, window, cx);
                }
            })
            .ok();
        }));
    }

    /// Which refs `path` is read from. A merge-request check shows a file that only the merge
    /// request changed against its base rather than against the development branch, which
    /// never had it changed.
    fn file_diff_sides(&self, path: &RepoPath) -> Result<FileDiffSides, SharedString> {
        let (base, head) = (self.base.clone(), self.head.clone());
        match &self.state {
            RefComparisonState::Changed { entries, .. } => {
                let status = entries
                    .iter()
                    .find(|entry| entry.path == *path)
                    .map(|entry| &entry.status);
                Ok(FileDiffSides::listed(base, head, status))
            }
            RefComparisonState::MergeRequestChecked(check) => {
                let Some(row) = check.rows.iter().find(|row| row.path == *path) else {
                    return Ok(FileDiffSides::listed(base, head, None));
                };
                match row.verdict {
                    MergeRequestFileVerdict::OnlyInMergeRequest
                    | MergeRequestFileVerdict::BaseChangeMaybeDropped => {
                        let RefComparisonMode::MergeRequestCheck {
                            merge_request_base, ..
                        } = &self.mode
                        else {
                            return Ok(FileDiffSides::listed(base, head, row.status.as_ref()));
                        };
                        Ok(FileDiffSides::listed(
                            merge_request_base.clone(),
                            head,
                            row.status.as_ref(),
                        ))
                    }
                    MergeRequestFileVerdict::Matched { deleted: true } => Err(i18n::t!(
                        "Deleted on both the development branch and the MR branch."
                    )
                    .into()),
                    MergeRequestFileVerdict::Matched { deleted: false } => Ok(FileDiffSides {
                        left: base,
                        right: head,
                        left_may_be_missing: false,
                        right_may_be_missing: false,
                    }),
                    MergeRequestFileVerdict::SuspectedMissing
                    | MergeRequestFileVerdict::Explained { .. } => {
                        Ok(FileDiffSides::listed(base, head, row.status.as_ref()))
                    }
                }
            }
            RefComparisonState::Loading
            | RefComparisonState::Identical
            | RefComparisonState::Failed(_) => Ok(FileDiffSides::listed(base, head, None)),
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
        let sides = match self.file_diff_sides(&path) {
            Ok(sides) => sides,
            Err(reason) => {
                self.file_diff_error = Some(reason);
                self._file_task = None;
                return;
            }
        };
        let path_text = path.as_unix_str().to_string();
        let (base_text, head_text) = self.repository.update(cx, |repository, _cx| {
            (
                repository.file_at_ref(sides.left.to_string(), path_text.clone()),
                repository.file_at_ref(sides.right.to_string(), path_text.clone()),
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
            let base_may_be_missing = sides.left_may_be_missing;
            let head_may_be_missing = sides.right_may_be_missing;
            if (base_output.is_none() && !base_may_be_missing)
                || (head_output.is_none() && !head_may_be_missing)
                // Whatever the status is, a file that is listed as changed has
                // contents on at least one of the two sides.
                || (base_output.is_none() && head_output.is_none())
            {
                this.update(cx, |this, cx| {
                    this.file_diff_editor = None;
                    this.file_diff_error =
                        Some(i18n::t!("Could not get the file's contents.").into());
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
                    this.file_diff_error = Some(i18n::t!("Too large to show.").into());
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
                    this.file_diff_error =
                        Some(i18n::t!("Binary files cannot be shown side by side.").into());
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
        let merge_request_check = match &self.mode {
            RefComparisonMode::Refs => None,
            RefComparisonMode::MergeRequestCheck {
                merge_request_base,
                excluded,
            } => Some((
                merge_request_base.to_string(),
                persistence::serialize_excluded_commits(excluded),
            )),
        };

        let db = persistence::RefComparisonsDb::global(cx);
        cx.background_spawn(async move {
            match merge_request_check {
                None => {
                    db.save_ref_comparison(
                        item_id,
                        workspace_id,
                        repo_working_path,
                        base,
                        head,
                        selected_path,
                    )
                    .await
                }
                Some((merge_request_base, excluded)) => {
                    db.save_merge_request_check(
                        item_id,
                        workspace_id,
                        repo_working_path,
                        base,
                        head,
                        selected_path,
                        merge_request_base,
                        excluded,
                    )
                    .await
                }
            }
        })
    }

    /// Asks for the branch the merge request goes into and reopens this comparison as a
    /// merge-request check of the same two refs.
    fn reopen_as_merge_request_check(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = Workspace::for_window(window, cx) else {
            return;
        };
        let options = merge_request_base_candidates(
            &self.repository.read(cx).branch_list,
            &self.base,
            &self.head,
        );
        let prompt = crate::picker_prompt::prompt_even_for_one(
            i18n::t!("MR's Base (Merge Target) Branch"),
            options.clone(),
            workspace.downgrade(),
            window,
            cx,
        );
        let repository = self.repository.clone();
        let source = self.base.clone();
        let merge_request = self.head.clone();
        cx.spawn_in(window, async move |_this, cx| {
            let Some(merge_request_base) =
                prompt.await.and_then(|index| options.get(index).cloned())
            else {
                return;
            };
            workspace
                .update_in(cx, |workspace, window, cx| {
                    RefComparison::open_merge_request_check(
                        workspace,
                        repository,
                        merge_request_base,
                        source,
                        merge_request,
                        Vec::new(),
                        window,
                        cx,
                    );
                })
                .log_err();
        })
        .detach();
    }

    fn render_merge_request_header(&self, cx: &mut Context<Self>) -> AnyElement {
        let RefComparisonMode::MergeRequestCheck {
            merge_request_base,
            excluded,
        } = &self.mode
        else {
            return Empty.into_any_element();
        };
        let mut scope = i18n::t!(
            "Compares only the files {} changed after it branched off {merge_request_base} (the base).",
            self.base
        );
        if let RefComparisonState::MergeRequestChecked(check) = &self.state {
            scope.push(' ');
            scope.push_str(&i18n::t!(
                "Branched off at {}.",
                check.merge_base.display_short()
            ));
        }
        if !excluded.is_empty() {
            scope.push(' ');
            scope.push_str(&i18n::tn!(
                excluded.len(),
                "{count} excluded commit.",
                "{count} excluded commits."
            ));
        }
        let show_all_files = self.show_all_files;
        v_flex()
            .w_full()
            .p_2()
            .gap_0p5()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .justify_between()
                    .child(Label::new(i18n::t!(
                        "MR Check: {} (development branch) → {} (MR branch)",
                        self.base,
                        self.head
                    )))
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new(
                                    "merge-request-check-mismatched-only",
                                    i18n::t!("Mismatches Only"),
                                )
                                .label_size(LabelSize::Small)
                                .toggle_state(!show_all_files)
                                .on_click(cx.listener(
                                    |this, _, _, cx| {
                                        this.show_all_files = false;
                                        cx.notify();
                                    },
                                )),
                            )
                            .child(
                                Button::new(
                                    "merge-request-check-all",
                                    i18n::tc!("merge request check", "All"),
                                )
                                .label_size(LabelSize::Small)
                                .toggle_state(show_all_files)
                                .on_click(cx.listener(
                                    |this, _, _, cx| {
                                        this.show_all_files = true;
                                        cx.notify();
                                    },
                                )),
                            ),
                    ),
            )
            .child(Label::new(scope).size(LabelSize::Small).color(Color::Muted))
            .when_some(
                match &self.state {
                    RefComparisonState::MergeRequestChecked(check) => {
                        merge_request_behind_notice(check.base_commits_ahead)
                    }
                    _ => None,
                },
                |this, notice| {
                    this.child(
                        h_flex()
                            .gap_1()
                            .child(
                                Icon::new(IconName::Warning)
                                    .size(IconSize::XSmall)
                                    .color(Color::Warning),
                            )
                            .child(
                                Label::new(notice)
                                    .size(LabelSize::Small)
                                    .color(Color::Warning),
                            ),
                    )
                },
            )
            .when_some(
                match &self.state {
                    RefComparisonState::MergeRequestChecked(check) => Some(&check.counts),
                    _ => None,
                },
                |this, counts| {
                    let (verdict, verdict_color) = counts.verdict();
                    this.child(
                        h_flex()
                            .gap_2()
                            .flex_wrap()
                            .child(Label::new(verdict).color(verdict_color))
                            .child(
                                Label::new(counts.summary())
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            ),
                    )
                },
            )
            .into_any_element()
    }

    fn render_merge_request_file_list(
        &self,
        check: &MergeRequestCheck,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut list = v_flex()
            .id("merge-request-check-files")
            .h_full()
            .w(rems(22.))
            .flex_none()
            .overflow_y_scroll()
            .border_r_1()
            .border_color(cx.theme().colors().border_variant);
        let merge_request_is_behind = check.base_commits_ahead > 0;
        let mut previous_rank = None;
        for (index, row) in check.rows.iter().enumerate() {
            if !self.show_all_files
                && matches!(row.verdict, MergeRequestFileVerdict::Matched { .. })
            {
                continue;
            }
            let rank = row.verdict.rank();
            if row.verdict == MergeRequestFileVerdict::OnlyInMergeRequest
                && previous_rank != Some(rank)
            {
                list = list.child(
                    div().px_2().pt_2().pb_1().child(
                        Label::new(if merge_request_is_behind {
                            i18n::t!(
                                "Not from the development branch (includes newer base changes the MR branch lacks)"
                            )
                        } else {
                            i18n::t!(
                                "Not from the development branch (came in while resolving conflicts, for example)"
                            )
                        })
                        .size(LabelSize::XSmall)
                        .color(Color::Warning),
                    ),
                );
            }
            previous_rank = Some(rank);
            list = list.child(self.render_merge_request_row(
                index,
                row,
                check.history_truncated,
                merge_request_is_behind,
                cx,
            ));
        }
        list.when(check.omitted_count > 0, |this| {
            this.child(
                div().px_2().py_1().child(
                    Label::new(i18n::t!("{} more omitted", check.omitted_count))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                ),
            )
        })
        .into_any_element()
    }

    fn render_merge_request_row(
        &self,
        index: usize,
        row: &MergeRequestCheckRow,
        history_truncated: bool,
        merge_request_is_behind: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let path = row.path.clone();
        let is_selected = self.selected_path.as_ref() == Some(&row.path);
        let is_unavailable = self.unavailable_paths.contains(&row.path);
        let file_name = row
            .path
            .file_name()
            .map(|name| name.to_string())
            .unwrap_or_else(|| row.path.as_unix_str().to_string());
        let is_missing = row.verdict == MergeRequestFileVerdict::SuspectedMissing;
        let no_commits_reason = if history_truncated {
            i18n::t!(
                "The commits that touched it are not in the loaded history (the newest {MAX_MERGE_REQUEST_HISTORY_COMMITS} commits)."
            )
        } else {
            i18n::t!(
                "Could not tell which commits touched it (it may have come in through a merge)."
            )
            .to_string()
        };
        let description = row.verdict.description(merge_request_is_behind);
        ListItem::new(("merge-request-check-file", index))
            .spacing(ListItemSpacing::Sparse)
            .toggle_state(is_selected)
            .tooltip({
                let mut text = format!("{}\n{description}", row.path.as_unix_str());
                if row.merge_request_has_base_version {
                    text.push('\n');
                    text.push_str(i18n::tr(MERGE_REQUEST_HAS_BASE_VERSION_NOTE));
                }
                move |_, cx| Tooltip::simple(text.clone(), cx)
            })
            .child(
                v_flex()
                    .min_w_0()
                    .child(Label::new(file_name).truncate())
                    .child(
                        Label::new(row.verdict.label())
                            .size(LabelSize::XSmall)
                            .color(row.verdict.color()),
                    )
                    .when(row.merge_request_has_base_version, |this| {
                        this.child(
                            Label::new(i18n::tr(MERGE_REQUEST_HAS_BASE_VERSION_NOTE))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                    })
                    .when(is_missing, |this| {
                        this.children(row.commits.iter().map(|commit| {
                            h_flex()
                                .gap_1()
                                .min_w_0()
                                .child(
                                    Label::new(commit.sha.display_short())
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted)
                                        .buffer_font(cx),
                                )
                                .child(
                                    Label::new(commit.subject.clone())
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted)
                                        .truncate(),
                                )
                        }))
                        .when(row.omitted_commit_count > 0, |this| {
                            this.child(
                                Label::new(i18n::tn!(
                                    row.omitted_commit_count,
                                    "{count} more commit",
                                    "{count} more commits"
                                ))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                            )
                        })
                        .when(row.commits.is_empty(), |this| {
                            this.child(
                                Label::new(no_commits_reason)
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                        })
                    }),
            )
            .when(is_unavailable, |this| {
                this.end_slot(
                    Icon::new(IconName::Warning)
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                )
            })
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_file_diff(path.clone(), window, cx);
            }))
            .into_any_element()
    }

    fn render_merge_request_file_pane(&self, check: &MergeRequestCheck) -> AnyElement {
        if let Some(error) = &self.file_diff_error {
            return h_flex()
                .flex_1()
                .size_full()
                .justify_center()
                .child(Label::new(error.clone()).color(Color::Muted))
                .into_any_element();
        }
        if let Some(editor) = self.file_diff_editor.clone() {
            // Which refs the two sides are depends on the row, so it is spelled out.
            let sides = self
                .selected_path
                .as_ref()
                .and_then(|path| self.file_diff_sides(path).ok());
            return v_flex()
                .flex_1()
                .size_full()
                .when_some(sides, |this, sides| {
                    this.child(
                        div().px_2().py_1().child(
                            Label::new(i18n::t!("Left: {} / Right: {}", sides.left, sides.right))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        ),
                    )
                })
                .child(div().flex_1().min_h_0().w_full().child(editor))
                .into_any_element();
        }
        let has_listed_rows = check.rows.iter().any(|row| {
            self.show_all_files || !matches!(row.verdict, MergeRequestFileVerdict::Matched { .. })
        });
        let message = if has_listed_rows {
            i18n::t!(
                "Choose a file from the list on the left to show the development branch (left) and the MR branch (right) side by side. Rows in the warning color show the base (left) and the MR branch (right)."
            )
        } else if check.rows.is_empty() {
            i18n::t!("No files changed on the development branch.")
        } else {
            i18n::t!("No files mismatch. \"All\" also shows the matching files.")
        };
        v_flex()
            .flex_1()
            .size_full()
            .justify_center()
            .items_center()
            .gap_1()
            .child(Label::new(message).color(Color::Muted))
            .when(has_listed_rows, |this| {
                // `diff-tree` runs with `--no-renames`, so a renamed file shows up twice.
                this.child(
                    Label::new(i18n::t!(
                        "A renamed file is split into two rows: its old name and its new name."
                    ))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
            })
            .into_any_element()
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
                        Label::new(i18n::t!("{} more omitted", omitted_count))
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
        match self.mode {
            RefComparisonMode::Refs => {
                i18n::t!("Comparison of {} and {}", self.base, self.head).into()
            }
            RefComparisonMode::MergeRequestCheck { .. } => {
                i18n::t!("MR Check {} → {}", self.base, self.head).into()
            }
        }
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
        let is_merge_request_check = self.mode != RefComparisonMode::Refs;
        let header = if is_merge_request_check {
            self.render_merge_request_header(cx)
        } else {
            // Rendered often, so the branch list is only scanned, not copied into candidates.
            let has_merge_request_bases =
                self.repository.read(cx).branch_list.iter().any(|branch| {
                    let name = branch.name();
                    name != self.base.as_ref() && name != self.head.as_ref()
                });
            h_flex()
                .w_full()
                .p_2()
                .gap_2()
                .justify_between()
                .border_b_1()
                .border_color(cx.theme().colors().border_variant)
                .child(
                    v_flex()
                        .gap_0p5()
                        .child(Label::new(i18n::t!(
                            "Comparison of {} (source) and {} (target)",
                            self.base, self.head
                        )))
                        .child(
                            Label::new(i18n::t!(
                                "Compares the contents of two branches or tags themselves. If there are no differences, everything has been ported."
                            ))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                        ),
                )
                .child(
                    Button::new("ref-comparison-open-merge-request-check", i18n::t!("View as MR Check…"))
                        .label_size(LabelSize::Small)
                        .disabled(!has_merge_request_bases)
                        .tooltip(Tooltip::text(i18n::t!(
                            "Choose the MR's base (merge target) and reopen as a comparison that the base's own changes do not bury."
                        )))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.reopen_as_merge_request_check(window, cx);
                        })),
                )
                .into_any_element()
        };

        let body = match &self.state {
            RefComparisonState::Loading => h_flex()
                .size_full()
                .justify_center()
                .child(
                    Label::new(i18n::t!("Loading comparison…"))
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
                    Label::new(i18n::t!("No differences (everything has been ported)"))
                        .color(Color::Success)
                        .size(LabelSize::Large),
                )
                .child(
                    Label::new(i18n::t!(
                        "{} and {} have the same contents.",
                        self.base,
                        self.head
                    ))
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
                    Label::new(if is_merge_request_check {
                        i18n::t!("Could not run the MR check")
                    } else {
                        i18n::t!("Could not compare")
                    })
                    .color(Color::Error)
                    .size(LabelSize::Large),
                )
                .child(
                    Label::new(error.clone())
                        .color(Color::Muted)
                        .size(LabelSize::Small),
                )
                .into_any_element(),
            RefComparisonState::MergeRequestChecked(check) => h_flex()
                .size_full()
                .child(self.render_merge_request_file_list(check, cx))
                .child(self.render_merge_request_file_pane(check))
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
                                Label::new(i18n::tn!(
                                    count,
                                    "{count} file differs. Choose it from the list on the left to show the contents side by side.",
                                    "{count} files differ. Choose one from the list on the left to show the contents side by side."
                                ))
                                .color(Color::Muted),
                            )
                            // `diff-tree` runs with `--no-renames`, so a renamed
                            // file shows up twice and the count looks inflated.
                            .child(
                                Label::new(i18n::t!(
                                    "A renamed file is split into two rows, \"Deleted\" and \"Added\"."
                                ))
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
        let Some((repo_working_path, base, head, selected_path, merge_request_base, excluded)) =
            db.get_ref_comparison(item_id, workspace_id).ok().flatten()
        else {
            return Task::ready(Err(anyhow!("no ref comparison to restore")));
        };
        let mode = match merge_request_base {
            None => RefComparisonMode::Refs,
            Some(merge_request_base) => {
                // Restoring with a different set of excluded commits would quietly turn what
                // the user left out on purpose into files suspected missing.
                let excluded = match persistence::deserialize_excluded_commits(
                    excluded.as_deref().unwrap_or_default(),
                ) {
                    Ok(excluded) => excluded,
                    Err(error) => return Task::ready(Err(error)),
                };
                RefComparisonMode::MergeRequestCheck {
                    merge_request_base: merge_request_base.into(),
                    excluded,
                }
            }
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
                    anyhow::bail!("repository not found: {path:?}");
                };

                let comparison = cx.new(|cx| {
                    RefComparison::new_with_mode(
                        repository,
                        project.clone(),
                        base.into(),
                        head.into(),
                        mode,
                        cx,
                    )
                });

                if let Some(selected_path) =
                    selected_path.and_then(|value| RepoPath::new(&value).ok())
                {
                    comparison.update(cx, |comparison, cx| {
                        comparison.open_file_diff_once_loaded(selected_path, window, cx);
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

    use anyhow::{Context as _, Result};
    use db::{
        query,
        sqlez::{domain::Domain, thread_safe_connection::ThreadSafeConnection},
        sqlez_macros::sql,
    };
    use workspace::WorkspaceDb;

    pub struct RefComparisonsDb(ThreadSafeConnection);

    impl Domain for RefComparisonsDb {
        const NAME: &str = stringify!(RefComparisonsDb);

        const MIGRATIONS: &[&str] = &[
            sql!(
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
            ),
            // A merge-request check: NULL `merge_request_base` is a plain comparison.
            sql!(
                ALTER TABLE ref_comparisons ADD COLUMN merge_request_base TEXT;
                ALTER TABLE ref_comparisons ADD COLUMN excluded_commits TEXT;
            ),
        ];
    }

    db::static_connection!(RefComparisonsDb, [WorkspaceDb]);

    /// Full SHAs separated by spaces.
    pub fn serialize_excluded_commits(excluded: &[git::Oid]) -> String {
        excluded
            .iter()
            .map(|sha| sha.to_string())
            .collect::<Vec<_>>()
            .join(" ")
    }

    pub fn deserialize_excluded_commits(text: &str) -> Result<Vec<git::Oid>> {
        let excluded = text
            .split_whitespace()
            .map(|sha| {
                sha.parse::<git::Oid>()
                    .with_context(|| format!("could not read a saved excluded commit: {sha}"))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(super::normalize_excluded_commits(excluded))
    }

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
            pub async fn save_merge_request_check(
                item_id: workspace::ItemId,
                workspace_id: workspace::WorkspaceId,
                repo_working_path: String,
                base: String,
                head: String,
                selected_path: Option<String>,
                merge_request_base: String,
                excluded_commits: String
            ) -> Result<()> {
                INSERT OR REPLACE INTO ref_comparisons(
                    item_id,
                    workspace_id,
                    repo_working_path,
                    base,
                    head,
                    selected_path,
                    merge_request_base,
                    excluded_commits
                )
                VALUES (?, ?, ?, ?, ?, ?, ?, ?)
            }
        }

        query! {
            pub fn get_ref_comparison(
                item_id: workspace::ItemId,
                workspace_id: workspace::WorkspaceId
            ) -> Result<Option<(
                PathBuf,
                String,
                String,
                Option<String>,
                Option<String>,
                Option<String>
            )>> {
                SELECT
                    repo_working_path,
                    base,
                    head,
                    selected_path,
                    merge_request_base,
                    excluded_commits
                FROM ref_comparisons
                WHERE item_id = ? AND workspace_id = ?
            }
        }
    }
}
