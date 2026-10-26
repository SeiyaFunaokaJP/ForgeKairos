use crate::commit_view::CommitView;
use crate::git_terms::{
    RESET_HARD_SUMMARY, RESET_MIXED_SUMMARY, RESET_SOFT_SUMMARY, commit_operation_help, help_aside,
};
use anyhow::{Result, anyhow};
use editor::Editor;
use futures::channel::oneshot;
use git::Oid;
use git::repository::{
    FixupChanges, FixupOutcome, FixupTarget, GitCommandOutput, RepoPath, ResetMode,
};
use git::status::{FileStatus, StatusCode};
use git_ui_core::notifications::{open_output, show_error_toast};
use gpui::{
    Action, AsyncApp, ClipboardItem, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    PromptLevel, SharedString, WeakEntity, Window, actions,
};
use menu::{Cancel, Confirm, SecondaryConfirm, SelectNext, SelectPrevious};
use notifications::status_toast::StatusToast;
use project::{
    GIT_COMMAND_TASK_TAG,
    git_store::{ChangesLeftInStash, Repository},
};
use std::rc::Rc;

use task::{TaskContext, TaskVariables, VariableName};
use ui::{
    Color, ContextMenu, ContextMenuEntry, DocumentationSide, Headline, HeadlineSize, IconName,
    IconPosition, IconSize, ListItem, Tooltip, prelude::*,
};
use util::ResultExt as _;
use workspace::notifications::{NotificationId, simple_message_notification::MessageNotification};
use workspace::{ModalView, Workspace};

actions!(
    git_graph,
    [
        /// Copies the SHA of the selected commit to the clipboard.
        CopyCommitSha,
        /// Copies a tag from the selected commit to the clipboard.
        CopyCommitTag,
        /// Opens the commit view for the selected commit.
        OpenCommitView,
    ]
);

const COMMIT_TAG_LIST_WIDTH_IN_REMS: Rems = rems(10.);
pub(crate) const CUSTOM_GIT_COMMANDS_DOCS_SLUG: &str = "tasks#custom-git-commands";

pub(crate) struct CommitContextMenuData {
    pub(crate) sha: Oid,
    pub(crate) tag_names: Vec<SharedString>,
    /// `None` where the caller has not loaded the commit's parents.
    pub(crate) is_merge: Option<bool>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommitContextMenuSource {
    GitGraph,
    GitPanel,
}

/// `extra_entries` adds what only the caller knows about the commit, after the entries that
/// change the repository.
pub(crate) fn commit_context_menu(
    commit: CommitContextMenuData,
    source: CommitContextMenuSource,
    ref_name: Option<SharedString>,
    focus_handle: FocusHandle,
    repository: Option<WeakEntity<Repository>>,
    workspace: WeakEntity<Workspace>,
    extra_entries: impl FnOnce(ContextMenu) -> ContextMenu + 'static,
    window: &mut Window,
    cx: &mut App,
) -> Entity<ContextMenu> {
    let sha = commit.sha;
    let sha_short = sha.display_short();
    let git_tasks = git_context_menu_tasks(
        git_task_context(&repository, sha, ref_name.as_deref(), cx),
        &workspace,
        cx,
    );
    let header = match &ref_name {
        Some(ref_name) => i18n::t!("Ref {ref_name}"),
        None => i18n::t!("Commit {sha_short}"),
    };
    let facts = repository
        .as_ref()
        .and_then(|repository| repository.upgrade())
        .map(|repository| CommitFacts::read(repository.read(cx), sha, commit.is_merge))
        .unwrap_or_default();

    ContextMenu::build(window, cx, move |context_menu, _, _| {
        context_menu
            .context(focus_handle)
            .header(header)
            .entry(i18n::t!("View Diff"), Some(OpenCommitView.boxed_clone()), {
                let repository = repository.clone();
                let workspace = workspace.clone();
                move |window, cx| {
                    let Some(repository) = repository.clone() else {
                        return;
                    };
                    CommitView::open(
                        sha.to_string(),
                        repository,
                        workspace.clone(),
                        None,
                        None,
                        window,
                        cx,
                    );
                }
            })
            .entry(
                i18n::t!("Copy SHA"),
                Some(CopyCommitSha.boxed_clone()),
                move |_window, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(sha.to_string()));
                },
            )
            .when_some(ref_name.clone(), |menu, ref_name| {
                menu.entry(i18n::t!("Copy Ref Name"), None, move |_window, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(ref_name.to_string()));
                })
            })
            .when(ref_name.is_none(), |menu| {
                menu.map(|menu| {
                    let tag_names = commit.tag_names.clone();
                    let copy_tag_label = i18n::t!("Copy Tag");

                    match tag_names.as_slice() {
                        [] => menu.item(
                            ContextMenuEntry::new(copy_tag_label)
                                .action(CopyCommitTag.boxed_clone())
                                .disabled(true),
                        ),
                        [tag_name] => {
                            let tag_name = tag_name.clone();
                            let label = format!("{copy_tag_label}: {tag_name}");
                            menu.entry(
                                label,
                                Some(CopyCommitTag.boxed_clone()),
                                move |_window, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        tag_name.to_string(),
                                    ));
                                },
                            )
                        }
                        _ => menu.submenu(copy_tag_label, move |menu, _window, _cx| {
                            let mut menu = menu.fixed_width(COMMIT_TAG_LIST_WIDTH_IN_REMS.into());

                            for tag_name in tag_names.clone() {
                                let tag_name_to_copy = tag_name.clone();
                                menu = menu.entry(tag_name, None, move |_window, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        tag_name_to_copy.to_string(),
                                    ));
                                });
                            }
                            menu
                        }),
                    }
                })
            })
            .when(source == CommitContextMenuSource::GitPanel, |menu| {
                menu.entry(i18n::t!("Show in Git Graph"), None, move |window, cx| {
                    window.dispatch_action(
                        Box::new(crate::git_graph::OpenAtCommit {
                            sha: sha.to_string(),
                        }),
                        cx,
                    );
                })
            })
            .when_some(repository.clone(), |menu, repository| {
                let workspace = workspace.clone();
                let facts = facts.clone();
                menu.map(move |menu| {
                    commit_rewrite_entries(menu, sha, repository, workspace, facts)
                })
            })
            .map(extra_entries)
            .map(|mut menu| {
                menu = menu.separator().header(i18n::t!("Custom Commands"));

                if git_tasks.is_empty() {
                    return menu.item(
                        ContextMenuEntry::new(i18n::t!("Learn More"))
                            .icon(IconName::ArrowUpRight)
                            .icon_color(Color::Muted)
                            .icon_position(IconPosition::End)
                            .handler(|_window, cx| {
                                let docs_url =
                                    release_channel::docs_url(CUSTOM_GIT_COMMANDS_DOCS_SLUG, cx);
                                cx.open_url(&docs_url);
                            }),
                    );
                }

                for (task_source_kind, resolved_task) in git_tasks {
                    let label = resolved_task.display_label().to_string();
                    let workspace = workspace.clone();
                    menu = menu.entry(label, None, move |window, cx| {
                        workspace
                            .update(cx, |workspace, cx| {
                                workspace.schedule_resolved_task(
                                    task_source_kind.clone(),
                                    resolved_task.clone(),
                                    false,
                                    window,
                                    cx,
                                );
                            })
                            .ok();
                    });
                }

                menu
            })
    })
}

/// One of the operations the commit menu offers, in the shape its entries and the workspace
/// actions below both hand the repository and the commit to.
type CommitOperation = fn(Entity<Repository>, Entity<Workspace>, String, &mut Window, &mut App);

/// The commit menu's operations by name, so that other entry points (the graph's button row)
/// run exactly what the menu item runs and name the same action for its key binding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommitMenuOperation {
    CreateBranch,
    Checkout,
    CreateTag,
    CherryPick,
    Revert,
    Reword,
    Fixup,
    Drop,
    ResetSoft,
    ResetMixed,
    ResetHard,
    CopyTagDistance,
}

impl CommitMenuOperation {
    pub(crate) fn action(self, sha: &str) -> Box<dyn Action> {
        let sha = sha.to_string();
        match self {
            Self::CreateBranch => git::CreateBranchFrom { sha }.boxed_clone(),
            Self::Checkout => git::CheckoutCommit { sha }.boxed_clone(),
            Self::CreateTag => git::CreateTagAt { sha }.boxed_clone(),
            Self::CherryPick => git::CherryPickCommit { sha }.boxed_clone(),
            Self::Revert => git::RevertCommit { sha }.boxed_clone(),
            Self::Reword => git::RewordCommit { sha }.boxed_clone(),
            Self::Fixup => git::FixupCommit { sha }.boxed_clone(),
            Self::Drop => git::DropCommit { sha }.boxed_clone(),
            Self::ResetSoft => git::ResetToCommit {
                sha,
                mode: git::ResetToCommitMode::Soft,
            }
            .boxed_clone(),
            Self::ResetMixed => git::ResetToCommit {
                sha,
                mode: git::ResetToCommitMode::Mixed,
            }
            .boxed_clone(),
            Self::ResetHard => git::ResetToCommit {
                sha,
                mode: git::ResetToCommitMode::Hard,
            }
            .boxed_clone(),
            Self::CopyTagDistance => git::CopyTagDistance { sha }.boxed_clone(),
        }
    }

    fn operation(self) -> CommitOperation {
        match self {
            Self::CreateBranch => create_branch_from_commit,
            Self::Checkout => checkout_commit,
            Self::CreateTag => create_tag_at_commit,
            Self::CherryPick => cherry_pick_commit,
            Self::Revert => revert_commit,
            Self::Reword => reword_commit,
            Self::Fixup => fixup_choosing_changes,
            Self::Drop => drop_commit,
            Self::ResetSoft => |repository, workspace, sha, window, cx| {
                reset_to_commit(repository, workspace, sha, ResetMode::Soft, window, cx)
            },
            Self::ResetMixed => |repository, workspace, sha, window, cx| {
                reset_to_commit(repository, workspace, sha, ResetMode::Mixed, window, cx)
            },
            Self::ResetHard => |repository, workspace, sha, window, cx| {
                reset_to_commit(repository, workspace, sha, ResetMode::Hard, window, cx)
            },
            Self::CopyTagDistance => copy_tag_distance,
        }
    }
}

/// What the commit menu and the graph's buttons know about a commit before running anything on
/// it, which is enough to grey out an operation that cannot work. Everything else is refused by
/// the operation itself when it runs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CommitFacts {
    /// The commit is the one checked out.
    pub(crate) is_head: bool,
    /// `None` where the caller has not loaded the commit's parents.
    pub(crate) is_merge: Option<bool>,
    /// Why a fixup cannot be started at all right now (see [`fixup_menu_refusal`]).
    pub(crate) fixup_refusal: Option<SharedString>,
}

impl CommitFacts {
    pub(crate) fn read(repository: &Repository, sha: Oid, is_merge: Option<bool>) -> Self {
        let is_head = repository
            .head_commit
            .as_ref()
            .is_some_and(|head| Oid::try_from(head.sha.as_ref()).ok() == Some(sha));
        Self {
            is_head,
            is_merge,
            fixup_refusal: fixup_menu_refusal(repository),
        }
    }
}

/// Why `operation` cannot run on the commit `facts` describes, or `None` when it can be tried.
///
/// Cherry-pick and revert are run without `-m`, which git refuses for a merge commit because it
/// cannot tell which parent the change is measured against.
pub(crate) fn commit_operation_refusal(
    operation: CommitMenuOperation,
    facts: &CommitFacts,
) -> Option<SharedString> {
    match operation {
        CommitMenuOperation::CherryPick if facts.is_head => Some(
            i18n::t!("This is the tip of the current branch. There is nothing to pick.").into(),
        ),
        CommitMenuOperation::CherryPick if facts.is_merge == Some(true) => {
            Some(i18n::t!("A merge commit cannot be cherry-picked.").into())
        }
        CommitMenuOperation::Revert if facts.is_merge == Some(true) => {
            Some(i18n::t!("A merge commit cannot be reverted.").into())
        }
        CommitMenuOperation::Fixup => facts.fixup_refusal.clone(),
        _ => None,
    }
}

/// Runs one of the commit menu's operations on `sha` of `repository`, the way its menu item does.
pub(crate) fn run_commit_menu_operation(
    operation: CommitMenuOperation,
    repository: Entity<Repository>,
    workspace: Entity<Workspace>,
    sha: String,
    window: &mut Window,
    cx: &mut App,
) {
    (operation.operation())(repository, workspace, sha, window, cx);
}

/// The entries that change the repository rather than only read the commit. They need a
/// repository, so a menu opened where none could be resolved keeps just the copy items.
///
/// An item that [`commit_operation_refusal`] refuses for `facts` is greyed out with the reason
/// beside it.
fn commit_rewrite_entries(
    menu: ContextMenu,
    sha: Oid,
    repository: WeakEntity<Repository>,
    workspace: WeakEntity<Workspace>,
    facts: CommitFacts,
) -> ContextMenu {
    let sha = sha.to_string();
    let entry = |menu: ContextMenu, label: &str, operation: CommitMenuOperation| {
        let help = commit_operation_help(operation);
        match commit_operation_refusal(operation, &facts) {
            Some(reason) => menu.item(
                ContextMenuEntry::new(label.to_string())
                    .action(operation.action(&sha))
                    .disabled(true)
                    .documentation_aside(DocumentationSide::Right, help_aside(help, Some(reason))),
            ),
            None => menu.item(
                ContextMenuEntry::new(label.to_string())
                    .action(operation.action(&sha))
                    .handler(commit_action_handler(
                        &repository,
                        &workspace,
                        &sha,
                        operation,
                    ))
                    .documentation_aside(DocumentationSide::Right, help_aside(help, None)),
            ),
        }
    };
    let fixup_label = i18n::t!("Fold Current Changes into This Commit (fixup)…");
    let menu = menu.separator();
    let menu = entry(
        menu,
        i18n::t!("Create Branch Here…"),
        CommitMenuOperation::CreateBranch,
    );
    let menu = entry(
        menu,
        i18n::t!("Check Out This Commit (detached)"),
        CommitMenuOperation::Checkout,
    );
    let menu = entry(menu, i18n::t!("Tag Here…"), CommitMenuOperation::CreateTag);
    let menu = menu.separator();
    let menu = entry(
        menu,
        i18n::t!("Cherry-Pick This Commit onto the Current Branch"),
        CommitMenuOperation::CherryPick,
    );
    let menu = entry(
        menu,
        i18n::t!("Revert This Commit"),
        CommitMenuOperation::Revert,
    );
    let menu = entry(
        menu,
        i18n::t!("Reword Message…"),
        CommitMenuOperation::Reword,
    );
    let menu = entry(menu, fixup_label, CommitMenuOperation::Fixup);
    let menu = entry(
        menu,
        i18n::t!("Drop This Commit…"),
        CommitMenuOperation::Drop,
    );
    let menu = menu.separator();
    let menu = entry(
        menu,
        &i18n::t!(
            "Reset to This Commit (soft: {summary})",
            summary = i18n::tr(RESET_SOFT_SUMMARY)
        ),
        CommitMenuOperation::ResetSoft,
    );
    let menu = entry(
        menu,
        &i18n::t!(
            "Reset to This Commit (mixed: {summary})",
            summary = i18n::tr(RESET_MIXED_SUMMARY)
        ),
        CommitMenuOperation::ResetMixed,
    );
    let menu = entry(
        menu,
        &i18n::t!(
            "Reset to This Commit (hard: {summary})…",
            summary = i18n::tr(RESET_HARD_SUMMARY)
        ),
        CommitMenuOperation::ResetHard,
    );
    let menu = menu.separator();
    entry(
        menu,
        i18n::t!("Copy Distance from Latest Tag"),
        CommitMenuOperation::CopyTagDistance,
    )
}

fn commit_action_handler(
    repository: &WeakEntity<Repository>,
    workspace: &WeakEntity<Workspace>,
    sha: &str,
    operation: CommitMenuOperation,
) -> impl Fn(&mut Window, &mut App) + 'static {
    let repository = repository.clone();
    let workspace = workspace.clone();
    let sha = sha.to_string();
    move |window, cx| {
        let (Some(repository), Some(workspace)) = (repository.upgrade(), workspace.upgrade())
        else {
            return;
        };
        run_commit_menu_operation(operation, repository, workspace, sha.clone(), window, cx);
    }
}

/// Makes the commit menu's items reachable from the keymap and the command palette as well,
/// against the project's active repository.
pub(crate) fn register_commit_actions(workspace: &mut Workspace) {
    fn run(
        workspace: &mut Workspace,
        sha: &str,
        operation: CommitMenuOperation,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let Some(repository) = workspace.project().read(cx).active_repository(cx) else {
            return;
        };
        let Some(sha) = commit_for_action(workspace, sha, &repository, cx) else {
            let toast = commit_status_toast(
                i18n::t!("Choose a commit (right-click one in the history tab or the Git Graph)"),
                IconName::GitBranch,
                Color::Muted,
                cx,
            );
            workspace.toggle_status_toast(toast, cx);
            return;
        };
        let workspace = cx.entity();
        // Out of the workspace's own update: the operations that open a modal update the
        // workspace again, which is not allowed from inside this handler.
        window.defer(cx, move |window, cx| {
            run_commit_menu_operation(operation, repository, workspace, sha, window, cx);
        });
    }

    /// The commit an action carrying no SHA works on: what the history tab has selected, and
    /// failing that HEAD, so that the palette entries are not dead ends.
    fn commit_for_action(
        workspace: &Workspace,
        sha: &str,
        repository: &Entity<Repository>,
        cx: &App,
    ) -> Option<String> {
        if !sha.is_empty() {
            return Some(sha.to_string());
        }
        workspace
            .panel::<crate::git_panel::GitPanel>(cx)
            .and_then(|panel| panel.read(cx).selected_history_commit())
            .or_else(|| {
                repository
                    .read(cx)
                    .head_commit
                    .as_ref()
                    .map(|commit| commit.sha.to_string())
            })
    }

    workspace.register_action(|workspace, action: &git::CreateBranchFrom, window, cx| {
        run(
            workspace,
            &action.sha,
            CommitMenuOperation::CreateBranch,
            window,
            cx,
        );
    });
    workspace.register_action(|workspace, action: &git::CheckoutCommit, window, cx| {
        run(
            workspace,
            &action.sha,
            CommitMenuOperation::Checkout,
            window,
            cx,
        );
    });
    workspace.register_action(|workspace, action: &git::CreateTagAt, window, cx| {
        run(
            workspace,
            &action.sha,
            CommitMenuOperation::CreateTag,
            window,
            cx,
        );
    });
    workspace.register_action(|workspace, action: &git::CherryPickCommit, window, cx| {
        run(
            workspace,
            &action.sha,
            CommitMenuOperation::CherryPick,
            window,
            cx,
        );
    });
    workspace.register_action(|workspace, action: &git::RevertCommit, window, cx| {
        run(
            workspace,
            &action.sha,
            CommitMenuOperation::Revert,
            window,
            cx,
        );
    });
    workspace.register_action(|workspace, action: &git::RewordCommit, window, cx| {
        run(
            workspace,
            &action.sha,
            CommitMenuOperation::Reword,
            window,
            cx,
        );
    });
    workspace.register_action(|workspace, action: &git::DropCommit, window, cx| {
        run(
            workspace,
            &action.sha,
            CommitMenuOperation::Drop,
            window,
            cx,
        );
    });
    workspace.register_action(|workspace, action: &git::FixupCommit, window, cx| {
        run(
            workspace,
            &action.sha,
            CommitMenuOperation::Fixup,
            window,
            cx,
        );
    });
    workspace.register_action(|workspace, action: &git::CopyTagDistance, window, cx| {
        run(
            workspace,
            &action.sha,
            CommitMenuOperation::CopyTagDistance,
            window,
            cx,
        );
    });
    workspace.register_action(|workspace, action: &git::ResetToCommit, window, cx| {
        let operation = match action.mode {
            git::ResetToCommitMode::Soft => CommitMenuOperation::ResetSoft,
            git::ResetToCommitMode::Mixed => CommitMenuOperation::ResetMixed,
            git::ResetToCommitMode::Hard => CommitMenuOperation::ResetHard,
        };
        run(workspace, &action.sha, operation, window, cx);
    });
}

fn short_sha(sha: &str) -> String {
    sha.chars().take(git::SHORT_SHA_LENGTH).collect()
}

fn create_branch_from_commit(
    repository: Entity<Repository>,
    workspace: Entity<Workspace>,
    sha: String,
    window: &mut Window,
    cx: &mut App,
) {
    let title = i18n::t!("Create Branch from {}", short_sha(&sha));
    let host = workspace.clone();
    open_commit_text_modal(
        &host,
        CommitTextModalOptions {
            title: title.into(),
            name_placeholder: Some(i18n::t!("New branch name").into()),
            message: None,
            confirm_label: i18n::t!("Create…").into(),
        },
        move |input, window, cx| {
            let name = input.name.trim().to_string();
            if name.is_empty() {
                return;
            }
            confirm_and_create_branch(repository, workspace, name, sha, window, cx);
        },
        window,
        cx,
    );
}

/// Asks whether to switch to the new branch before creating it, so the answer is part of the same
/// decision rather than a surprise afterwards.
fn confirm_and_create_branch(
    repository: Entity<Repository>,
    workspace: Entity<Workspace>,
    name: String,
    sha: String,
    window: &mut Window,
    cx: &mut App,
) {
    let prompt = window.prompt(
        PromptLevel::Info,
        &i18n::t!("Create branch \"{name}\"?"),
        Some(&i18n::t!("Base commit: {}", short_sha(&sha))),
        &[
            i18n::t!("Create and Switch"),
            i18n::t!("Create Only"),
            i18n::t!("Cancel"),
        ],
        cx,
    );
    cx.spawn(async move |cx| {
        let Some(answer) = prompt.await.ok().filter(|answer| *answer < 2) else {
            return;
        };
        let switch_to_branch = answer == 0;
        let created = repository.update(cx, |repository, _| {
            repository.create_branch(name.clone(), Some(sha.clone()))
        });
        if let Err(error) = await_receiver(created).await {
            cx.update(|cx| show_error_toast(workspace.clone(), "branch", error, cx));
            return;
        }
        if !switch_to_branch {
            report_success(&workspace, i18n::t!("Created branch \"{name}\"."), cx);
            return;
        }
        let switched =
            repository.update(cx, |repository, _| repository.change_branch(name.clone()));
        match await_receiver(switched).await {
            Ok(()) => report_success(
                &workspace,
                i18n::t!("Created branch \"{name}\" and switched to it."),
                cx,
            ),
            Err(error) => {
                cx.update(|cx| show_error_toast(workspace.clone(), "switch", error, cx));
            }
        }
    })
    .detach();
}

fn checkout_commit(
    repository: Entity<Repository>,
    workspace: Entity<Workspace>,
    sha: String,
    _window: &mut Window,
    cx: &mut App,
) {
    let receiver = repository.update(cx, |repository, cx| {
        repository.checkout_detached(sha.clone(), cx)
    });
    report_command(
        workspace,
        "checkout",
        i18n::t!("Checked out {} (detached HEAD).", short_sha(&sha)),
        receiver,
        None,
        cx,
    );
}

fn create_tag_at_commit(
    repository: Entity<Repository>,
    workspace: Entity<Workspace>,
    sha: String,
    window: &mut Window,
    cx: &mut App,
) {
    let title = i18n::t!("Create Tag at {}", short_sha(&sha));
    let host = workspace.clone();
    open_commit_text_modal(
        &host,
        CommitTextModalOptions {
            title: title.into(),
            name_placeholder: Some(i18n::t!("Tag name (e.g. v1.2.3)").into()),
            message: Some((
                i18n::t!("Message (leave empty for a lightweight tag)").into(),
                String::new(),
            )),
            confirm_label: i18n::t!("Create Tag").into(),
        },
        move |input, _window, cx| {
            let name = input.name.trim().to_string();
            if name.is_empty() {
                return;
            }
            let message = input.message.trim().to_string();
            let message = (!message.is_empty()).then_some(message);
            let receiver = repository.update(cx, |repository, cx| {
                repository.create_tag_with_message(name.clone(), sha.clone(), message, cx)
            });
            report_command(
                workspace,
                "tag",
                i18n::t!("Created tag \"{name}\"."),
                receiver,
                None,
                cx,
            );
        },
        window,
        cx,
    );
}

fn cherry_pick_commit(
    repository: Entity<Repository>,
    workspace: Entity<Workspace>,
    sha: String,
    _window: &mut Window,
    cx: &mut App,
) {
    let receiver = repository.update(cx, |repository, cx| {
        repository.cherry_pick(vec![sha.clone()], false, cx)
    });
    report_command(
        workspace,
        "cherry-pick",
        i18n::t!("Cherry-picked {}.", short_sha(&sha)),
        receiver,
        None,
        cx,
    );
}

fn revert_commit(
    repository: Entity<Repository>,
    workspace: Entity<Workspace>,
    sha: String,
    _window: &mut Window,
    cx: &mut App,
) {
    let receiver = repository.update(cx, |repository, cx| {
        repository.revert_commits(vec![sha.clone()], false, cx)
    });
    report_command(
        workspace,
        "revert",
        i18n::t!("Reverted {}.", short_sha(&sha)),
        receiver,
        None,
        cx,
    );
}

fn reword_commit(
    repository: Entity<Repository>,
    workspace: Entity<Workspace>,
    sha: String,
    window: &mut Window,
    cx: &mut App,
) {
    let details = repository.update(cx, |repository, _| repository.show(sha.clone()));
    workspace.update(cx, |_workspace, cx| {
        cx.spawn_in(window, async move |workspace, cx| {
            let current_message = details
                .await
                .log_err()
                .and_then(|details| details.log_err())
                .map(|details| details.message.to_string())
                .unwrap_or_default();
            workspace
                .update_in(cx, |workspace, window, cx| {
                    let workspace_entity = cx.entity();
                    let title = i18n::t!("Reword the Message of {}", short_sha(&sha));
                    workspace.toggle_modal(window, cx, |window, cx| {
                        CommitTextModal::new(
                            CommitTextModalOptions {
                                title: title.into(),
                                name_placeholder: None,
                                message: Some((
                                    i18n::t!("New commit message").into(),
                                    current_message,
                                )),
                                confirm_label: i18n::t!("Rewrite").into(),
                            },
                            Box::new(move |input, _window, cx| {
                                let message = input.message.trim().to_string();
                                if message.is_empty() {
                                    return;
                                }
                                // Queued first, so that it reads the commit before the reword
                                // replaces it with one no upstream has.
                                let pushed = repository.update(cx, |repository, _| {
                                    repository.commit_is_pushed(sha.clone())
                                });
                                let receiver = repository.update(cx, |repository, cx| {
                                    repository.reword_commit(sha.clone(), message, cx)
                                });
                                report_command(
                                    workspace_entity,
                                    "commit-tree",
                                    i18n::t!("Reworded the message of {}.", short_sha(&sha)),
                                    receiver,
                                    Some(pushed),
                                    cx,
                                );
                            }),
                            window,
                            cx,
                        )
                    });
                })
                .ok();
        })
        .detach();
    });
}

/// Takes one commit out of the branch's history by replaying everything after it onto its parent.
/// The commits that follow it get new SHAs, so this asks first.
fn drop_commit(
    repository: Entity<Repository>,
    workspace: Entity<Workspace>,
    sha: String,
    window: &mut Window,
    cx: &mut App,
) {
    let short = short_sha(&sha);
    let prompt = window.prompt(
        PromptLevel::Warning,
        &i18n::t!("Drop {short} from the history?"),
        Some(&i18n::t!(
            "The changes of {short} are undone, and every commit after it gets a new SHA.\nIf the branch was pushed, it needs a force push afterwards."
        )),
        &[i18n::tc!("git history", "Drop"), i18n::t!("Cancel")],
        cx,
    );
    cx.spawn(async move |cx| {
        if prompt.await.ok() != Some(0) {
            return;
        }
        cx.update(move |cx| {
            let pushed =
                repository.update(cx, |repository, _| repository.commit_is_pushed(sha.clone()));
            let receiver =
                repository.update(cx, |repository, cx| repository.drop_commit(sha.clone(), cx));
            report_command(
                workspace,
                "rebase",
                i18n::t!("Dropped {} from the history.", short_sha(&sha)),
                receiver,
                Some(pushed),
                cx,
            );
        });
    })
    .detach();
}

/// Folds changes into `sha`, a commit on the current branch, after the user confirms (fixup).
///
/// `repo_paths` = `None` asks which changes to use (the staged ones or everything); `Some` uses
/// exactly those files, staged or not, which is what dropping git panel entries onto a commit
/// row of the graph does.
pub(crate) fn fixup_commit(
    repository: Entity<Repository>,
    workspace: Entity<Workspace>,
    sha: String,
    repo_paths: Option<Vec<git::repository::RepoPath>>,
    window: &mut Window,
    cx: &mut App,
) {
    let changes = {
        let repository = repository.read(cx);
        match repo_paths {
            None => {
                let counts = FixupChangeCounts::from_statuses(
                    repository.cached_status().map(|entry| entry.status),
                );
                match fixup_refusal_for(counts, repository.branch.is_some()) {
                    Some(reason) => Err(reason),
                    None => Ok(FixupModalChanges::Choice {
                        selected: if counts.staged > 0 {
                            FixupChoice::Staged
                        } else {
                            FixupChoice::AllTracked
                        },
                        counts,
                    }),
                }
            }
            Some(paths) if paths.is_empty() => Err(i18n::t!("There are no files to fold in.")),
            Some(paths) => {
                let other_staged = repository.cached_status().any(|entry| {
                    entry.status.staging().has_staged()
                        && !paths
                            .iter()
                            .any(|picked| entry.repo_path.starts_with(picked))
                });
                Ok(FixupModalChanges::Paths {
                    paths,
                    other_staged,
                })
            }
        }
    };
    let changes = match changes {
        Ok(changes) => changes,
        Err(reason) => {
            show_warning_toast(&workspace, reason, cx);
            return;
        }
    };

    let target = repository.update(cx, |repository, _| repository.fixup_target(sha));
    workspace.update(cx, |_workspace, cx| {
        cx.spawn_in(window, async move |workspace, cx| {
            let target = match await_receiver(target).await {
                Ok(target) => target,
                Err(error) => {
                    workspace
                        .update(cx, |workspace, cx| {
                            let toast = commit_status_toast(
                                i18n::t!("Cannot fixup: {}", fixup_error_message(&error)),
                                IconName::Warning,
                                Color::Warning,
                                cx,
                            );
                            workspace.toggle_status_toast(toast, cx);
                        })
                        .ok();
                    return;
                }
            };
            workspace
                .update_in(cx, |workspace, window, cx| {
                    let workspace_entity = cx.entity();
                    workspace.toggle_modal(window, cx, |_window, cx| {
                        FixupModal::new(
                            target,
                            changes,
                            Box::new(move |target, changes, _window, cx| {
                                run_fixup(repository, workspace_entity, target, changes, cx);
                            }),
                            cx,
                        )
                    });
                })
                .ok();
        })
        .detach();
    });
}

/// The fixup the commit menu and `git::FixupCommit` offer, which asks which of the changes to use.
fn fixup_choosing_changes(
    repository: Entity<Repository>,
    workspace: Entity<Workspace>,
    sha: String,
    window: &mut Window,
    cx: &mut App,
) {
    fixup_commit(repository, workspace, sha, None, window, cx);
}

/// How many files of each kind a fixup could take, read from the last status scan.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct FixupChangeCounts {
    /// Files with something staged.
    staged: usize,
    /// Tracked files with any change, staged or not: what `commit -a` takes.
    tracked: usize,
    conflicted: usize,
}

impl FixupChangeCounts {
    fn from_statuses(statuses: impl IntoIterator<Item = FileStatus>) -> Self {
        let mut counts = Self::default();
        for status in statuses {
            match status {
                FileStatus::Unmerged(_) => counts.conflicted += 1,
                FileStatus::Tracked(tracked) => {
                    let staged = tracked.index_status != StatusCode::Unmodified;
                    if staged {
                        counts.staged += 1;
                    }
                    if staged || tracked.worktree_status != StatusCode::Unmodified {
                        counts.tracked += 1;
                    }
                }
                FileStatus::Untracked | FileStatus::Ignored => {}
            }
        }
        counts
    }
}

/// Why the commit menu's fixup item is greyed out, as far as the repository already knows without
/// asking git. The rest (a commit off the current branch, a stopped rebase, …) is refused when
/// the item runs.
pub(crate) fn fixup_menu_refusal(repository: &Repository) -> Option<SharedString> {
    let counts =
        FixupChangeCounts::from_statuses(repository.cached_status().map(|entry| entry.status));
    fixup_refusal_for(counts, repository.branch.is_some()).map(SharedString::from)
}

fn fixup_refusal_for(counts: FixupChangeCounts, on_branch: bool) -> Option<&'static str> {
    if !on_branch {
        Some(i18n::t!(
            "Not available on a detached HEAD. Switch to a branch first."
        ))
    } else if counts.conflicted > 0 {
        Some(i18n::t!("Some files have conflicts. Resolve them first."))
    } else if counts.tracked == 0 {
        Some(i18n::t!(
            "There are no changes to put in the commit. Untracked files can be put in once they are staged."
        ))
    } else {
        None
    }
}

/// What the user reads when a fixup is refused before git changed anything: the job's own
/// sentence, or, from an SSH host running a Zed too old to allow a command, why it cannot work
/// there at all.
fn fixup_error_message(error: &anyhow::Error) -> String {
    let message = format!("{error:#}");
    if message.contains("unsupported git command:") {
        return i18n::t!(
            "The Zed server on the remote host is too old for this operation. Update the server."
        )
        .to_string();
    }
    message
}

/// How many dragged files the confirmation lists before it only counts the rest.
const FIXUP_LISTED_PATHS: usize = 10;

/// The first `limit` of `paths` as the confirmation lists them, and how many it leaves out.
fn listed_fixup_paths(paths: &[RepoPath], limit: usize) -> (Vec<SharedString>, usize) {
    let listed = paths
        .iter()
        .take(limit)
        .map(|path| SharedString::from(path.as_unix_str().to_string()))
        .collect::<Vec<_>>();
    let left_out = paths.len().saturating_sub(listed.len());
    (listed, left_out)
}

fn run_fixup(
    repository: Entity<Repository>,
    workspace: Entity<Workspace>,
    target: FixupTarget,
    changes: FixupChanges,
    cx: &mut App,
) {
    let receiver = repository.update(cx, |repository, cx| {
        repository.fixup_commit(target.sha.clone(), changes, cx)
    });
    cx.spawn(async move |cx| {
        let outcome = await_receiver(receiver).await;
        cx.update(|cx| {
            report_fixup(&repository, &workspace, &target, outcome, cx);
            refresh_sequencer_banner(&workspace, cx);
        });
    })
    .detach();
}

fn report_fixup(
    repository: &Entity<Repository>,
    workspace: &Entity<Workspace>,
    target: &FixupTarget,
    outcome: Result<FixupOutcome>,
    cx: &mut App,
) {
    let short = short_sha(&target.sha);
    let fixup_subject = format!("fixup! {}", target.subject);
    let undo = |old_head: String, new_head: String| -> FollowUp {
        let repository = repository.clone();
        let workspace = workspace.clone();
        let branch = target.branch.clone();
        Rc::new(move |window, cx| {
            undo_fixup(
                repository.clone(),
                workspace.clone(),
                FixupUndo {
                    branch: branch.clone(),
                    old_head: old_head.clone(),
                    new_head: new_head.clone(),
                },
                window,
                cx,
            )
        })
    };
    let git_output_follow_up = |output: String| -> Option<(SharedString, FollowUp)> {
        (!output.trim().is_empty()).then(|| {
            let workspace = workspace.downgrade();
            let open: FollowUp = Rc::new(move |window, cx| {
                workspace
                    .update(cx, |workspace, cx| {
                        open_output("fixup", workspace, &output, window, cx)
                    })
                    .ok();
            });
            (SharedString::from(i18n::t!("Show Git Output")), open)
        })
    };

    match outcome {
        Ok(FixupOutcome::Folded { old_head, new_head }) => {
            let message = i18n::t!(
                "Folded the changes into {short}. Previous tip: {} (recoverable from the reflog)",
                short_sha(&old_head)
            );
            show_rewrite_succeeded(
                workspace,
                message,
                Some(undo(old_head, new_head)),
                target.pushed,
                cx,
            );
        }
        Ok(FixupOutcome::NotCommitted { output, head_moved }) => {
            let headline = if head_moved {
                i18n::t!("Could not commit, but HEAD has moved. Check it with git reflog.")
                    .to_string()
            } else {
                i18n::t!(
                    "Could not commit (a hook, for example). The history is unchanged and the changes are still there."
                )
                .to_string()
            };
            show_fixup_problem(
                workspace,
                headline,
                git_output_follow_up(output.failure_message()),
                cx,
            );
        }
        Ok(FixupOutcome::RebaseStopped {
            message,
            old_head,
            fixup_commit,
        }) => {
            let headline = i18n::t!(
                "The rebase stopped while folding into {short} (a conflict, for example). Continue or abort it from the Git panel's banner. \
                 Even after aborting, the changes stay at the tip as the \"{fixup_subject}\" commit ({}). \
                 Skipping that commit takes the changes out of the history (recoverable from git reflog). Previous tip: {}",
                short_sha(&fixup_commit),
                short_sha(&old_head)
            );
            show_fixup_problem(workspace, headline, git_output_follow_up(message), cx);
        }
        Ok(FixupOutcome::RebaseRefused {
            message,
            rolled_back,
        }) => {
            let headline = if rolled_back {
                i18n::t!(
                    "Could not fold into {short} (the rebase could not start). The history is unchanged and the changes are left staged."
                )
            } else {
                i18n::t!(
                    "Could not fold into {short} (the rebase failed). The changes are left at the tip as the \"{fixup_subject}\" commit. Check the working tree with git status too."
                )
            };
            show_fixup_problem(workspace, headline, git_output_follow_up(message), cx);
        }
        Ok(FixupOutcome::LeftUnfolded { old_head, new_head }) => {
            let headline = i18n::t!(
                "The rebase finished, but the \"{fixup_subject}\" commit was not squashed into {short}. Previous tip: {} (recoverable from the reflog)",
                short_sha(&old_head)
            );
            show_fixup_problem(
                workspace,
                headline,
                Some((
                    SharedString::from(i18n::t!("Undo…")),
                    undo(old_head, new_head),
                )),
                cx,
            );
        }
        Ok(FixupOutcome::ChangesLeftInStash { old_head, .. }) => {
            let mut headline = i18n::t!(
                "Folded the changes into {short}. However, {}\nPrevious tip: {} (recoverable from the reflog)",
                ChangesLeftInStash,
                short_sha(&old_head)
            );
            if target.pushed {
                headline.push('\n');
                headline.push_str(i18n::tr(FORCE_PUSH_NEEDED));
            }
            show_fixup_problem(workspace, headline, None, cx);
        }
        Err(error) => show_warning_toast(
            workspace,
            i18n::t!("Could not fixup: {}", fixup_error_message(&error)),
            cx,
        ),
    }
}

/// Where a fixup left `branch` (`new_head`) and where it had been (`old_head`).
#[derive(Clone, Debug)]
struct FixupUndo {
    branch: String,
    old_head: String,
    new_head: String,
}

impl FixupUndo {
    /// Whether the output of `git rev-parse HEAD --symbolic-full-name HEAD` says HEAD is still
    /// the fixup's result on the fixup's branch. Another branch at the same commit does not
    /// count: `reset` would move that one instead.
    fn still_applies(&self, rev_parse_output: &str) -> bool {
        let mut lines = rev_parse_output.lines().map(str::trim);
        lines.next() == Some(self.new_head.as_str()) && lines.next() == Some(self.branch.as_str())
    }
}

/// Takes a fixup back: the branch returns to `old_head` with `reset --soft`, so what was folded
/// in comes back as staged changes. The graph's undo uses `--hard`, which here would throw those
/// changes away, since nothing `old_head` reaches ever had them.
fn undo_fixup(
    repository: Entity<Repository>,
    workspace: Entity<Workspace>,
    undo: FixupUndo,
    window: &mut Window,
    cx: &mut App,
) {
    let FixupUndo {
        old_head, new_head, ..
    } = undo.clone();
    let short_old = short_sha(&old_head);
    let short_new = short_sha(&new_head);
    let prompt = window.prompt(
        PromptLevel::Warning,
        i18n::t!("Go back to before the fixup?"),
        Some(&i18n::t!(
            "Leaves the current tip {short_new} and goes back to {short_old} (git reset --soft).\nThe folded changes stay as staged changes."
        )),
        &[i18n::tc!("git history", "Go Back"), i18n::t!("Cancel")],
        cx,
    );
    cx.spawn(async move |cx| {
        if prompt.await.ok() != Some(0) {
            return;
        }
        // Asked of git rather than of the last scan, which can still show the HEAD from before.
        let head = repository.update(cx, |repository, _| {
            repository.run_git_command(
                vec![
                    "rev-parse".into(),
                    "HEAD".into(),
                    "--symbolic-full-name".into(),
                    "HEAD".into(),
                ],
                None,
            )
        });
        match await_receiver(head).await {
            Ok(output) if output.succeeded() && undo.still_applies(&output.stdout) => {}
            Ok(_) => {
                cx.update(|cx| {
                    show_warning_toast(
                        &workspace,
                        i18n::t!(
                            "HEAD has moved or another branch was checked out since the fixup, so it cannot be undone from here. Undo it by hand with git reflog."
                        ),
                        cx,
                    )
                });
                return;
            }
            Err(error) => {
                cx.update(|cx| show_error_toast(workspace.clone(), "rev-parse", error, cx));
                return;
            }
        }
        cx.update(|cx| {
            let receiver = repository.update(cx, |repository, cx| {
                repository.reset_to(old_head.clone(), ResetMode::Soft, cx)
            });
            report_command(
                workspace,
                "reset",
                i18n::t!("Went back to {short_old}. The folded changes are staged."),
                receiver,
                None,
                cx,
            );
        });
    })
    .detach();
}

/// What a fixup that did not go as asked left behind, kept on screen until dismissed, with one
/// follow-up (git's own output, or undoing) a click away.
/// Says a history rewrite went through except that its `--autostash` could not put the user's
/// uncommitted changes back ([`ChangesLeftInStash`]). `old_head` is where the branch pointed
/// before, when the caller knows it.
pub(crate) fn show_rewrite_left_changes_in_stash(
    workspace: &Entity<Workspace>,
    success: &str,
    old_head: Option<&str>,
    needs_force_push: bool,
    cx: &mut App,
) {
    let mut headline = i18n::t!("{}\nHowever, {}", success, ChangesLeftInStash);
    if let Some(old_head) = old_head {
        headline.push('\n');
        headline.push_str(&i18n::t!(
            "Previous tip: {} (recoverable from the reflog)",
            short_sha(old_head)
        ));
    }
    if needs_force_push {
        headline.push('\n');
        headline.push_str(i18n::tr(FORCE_PUSH_NEEDED));
    }
    show_fixup_problem(workspace, headline, None, cx);
}

/// Whether a history rewrite came back as [`ChangesLeftInStash`].
pub(crate) fn left_changes_in_stash<T>(
    result: &Result<anyhow::Result<T>, futures::channel::oneshot::Canceled>,
) -> bool {
    matches!(result, Ok(Err(error)) if error.downcast_ref::<ChangesLeftInStash>().is_some())
}

fn show_fixup_problem(
    workspace: &Entity<Workspace>,
    headline: String,
    follow_up: Option<(SharedString, FollowUp)>,
    cx: &mut App,
) {
    workspace.update(cx, |workspace, cx| {
        let toast = StatusToast::new(headline, cx, move |this, _cx| {
            let this = this
                .icon(
                    Icon::new(IconName::Warning)
                        .size(IconSize::Small)
                        .color(Color::Warning),
                )
                .auto_dismiss(false)
                .dismiss_button(true);
            match follow_up {
                Some((label, run)) => this.action(label, move |window, cx| run(window, cx)),
                None => this,
            }
        });
        workspace.toggle_status_toast(toast, cx);
    });
}

fn show_warning_toast(
    workspace: &Entity<Workspace>,
    message: impl Into<SharedString>,
    cx: &mut App,
) {
    let message = message.into();
    workspace.update(cx, |workspace, cx| {
        let toast = commit_status_toast(message, IconName::Warning, Color::Warning, cx);
        workspace.toggle_status_toast(toast, cx);
    });
}

fn reset_to_commit(
    repository: Entity<Repository>,
    workspace: Entity<Workspace>,
    sha: String,
    mode: ResetMode,
    window: &mut Window,
    cx: &mut App,
) {
    let short = short_sha(&sha);
    let (flag, kept) = match mode {
        ResetMode::Soft => ("--soft", i18n::t!("The changes stay staged.")),
        ResetMode::Mixed => ("--mixed", i18n::t!("The changes stay in the working tree.")),
        ResetMode::Hard => ("--hard", i18n::t!("Every uncommitted change is lost.")),
    };
    let run = move |cx: &mut App| {
        let receiver = repository.update(cx, |repository, cx| {
            repository.reset_to(sha.clone(), mode, cx)
        });
        report_command(
            workspace,
            "reset",
            i18n::t!("Reset to {} ({flag}).", short_sha(&sha)),
            receiver,
            None,
            cx,
        );
    };

    if mode != ResetMode::Hard {
        run(cx);
        return;
    }
    // Only `--hard` throws work away, so it is the only one that asks first.
    let prompt = window.prompt(
        PromptLevel::Warning,
        &i18n::t!("Reset to {short}?"),
        Some(&i18n::t!("Runs git reset {flag} {short}.\n{kept}")),
        &[i18n::t!("Reset (Discard Changes)"), i18n::t!("Cancel")],
        cx,
    );
    cx.spawn(async move |cx| {
        if prompt.await.ok() != Some(0) {
            return;
        }
        cx.update(run);
    })
    .detach();
}

fn copy_tag_distance(
    repository: Entity<Repository>,
    workspace: Entity<Workspace>,
    sha: String,
    _window: &mut Window,
    cx: &mut App,
) {
    let receiver = repository.update(cx, |repository, _| repository.describe(sha));
    cx.spawn(async move |cx| {
        let described = await_receiver(receiver).await;
        cx.update(|cx| match described {
            Ok(Some(distance)) => {
                let text = format!(
                    "{}-{}-g{}",
                    distance.tag, distance.commits_since, distance.short_sha
                );
                cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                show_success_toast(&workspace, i18n::t!("Copied {text}."), cx);
            }
            Ok(None) => show_success_toast(&workspace, i18n::t!("There are no tags."), cx),
            Err(error) => show_error_toast(workspace.clone(), "describe", error, cx),
        });
    })
    .detach();
}

/// Awaits a git command started from the commit menu and says what happened: git's own stderr
/// when it refused, a short confirmation when it worked.
///
/// `pushed` is [`Repository::commit_is_pushed`] for a command that rewrites history, asked before
/// the command ran: a rewrite of pushed commits leaves the branch needing a force push, and the
/// confirmation offers one then and only then.
fn report_command(
    workspace: Entity<Workspace>,
    operation: &'static str,
    success: impl Into<SharedString>,
    receiver: oneshot::Receiver<Result<GitCommandOutput>>,
    pushed: Option<oneshot::Receiver<Result<bool>>>,
    cx: &mut App,
) {
    let success = success.into();
    cx.spawn(async move |cx| {
        let output = await_receiver(receiver).await;
        // The rewrite itself went through; only the user's uncommitted changes did not come back.
        let left_in_stash = matches!(
            &output,
            Err(error) if error.downcast_ref::<ChangesLeftInStash>().is_some()
        );
        let needs_force_push = match (&output, pushed) {
            (Ok(output), Some(pushed)) if output.succeeded() => {
                await_receiver(pushed).await.log_err().unwrap_or(false)
            }
            (Err(_), Some(pushed)) if left_in_stash => {
                await_receiver(pushed).await.log_err().unwrap_or(false)
            }
            _ => false,
        };
        cx.update(|cx| match output {
            Ok(output) if output.succeeded() => {
                if needs_force_push {
                    show_rewrite_succeeded(&workspace, success, None, true, cx);
                } else {
                    show_success_toast(&workspace, success, cx);
                }
            }
            Err(_) if left_in_stash => {
                show_rewrite_left_changes_in_stash(&workspace, &success, None, needs_force_push, cx)
            }
            Ok(output) => show_error_toast(
                workspace.clone(),
                operation,
                anyhow!("{}", output.failure_message()),
                cx,
            ),
            Err(error) => show_error_toast(workspace.clone(), operation, error, cx),
        });
        cx.update(|cx| refresh_sequencer_banner(&workspace, cx));
    })
    .detach();
}

/// The git panel's banner is the only way out of a stopped `cherry-pick` / `revert` / `rebase`,
/// and a scan that finds no file changed does not announce one, so every command that can stop the
/// sequencer asks the panel to look again once it is done.
fn refresh_sequencer_banner(workspace: &Entity<Workspace>, cx: &mut App) {
    workspace.update(cx, |workspace, cx| {
        let Some(panel) = workspace.panel::<crate::git_panel::GitPanel>(cx) else {
            return;
        };
        panel.update(cx, |panel, cx| panel.refresh_sequencer_state(cx));
    });
}

/// Flattens the two ways a git job can fail to produce a value: the channel closing because the
/// repository went away, and the command itself erroring.
async fn await_receiver<T>(receiver: oneshot::Receiver<Result<T>>) -> Result<T> {
    receiver
        .await
        .map_err(|_| anyhow!(i18n::t!("The git command was interrupted.")))?
}

fn report_success(
    workspace: &Entity<Workspace>,
    message: impl Into<SharedString>,
    cx: &mut AsyncApp,
) {
    let workspace = workspace.clone();
    let message = message.into();
    cx.update(|cx| show_success_toast(&workspace, message, cx));
}

fn show_success_toast(
    workspace: &Entity<Workspace>,
    message: impl Into<SharedString>,
    cx: &mut App,
) {
    let message = message.into();
    workspace.update(cx, |workspace, cx| {
        let toast = commit_status_toast(message, IconName::GitBranch, Color::Muted, cx);
        workspace.toggle_status_toast(toast, cx);
    });
}

/// Built separately from showing it, because an action handler already holds the workspace and
/// updating it a second time from there would panic.
fn commit_status_toast(
    message: impl Into<SharedString>,
    icon: IconName,
    color: Color,
    cx: &mut App,
) -> Entity<StatusToast> {
    StatusToast::new(message.into(), cx, move |this, _cx| {
        this.icon(Icon::new(icon).size(IconSize::Small).color(color))
            .dismiss_button(true)
    })
}

/// Something a toast offers to do next.
pub(crate) type FollowUp = Rc<dyn Fn(&mut Window, &mut App)>;

const FORCE_PUSH_LABEL: &str = i18n::msg!("Force Push (with lease)");
const NO_UPSTREAM_NOTE: &str =
    i18n::msg!("This branch has no upstream, so whether it was pushed was not checked.");
const FORCE_PUSH_NEEDED: &str = i18n::msg!("It was pushed, so it needs a force push (with lease).");

/// Says a history rewrite worked, with what the user may want next: taking it back (`undo`), and
/// a force push when the rewritten commits had been pushed (`needs_force_push`, which has to be
/// asked before the rewrite, see [`Repository::commit_is_pushed`]). A branch that was never
/// pushed has nothing to overwrite, so it is not offered one.
///
/// A status toast holds one button, so the case that needs both uses a notification instead.
pub(crate) fn show_rewrite_succeeded(
    workspace: &Entity<Workspace>,
    message: impl Into<SharedString>,
    undo: Option<FollowUp>,
    needs_force_push: bool,
    cx: &mut App,
) {
    struct RewriteSucceededNotification;

    let mut message = message.into().to_string();
    if needs_force_push {
        message.push('\n');
        message.push_str(i18n::tr(FORCE_PUSH_NEEDED));
    }
    let force_push = |window: &mut Window, cx: &mut App| {
        window.dispatch_action(git::ForcePush.boxed_clone(), cx);
    };
    workspace.update(cx, |workspace, cx| match (undo, needs_force_push) {
        (Some(undo), true) => {
            workspace.show_notification(
                NotificationId::unique::<RewriteSucceededNotification>(),
                cx,
                |cx| {
                    cx.new(|cx| {
                        MessageNotification::new(message, cx)
                            .content_icon(IconName::GitBranch, Color::Muted)
                            .show_suppress_button(false)
                            .primary_message(i18n::tr(FORCE_PUSH_LABEL))
                            .primary_on_click(move |window, cx| force_push(window, cx))
                            .secondary_message(i18n::t!("Undo…"))
                            .secondary_on_click(move |window, cx| undo(window, cx))
                    })
                },
            );
        }
        (undo, needs_force_push) => {
            let toast = StatusToast::new(message, cx, move |this, _cx| {
                let this = this
                    .icon(
                        Icon::new(IconName::GitBranch)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .dismiss_button(true);
                if needs_force_push {
                    return this
                        .auto_dismiss(false)
                        .action(i18n::tr(FORCE_PUSH_LABEL), force_push);
                }
                match undo {
                    // Long enough to read and decide: undoing moves the branch back.
                    Some(undo) => this
                        .auto_dismiss(false)
                        .action(i18n::t!("Undo…"), move |window, cx| undo(window, cx)),
                    None => this,
                }
            });
            workspace.toggle_status_toast(toast, cx);
        }
    });
}

/// What [`CommitTextModal`] collects; the fields it was not given stay empty.
pub(crate) struct CommitTextInput {
    name: String,
    message: String,
}

type CommitTextConfirm = Box<dyn FnOnce(CommitTextInput, &mut Window, &mut App)>;

struct CommitTextModalOptions {
    title: SharedString,
    /// Placeholder of the single-line field, absent when the modal only asks for a message.
    name_placeholder: Option<SharedString>,
    /// Label and initial contents of the multi-line field, absent when the modal only asks for a
    /// name.
    message: Option<(SharedString, String)>,
    confirm_label: SharedString,
}

/// The commit menu's three text prompts — a branch name, a tag name with an optional message, and
/// a rewritten commit message — share one modal so their keyboard handling cannot drift apart.
pub(crate) struct CommitTextModal {
    title: SharedString,
    confirm_label: SharedString,
    name_editor: Option<Entity<Editor>>,
    message_label: Option<SharedString>,
    message_editor: Option<Entity<Editor>>,
    confirm: Option<CommitTextConfirm>,
    /// Why the name was refused, shown under the field so the modal can stay open with what the
    /// user typed still in it.
    name_error: Option<SharedString>,
}

fn open_commit_text_modal(
    workspace: &Entity<Workspace>,
    options: CommitTextModalOptions,
    confirm: impl FnOnce(CommitTextInput, &mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    workspace.update(cx, |workspace, cx| {
        workspace.toggle_modal(window, cx, |window, cx| {
            CommitTextModal::new(options, Box::new(confirm), window, cx)
        });
    });
}

impl CommitTextModal {
    fn new(
        options: CommitTextModalOptions,
        confirm: CommitTextConfirm,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name_editor = options.name_placeholder.map(|placeholder| {
            cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_placeholder_text(&placeholder, window, cx);
                editor
            })
        });
        let (message_label, message_editor) = match options.message {
            Some((label, initial)) => {
                let editor = cx.new(|cx| {
                    let mut editor = Editor::auto_height(3, 12, window, cx);
                    editor.set_text(initial, window, cx);
                    editor
                });
                (Some(label), Some(editor))
            }
            None => (None, None),
        };
        Self {
            title: options.title,
            confirm_label: options.confirm_label,
            name_editor,
            message_label,
            message_editor,
            confirm: Some(confirm),
            name_error: None,
        }
    }

    fn cancel(&mut self, _: &Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        self.submit(window, cx);
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self
            .name_editor
            .as_ref()
            .map(|editor| editor.read(cx).text(cx).trim().to_string());
        // git takes a ref name starting with `-` for an option, so the branch or tag would either
        // fail to be created or mean something else entirely.
        self.name_error = match name.as_deref() {
            Some("") => Some(i18n::t!("Enter a name.").into()),
            Some(name) if name.starts_with('-') => {
                Some(i18n::t!("A name cannot start with -.").into())
            }
            _ => None,
        };
        if self.name_error.is_some() {
            cx.notify();
            return;
        }
        let Some(confirm) = self.confirm.take() else {
            return;
        };
        let input = CommitTextInput {
            name: name.unwrap_or_default(),
            message: self
                .message_editor
                .as_ref()
                .map(|editor| editor.read(cx).text(cx))
                .unwrap_or_default(),
        };
        confirm(input, window, cx);
        cx.emit(DismissEvent);
    }

    /// Ctrl+Enter, for the multi-line field where Enter inserts a newline (the keymap gives the
    /// same pair to the git panel's commit box).
    fn secondary_confirm(
        &mut self,
        _: &SecondaryConfirm,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.submit(window, cx);
    }

    /// Tab moves between the two fields rather than indenting: the editors are form fields here,
    /// and the multi-line one already owns Enter.
    fn focus_message(
        &mut self,
        _: &editor::actions::Tab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(message_editor) = self.message_editor.as_ref()
            && self.name_editor.is_some()
        {
            message_editor.focus_handle(cx).focus(window, cx);
            cx.stop_propagation();
        }
    }

    fn focus_name(
        &mut self,
        _: &editor::actions::Backtab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(name_editor) = self.name_editor.as_ref() {
            name_editor.focus_handle(cx).focus(window, cx);
            cx.stop_propagation();
        }
    }
}

#[cfg(test)]
impl CommitTextModal {
    /// Test hooks. The fields stay private so that the modal's own validation is the only way in.
    pub(crate) fn set_name(&self, name: &str, window: &mut Window, cx: &mut App) {
        if let Some(editor) = self.name_editor.as_ref() {
            editor.update(cx, |editor, cx| editor.set_text(name, window, cx));
        }
    }

    pub(crate) fn name_error(&self) -> Option<SharedString> {
        self.name_error.clone()
    }
}

impl EventEmitter<DismissEvent> for CommitTextModal {}
impl ModalView for CommitTextModal {}

impl Focusable for CommitTextModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.name_editor
            .as_ref()
            .or(self.message_editor.as_ref())
            .map(|editor| editor.focus_handle(cx))
            .unwrap_or_else(|| cx.focus_handle())
    }
}

impl Render for CommitTextModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("CommitTextModal")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::secondary_confirm))
            .capture_action(cx.listener(Self::focus_message))
            .capture_action(cx.listener(Self::focus_name))
            .elevation_2(cx)
            .w(rems(34.))
            .child(
                h_flex()
                    .px_3()
                    .pt_2()
                    .pb_1()
                    .w_full()
                    .gap_1p5()
                    .child(Icon::new(IconName::GitBranch).size(IconSize::XSmall))
                    .child(Headline::new(self.title.clone()).size(HeadlineSize::XSmall)),
            )
            .children(
                self.name_editor
                    .clone()
                    .map(|editor| div().px_3().pb_2().w_full().child(editor)),
            )
            .children(self.name_error.clone().map(|error| {
                div()
                    .px_3()
                    .pb_2()
                    .child(Label::new(error).size(LabelSize::Small).color(Color::Error))
            }))
            .children(self.message_label.clone().map(|label| {
                div()
                    .px_3()
                    .pb_1()
                    .child(Label::new(label).size(LabelSize::Small).color(Color::Muted))
            }))
            .children(
                self.message_editor
                    .clone()
                    .map(|editor| div().px_3().pb_2().w_full().child(editor)),
            )
            .child(
                h_flex()
                    .px_3()
                    .pb_3()
                    .w_full()
                    .gap_1()
                    .justify_end()
                    .child(
                        Button::new("commit-text-cancel", i18n::t!("Cancel")).on_click(
                            cx.listener(|_, _, _window, cx| {
                                cx.emit(DismissEvent);
                            }),
                        ),
                    )
                    .child(
                        Button::new("commit-text-confirm", self.confirm_label.clone())
                            .style(ButtonStyle::Filled)
                            // The multi-line field keeps Enter for a newline, so the key that
                            // confirms there is worth spelling out.
                            .when_some(
                                self.message_editor
                                    .as_ref()
                                    .map(|editor| editor.focus_handle(cx)),
                                |button, focus_handle| {
                                    button.tooltip(move |_window, cx| {
                                        Tooltip::for_action_in(
                                            i18n::t!("Confirm"),
                                            &Confirm,
                                            &focus_handle,
                                            cx,
                                        )
                                    })
                                },
                            )
                            .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                    ),
            )
    }
}

/// The two sets of changes the fixup confirmation offers when no files were picked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FixupChoice {
    Staged,
    AllTracked,
}

enum FixupModalChanges {
    /// The staged changes or every change to a tracked file; the first only when something is
    /// staged.
    Choice {
        selected: FixupChoice,
        counts: FixupChangeCounts,
    },
    /// Exactly these files, which is what dropping them onto a commit asks for.
    Paths {
        paths: Vec<RepoPath>,
        /// Something outside `paths` is staged too. It stays out of the commit, and the rebase's
        /// autostash hands it back unstaged, which the confirmation says.
        other_staged: bool,
    },
}

impl FixupModalChanges {
    fn changes(&self) -> FixupChanges {
        match self {
            Self::Choice {
                selected: FixupChoice::Staged,
                ..
            } => FixupChanges::Staged,
            Self::Choice {
                selected: FixupChoice::AllTracked,
                ..
            } => FixupChanges::AllTracked,
            Self::Paths { paths, .. } => FixupChanges::Paths(paths.clone()),
        }
    }
}

type FixupConfirm = Box<dyn FnOnce(FixupTarget, FixupChanges, &mut Window, &mut App)>;

/// Confirms a fixup: which commit, which changes, and what it does to the history after it.
///
/// There is no text field, so the modal keeps focus itself and Enter / Esc / ↑↓ reach it through
/// the default `menu::*` bindings without an editor in the way.
pub(crate) struct FixupModal {
    focus_handle: FocusHandle,
    target: FixupTarget,
    changes: FixupModalChanges,
    confirm: Option<FixupConfirm>,
}

impl FixupModal {
    fn new(
        target: FixupTarget,
        changes: FixupModalChanges,
        confirm: FixupConfirm,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            target,
            changes,
            confirm: Some(confirm),
        }
    }

    fn select(&mut self, choice: FixupChoice, cx: &mut Context<Self>) {
        if let FixupModalChanges::Choice { selected, counts } = &mut self.changes
            // The staged changes are only a choice when there are some.
            && (choice == FixupChoice::AllTracked || counts.staged > 0)
        {
            *selected = choice;
            cx.notify();
        }
    }

    fn select_other(&mut self, cx: &mut Context<Self>) {
        if let FixupModalChanges::Choice { selected, .. } = &self.changes {
            let other = match selected {
                FixupChoice::Staged => FixupChoice::AllTracked,
                FixupChoice::AllTracked => FixupChoice::Staged,
            };
            self.select(other, cx);
        }
    }

    fn select_next(&mut self, _: &SelectNext, _window: &mut Window, cx: &mut Context<Self>) {
        self.select_other(cx);
    }

    fn select_previous(
        &mut self,
        _: &SelectPrevious,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_other(cx);
    }

    fn cancel(&mut self, _: &Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        self.submit(window, cx);
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(confirm) = self.confirm.take() else {
            return;
        };
        confirm(self.target.clone(), self.changes.changes(), window, cx);
        cx.emit(DismissEvent);
    }

    fn render_choice(
        &self,
        choice: FixupChoice,
        label: String,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let id = match choice {
            FixupChoice::Staged => "fixup-choice-staged",
            FixupChoice::AllTracked => "fixup-choice-all",
        };
        ListItem::new(id)
            .toggle_state(selected)
            .start_slot(
                div()
                    .child(
                        Icon::new(IconName::Check)
                            .size(IconSize::Small)
                            .color(Color::Accent),
                    )
                    .when(!selected, |slot| slot.invisible()),
            )
            .child(Label::new(label).size(LabelSize::Small))
            .on_click(cx.listener(move |this, _, _window, cx| this.select(choice, cx)))
    }

    fn render_changes(&self, cx: &mut Context<Self>) -> AnyElement {
        match &self.changes {
            FixupModalChanges::Choice { selected, counts } => v_flex()
                .gap_0p5()
                .when(counts.staged > 0, |list| {
                    list.child(self.render_choice(
                        FixupChoice::Staged,
                        i18n::tn!(
                            counts.staged,
                            "Only the staged changes ({count} file)",
                            "Only the staged changes ({count} files)"
                        ),
                        *selected == FixupChoice::Staged,
                        cx,
                    ))
                })
                .child(self.render_choice(
                    FixupChoice::AllTracked,
                    i18n::tn!(
                        counts.tracked,
                        "All changes ({count} tracked file)",
                        "All changes ({count} tracked files)"
                    ),
                    *selected == FixupChoice::AllTracked,
                    cx,
                ))
                .child(
                    Label::new(i18n::t!(
                        "Untracked files are not included unless they are staged."
                    ))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
                .into_any_element(),
            FixupModalChanges::Paths {
                paths,
                other_staged,
            } => {
                let (listed, left_out) = listed_fixup_paths(paths, FIXUP_LISTED_PATHS);
                v_flex()
                    .gap_0p5()
                    .child(
                        Label::new(i18n::tn!(
                            paths.len(),
                            "The {count} chosen file (staged or not, as it is now)",
                            "The {count} chosen files (staged or not, as they are now)"
                        ))
                        .size(LabelSize::Small),
                    )
                    .children(listed.into_iter().map(|path| {
                        div().pl_2().min_w_0().child(
                            Label::new(path)
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                                .truncate(),
                        )
                    }))
                    .when(left_out > 0, |list| {
                        list.child(
                            div().pl_2().child(
                                Label::new(i18n::t!("and {left_out} more"))
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            ),
                        )
                    })
                    .when(*other_staged, |list| {
                        list.child(
                            Label::new(i18n::t!(
                                "Other staged changes are not included (after the rewrite they are left unstaged)."
                            ))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                        )
                    })
                    .into_any_element()
            }
        }
    }
}

#[cfg(test)]
impl FixupModal {
    /// Test hooks: what confirming now would fold in, and what the confirmation says about the
    /// commit.
    pub(crate) fn changes_for_test(&self) -> FixupChanges {
        self.changes.changes()
    }

    pub(crate) fn target_for_test(&self) -> &FixupTarget {
        &self.target
    }
}

impl EventEmitter<DismissEvent> for FixupModal {}
impl ModalView for FixupModal {}

impl Focusable for FixupModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for FixupModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let short = short_sha(&self.target.sha);
        let warning = |text: String| {
            h_flex()
                .gap_1()
                .items_start()
                .child(
                    Icon::new(IconName::Warning)
                        .size(IconSize::Small)
                        .color(Color::Warning),
                )
                .child(Label::new(text).size(LabelSize::Small))
        };
        v_flex()
            .key_context("GitFixupModal")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .elevation_3(cx)
            .w(rems(36.))
            .p_3()
            .gap_2()
            .child(
                h_flex()
                    .gap_1p5()
                    .child(Icon::new(IconName::GitBranch).size(IconSize::XSmall))
                    .child(
                        Headline::new(i18n::t!("Fold Current Changes into {short} (fixup)"))
                            .size(HeadlineSize::XSmall),
                    ),
            )
            .child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .child(
                        Label::new(short.clone())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        div().flex_1().min_w_0().child(
                            Label::new(self.target.subject.clone())
                                .size(LabelSize::Small)
                                .truncate(),
                        ),
                    ),
            )
            .child(
                Label::new(i18n::t!("Changes to Fold In"))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(self.render_changes(cx))
            .child(warning(i18n::t!(
                "Every commit after {short} gets a new SHA."
            )))
            .when(self.target.pushed, |modal| {
                modal.child(warning(
                    i18n::t!("It was pushed, so it needs a force push (with lease) afterwards.")
                        .to_string(),
                ))
            })
            .when(!self.target.has_upstream, |modal| {
                modal.child(
                    Label::new(i18n::tr(NO_UPSTREAM_NOTE))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
            .child(
                h_flex()
                    .w_full()
                    .justify_end()
                    .gap_1()
                    .child(
                        Button::new("fixup-cancel", i18n::t!("Cancel")).on_click(cx.listener(
                            |_, _, _window, cx| {
                                cx.emit(DismissEvent);
                            },
                        )),
                    )
                    .child(
                        Button::new("fixup-confirm", i18n::t!("Fold into Commit"))
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                    ),
            )
    }
}

pub(crate) fn git_task_context(
    repository: &Option<WeakEntity<Repository>>,
    commit_sha: git::Oid,
    ref_name: Option<&str>,
    cx: &App,
) -> Option<TaskContext> {
    let repository_path = repository
        .as_ref()?
        .upgrade()?
        .read(cx)
        .work_directory_abs_path
        .to_path_buf();
    let repository_name = repository_path
        .file_name()
        .and_then(|name| name.to_str())
        .map(ToString::to_string);
    let mut task_variables = TaskVariables::from_iter([
        (VariableName::GitSha, commit_sha.to_string()),
        (VariableName::GitShaShort, commit_sha.display_short()),
        (
            VariableName::GitRepositoryPath,
            repository_path.to_string_lossy().into_owned(),
        ),
    ]);

    if let Some(repository_name) = repository_name {
        task_variables.insert(VariableName::GitRepositoryName, repository_name);
    }
    if let Some(ref_name) = ref_name {
        task_variables.insert(VariableName::GitRef, ref_name.to_string());
    }

    Some(TaskContext {
        cwd: Some(repository_path),
        task_variables,
        ..TaskContext::default()
    })
}

pub(crate) fn git_context_menu_tasks(
    task_context: Option<TaskContext>,
    workspace: &WeakEntity<Workspace>,
    cx: &App,
) -> Vec<(project::TaskSourceKind, task::ResolvedTask)> {
    let Some(task_context) = task_context else {
        return Vec::new();
    };
    let Some(workspace) = workspace.upgrade() else {
        return Vec::new();
    };
    let project = workspace.read(cx).project().clone();
    let task_inventory = project.read_with(cx, |project, cx| {
        project.task_store().read(cx).task_inventory().cloned()
    });
    let Some(task_inventory) = task_inventory else {
        return Vec::new();
    };

    task_inventory
        .read(cx)
        .resolve_global_tasks_with_tag(GIT_COMMAND_TASK_TAG, &task_context)
}

#[cfg(test)]
mod tests {
    use super::*;
    use git::repository::repo_path;
    use git::status::{TrackedStatus, UnmergedStatus, UnmergedStatusCode};

    fn tracked(index_status: StatusCode, worktree_status: StatusCode) -> FileStatus {
        FileStatus::Tracked(TrackedStatus {
            index_status,
            worktree_status,
        })
    }

    /// Undoing a fixup moves the branch it rewrote, only while HEAD is still its result there:
    /// another branch at the same commit is left alone.
    #[test]
    fn test_fixup_undo_applies_only_on_the_rewritten_branch() {
        let undo = FixupUndo {
            branch: "refs/heads/dev".to_string(),
            old_head: "a".repeat(40),
            new_head: "b".repeat(40),
        };
        let new_head = "b".repeat(40);
        assert!(undo.still_applies(&format!("{new_head}\nrefs/heads/dev\n")));
        assert!(undo.still_applies(&format!("{new_head}\r\nrefs/heads/dev\r\n")));
        assert!(
            !undo.still_applies(&format!("{new_head}\nrefs/heads/other\n")),
            "another branch at the same commit"
        );
        assert!(
            !undo.still_applies(&format!("{new_head}\nHEAD\n")),
            "a detached HEAD at the same commit"
        );
        assert!(!undo.still_applies(&format!("{}\nrefs/heads/dev\n", "c".repeat(40))));
        assert!(!undo.still_applies(&format!("{new_head}\n")));
    }

    /// `commit -a` takes every tracked change, staged or not; untracked and ignored files are
    /// never counted, since nothing short of staging them puts them in a fixup.
    #[test]
    fn test_fixup_change_counts() {
        let counts = FixupChangeCounts::from_statuses([
            tracked(StatusCode::Modified, StatusCode::Unmodified),
            tracked(StatusCode::Modified, StatusCode::Modified),
            tracked(StatusCode::Unmodified, StatusCode::Modified),
            tracked(StatusCode::Unmodified, StatusCode::Deleted),
            tracked(StatusCode::Added, StatusCode::Unmodified),
            FileStatus::Untracked,
            FileStatus::Ignored,
        ]);
        assert_eq!(
            counts,
            FixupChangeCounts {
                staged: 3,
                tracked: 5,
                conflicted: 0,
            }
        );

        let conflicted = FixupChangeCounts::from_statuses([FileStatus::Unmerged(UnmergedStatus {
            first_head: UnmergedStatusCode::Updated,
            second_head: UnmergedStatusCode::Updated,
        })]);
        assert_eq!(conflicted.conflicted, 1);
        assert_eq!(conflicted.tracked, 0);
    }

    #[test]
    fn test_fixup_refusal_for() {
        let changed = FixupChangeCounts {
            staged: 0,
            tracked: 1,
            conflicted: 0,
        };
        assert_eq!(fixup_refusal_for(changed, true), None);
        assert!(
            fixup_refusal_for(changed, false)
                .is_some_and(|reason| reason.contains("detached HEAD"))
        );
        assert!(
            fixup_refusal_for(
                FixupChangeCounts {
                    conflicted: 1,
                    ..changed
                },
                true
            )
            .is_some_and(|reason| reason.contains("conflicts"))
        );
        // Untracked files alone are not something the menu's fixup can take.
        assert!(
            fixup_refusal_for(FixupChangeCounts::default(), true)
                .is_some_and(|reason| reason.contains("no changes"))
        );
    }

    #[test]
    fn test_listed_fixup_paths_count_what_they_leave_out() {
        let paths = (0..12)
            .map(|index| repo_path(&format!("src/{index}.rs")))
            .collect::<Vec<_>>();
        let (listed, left_out) = listed_fixup_paths(&paths, FIXUP_LISTED_PATHS);
        assert_eq!(listed.len(), 10);
        assert_eq!(listed.first().map(|path| path.as_ref()), Some("src/0.rs"));
        assert_eq!(left_out, 2);

        let (listed, left_out) = listed_fixup_paths(&paths[..3], FIXUP_LISTED_PATHS);
        assert_eq!(listed.len(), 3);
        assert_eq!(left_out, 0);
    }

    /// An older Zed on the far side of SSH refuses a subcommand it was never taught (`add`) in
    /// English; the user is told why instead.
    #[test]
    fn test_fixup_error_message_explains_an_old_server() {
        let old_server = anyhow!("unsupported git command: add");
        assert!(fixup_error_message(&old_server).contains("too old"));

        let refusal = anyhow!("detached HEAD では使えません。");
        assert_eq!(
            fixup_error_message(&refusal),
            "detached HEAD では使えません。"
        );
    }

    #[test]
    fn test_fixup_modal_changes_follow_the_choice() {
        let counts = FixupChangeCounts {
            staged: 1,
            tracked: 2,
            conflicted: 0,
        };
        let choice = |selected| FixupModalChanges::Choice { selected, counts };
        assert_eq!(choice(FixupChoice::Staged).changes(), FixupChanges::Staged);
        assert_eq!(
            choice(FixupChoice::AllTracked).changes(),
            FixupChanges::AllTracked
        );
        let paths = vec![repo_path("a.rs")];
        assert_eq!(
            FixupModalChanges::Paths {
                paths: paths.clone(),
                other_staged: true,
            }
            .changes(),
            FixupChanges::Paths(paths)
        );
    }
}
