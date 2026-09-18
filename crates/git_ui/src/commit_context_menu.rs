use crate::commit_view::CommitView;
use anyhow::{Result, anyhow};
use editor::Editor;
use futures::channel::oneshot;
use git::Oid;
use git::repository::{GitCommandOutput, ResetMode};
use git_ui_core::notifications::show_error_toast;
use gpui::{
    Action, AsyncApp, ClipboardItem, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    PromptLevel, SharedString, WeakEntity, Window, actions,
};
use menu::{Cancel, Confirm, SecondaryConfirm};
use notifications::status_toast::StatusToast;
use project::{GIT_COMMAND_TASK_TAG, git_store::Repository};

use task::{TaskContext, TaskVariables, VariableName};
use ui::{
    Color, ContextMenu, ContextMenuEntry, Headline, HeadlineSize, IconName, IconPosition, IconSize,
    Tooltip, prelude::*,
};
use util::ResultExt as _;
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
const CUSTOM_GIT_COMMANDS_DOCS_SLUG: &str = "tasks#custom-git-commands";

pub(crate) struct CommitContextMenuData {
    pub(crate) sha: Oid,
    pub(crate) tag_names: Vec<SharedString>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommitContextMenuSource {
    GitGraph,
    GitPanel,
}

pub(crate) fn commit_context_menu(
    commit: CommitContextMenuData,
    source: CommitContextMenuSource,
    ref_name: Option<SharedString>,
    focus_handle: FocusHandle,
    repository: Option<WeakEntity<Repository>>,
    workspace: WeakEntity<Workspace>,
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
        Some(ref_name) => format!("ref {ref_name}"),
        None => format!("コミット {sha_short}"),
    };

    ContextMenu::build(window, cx, move |context_menu, _, _| {
        context_menu
            .context(focus_handle)
            .header(header)
            .entry("差分を表示", Some(OpenCommitView.boxed_clone()), {
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
                "SHA をコピー",
                Some(CopyCommitSha.boxed_clone()),
                move |_window, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(sha.to_string()));
                },
            )
            .when_some(ref_name.clone(), |menu, ref_name| {
                menu.entry("ref 名をコピー", None, move |_window, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(ref_name.to_string()));
                })
            })
            .when(ref_name.is_none(), |menu| {
                menu.map(|menu| {
                    let tag_names = commit.tag_names.clone();
                    let copy_tag_label = "タグをコピー";

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
                menu.entry("コミットグラフで表示", None, move |window, cx| {
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
                menu.map(move |menu| commit_rewrite_entries(menu, sha, repository, workspace))
            })
            .map(|mut menu| {
                menu = menu.separator().header("カスタムコマンド");

                if git_tasks.is_empty() {
                    return menu.item(
                        ContextMenuEntry::new("詳細")
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

/// The entries that change the repository rather than only read the commit. They need a
/// repository, so a menu opened where none could be resolved keeps just the copy items.
fn commit_rewrite_entries(
    menu: ContextMenu,
    sha: Oid,
    repository: WeakEntity<Repository>,
    workspace: WeakEntity<Workspace>,
) -> ContextMenu {
    let sha = sha.to_string();
    menu.separator()
        .entry(
            "ここからブランチを作成…",
            Some(git::CreateBranchFrom { sha: sha.clone() }.boxed_clone()),
            commit_action_handler(&repository, &workspace, &sha, create_branch_from_commit),
        )
        .entry(
            "このコミットをチェックアウト (detached)",
            Some(git::CheckoutCommit { sha: sha.clone() }.boxed_clone()),
            commit_action_handler(&repository, &workspace, &sha, checkout_commit),
        )
        .entry(
            "ここにタグを付ける…",
            Some(git::CreateTagAt { sha: sha.clone() }.boxed_clone()),
            commit_action_handler(&repository, &workspace, &sha, create_tag_at_commit),
        )
        .separator()
        .entry(
            "このコミットを現在のブランチに取り込む (cherry-pick)",
            Some(git::CherryPickCommit { sha: sha.clone() }.boxed_clone()),
            commit_action_handler(&repository, &workspace, &sha, cherry_pick_commit),
        )
        .entry(
            "このコミットを打ち消す (revert)",
            Some(git::RevertCommit { sha: sha.clone() }.boxed_clone()),
            commit_action_handler(&repository, &workspace, &sha, revert_commit),
        )
        .entry(
            "メッセージを書き換える…",
            Some(git::RewordCommit { sha: sha.clone() }.boxed_clone()),
            commit_action_handler(&repository, &workspace, &sha, reword_commit),
        )
        .entry(
            "このコミットを捨てる…",
            Some(git::DropCommit { sha: sha.clone() }.boxed_clone()),
            commit_action_handler(&repository, &workspace, &sha, drop_commit),
        )
        .separator()
        .entry(
            "このコミットまで戻す (soft: 変更をステージに残す)",
            Some(
                git::ResetToCommit {
                    sha: sha.clone(),
                    mode: git::ResetToCommitMode::Soft,
                }
                .boxed_clone(),
            ),
            reset_handler(&repository, &workspace, &sha, ResetMode::Soft),
        )
        .entry(
            "このコミットまで戻す (mixed: 変更を作業ツリーに残す)",
            Some(
                git::ResetToCommit {
                    sha: sha.clone(),
                    mode: git::ResetToCommitMode::Mixed,
                }
                .boxed_clone(),
            ),
            reset_handler(&repository, &workspace, &sha, ResetMode::Mixed),
        )
        .entry(
            "このコミットまで戻す (hard: 変更を捨てる)…",
            Some(
                git::ResetToCommit {
                    sha: sha.clone(),
                    mode: git::ResetToCommitMode::Hard,
                }
                .boxed_clone(),
            ),
            reset_handler(&repository, &workspace, &sha, ResetMode::Hard),
        )
        .separator()
        .entry(
            "直近のタグからの距離をコピー",
            Some(git::CopyTagDistance { sha: sha.clone() }.boxed_clone()),
            commit_action_handler(&repository, &workspace, &sha, copy_tag_distance),
        )
}

fn commit_action_handler(
    repository: &WeakEntity<Repository>,
    workspace: &WeakEntity<Workspace>,
    sha: &str,
    operation: CommitOperation,
) -> impl Fn(&mut Window, &mut App) + 'static {
    let repository = repository.clone();
    let workspace = workspace.clone();
    let sha = sha.to_string();
    move |window, cx| {
        let (Some(repository), Some(workspace)) = (repository.upgrade(), workspace.upgrade())
        else {
            return;
        };
        operation(repository, workspace, sha.clone(), window, cx);
    }
}

fn reset_handler(
    repository: &WeakEntity<Repository>,
    workspace: &WeakEntity<Workspace>,
    sha: &str,
    mode: ResetMode,
) -> impl Fn(&mut Window, &mut App) + 'static {
    let repository = repository.clone();
    let workspace = workspace.clone();
    let sha = sha.to_string();
    move |window, cx| {
        let (Some(repository), Some(workspace)) = (repository.upgrade(), workspace.upgrade())
        else {
            return;
        };
        reset_to_commit(repository, workspace, sha.clone(), mode, window, cx);
    }
}

/// Makes the commit menu's items reachable from the keymap and the command palette as well,
/// against the project's active repository.
pub(crate) fn register_commit_actions(workspace: &mut Workspace) {
    fn run(
        workspace: &mut Workspace,
        sha: &str,
        operation: CommitOperation,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let Some(repository) = workspace.project().read(cx).active_repository(cx) else {
            return;
        };
        let Some(sha) = commit_for_action(workspace, sha, &repository, cx) else {
            let toast = commit_status_toast(
                "コミットを選んでください (履歴タブかコミットグラフで右クリック)",
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
            operation(repository, workspace, sha, window, cx);
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
            create_branch_from_commit,
            window,
            cx,
        );
    });
    workspace.register_action(|workspace, action: &git::CheckoutCommit, window, cx| {
        run(workspace, &action.sha, checkout_commit, window, cx);
    });
    workspace.register_action(|workspace, action: &git::CreateTagAt, window, cx| {
        run(workspace, &action.sha, create_tag_at_commit, window, cx);
    });
    workspace.register_action(|workspace, action: &git::CherryPickCommit, window, cx| {
        run(workspace, &action.sha, cherry_pick_commit, window, cx);
    });
    workspace.register_action(|workspace, action: &git::RevertCommit, window, cx| {
        run(workspace, &action.sha, revert_commit, window, cx);
    });
    workspace.register_action(|workspace, action: &git::RewordCommit, window, cx| {
        run(workspace, &action.sha, reword_commit, window, cx);
    });
    workspace.register_action(|workspace, action: &git::DropCommit, window, cx| {
        run(workspace, &action.sha, drop_commit, window, cx);
    });
    workspace.register_action(|workspace, action: &git::CopyTagDistance, window, cx| {
        run(workspace, &action.sha, copy_tag_distance, window, cx);
    });
    workspace.register_action(|workspace, action: &git::ResetToCommit, window, cx| {
        let operation: CommitOperation = match action.mode {
            git::ResetToCommitMode::Soft => |repository, workspace, sha, window, cx| {
                reset_to_commit(repository, workspace, sha, ResetMode::Soft, window, cx)
            },
            git::ResetToCommitMode::Mixed => |repository, workspace, sha, window, cx| {
                reset_to_commit(repository, workspace, sha, ResetMode::Mixed, window, cx)
            },
            git::ResetToCommitMode::Hard => |repository, workspace, sha, window, cx| {
                reset_to_commit(repository, workspace, sha, ResetMode::Hard, window, cx)
            },
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
    let title = format!("{} からブランチを作成", short_sha(&sha));
    let host = workspace.clone();
    open_commit_text_modal(
        &host,
        CommitTextModalOptions {
            title: title.into(),
            name_placeholder: Some("新しいブランチ名".into()),
            message: None,
            confirm_label: "作成…".into(),
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
        &format!("ブランチ「{name}」を作成しますか?"),
        Some(&format!("基点にするコミット: {}", short_sha(&sha))),
        &["作成して切り替える", "作成のみ", "キャンセル"],
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
            report_success(
                &workspace,
                format!("ブランチ「{name}」を作成しました。"),
                cx,
            );
            return;
        }
        let switched =
            repository.update(cx, |repository, _| repository.change_branch(name.clone()));
        match await_receiver(switched).await {
            Ok(()) => report_success(
                &workspace,
                format!("ブランチ「{name}」を作成して切り替えました。"),
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
        format!(
            "{} をチェックアウトしました (detached HEAD)。",
            short_sha(&sha)
        ),
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
    let title = format!("{} にタグを付ける", short_sha(&sha));
    let host = workspace.clone();
    open_commit_text_modal(
        &host,
        CommitTextModalOptions {
            title: title.into(),
            name_placeholder: Some("タグ名 (例: v1.2.3)".into()),
            message: Some(("メッセージ (空なら軽量タグ)".into(), String::new())),
            confirm_label: "タグを付ける".into(),
        },
        move |input, _window, cx| {
            let name = input.name.trim().to_string();
            if name.is_empty() {
                return;
            }
            let message = input.message.trim().to_string();
            let message = (!message.is_empty()).then_some(message);
            let receiver = repository.update(cx, |repository, cx| {
                repository.create_tag(name.clone(), sha.clone(), message, cx)
            });
            report_command(
                workspace,
                "tag",
                format!("タグ「{name}」を付けました。"),
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
        format!("{} を取り込みました。", short_sha(&sha)),
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
        format!("{} を打ち消しました。", short_sha(&sha)),
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
                    let title = format!("{} のメッセージを書き換える", short_sha(&sha));
                    workspace.toggle_modal(window, cx, |window, cx| {
                        CommitTextModal::new(
                            CommitTextModalOptions {
                                title: title.into(),
                                name_placeholder: None,
                                message: Some(("新しいコミットメッセージ".into(), current_message)),
                                confirm_label: "書き換える".into(),
                            },
                            Box::new(move |input, _window, cx| {
                                let message = input.message.trim().to_string();
                                if message.is_empty() {
                                    return;
                                }
                                let receiver = repository.update(cx, |repository, cx| {
                                    repository.reword_commit(sha.clone(), message, cx)
                                });
                                report_command(
                                    workspace_entity,
                                    "commit-tree",
                                    format!("{} のメッセージを書き換えました。", short_sha(&sha)),
                                    receiver,
                                    Some(ForcePushHint),
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
        &format!("{short} を履歴から取り除きますか?"),
        Some(&format!(
            "{short} の変更が無かったことになり、その後のコミットの SHA はすべて変わります。\nプッシュ済みのブランチなら、この後に強制プッシュが必要です。"
        )),
        &["取り除く", "キャンセル"],
        cx,
    );
    cx.spawn(async move |cx| {
        if prompt.await.ok() != Some(0) {
            return;
        }
        cx.update(move |cx| {
            let receiver =
                repository.update(cx, |repository, cx| repository.drop_commit(sha.clone(), cx));
            report_command(
                workspace,
                "rebase",
                format!("{} を履歴から取り除きました。", short_sha(&sha)),
                receiver,
                Some(ForcePushHint),
                cx,
            );
        });
    })
    .detach();
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
        ResetMode::Soft => ("--soft", "変更はステージに残ります。"),
        ResetMode::Mixed => ("--mixed", "変更は作業ツリーに残ります。"),
        ResetMode::Hard => ("--hard", "コミットしていない変更はすべて失われます。"),
    };
    let run = move |cx: &mut App| {
        let receiver = repository.update(cx, |repository, cx| {
            repository.reset_to(sha.clone(), mode, cx)
        });
        report_command(
            workspace,
            "reset",
            format!("{} まで戻しました ({flag})。", short_sha(&sha)),
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
        &format!("{short} まで戻しますか?"),
        Some(&format!("git reset {flag} {short} を実行します。\n{kept}")),
        &["戻す (変更を捨てる)", "キャンセル"],
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
                show_success_toast(&workspace, format!("{text} をコピーしました。"), cx);
            }
            Ok(None) => show_success_toast(&workspace, "タグがありません。", cx),
            Err(error) => show_error_toast(workspace.clone(), "describe", error, cx),
        });
    })
    .detach();
}

/// Marks an operation after which an already-pushed branch needs a force push.
#[derive(Clone, Copy)]
struct ForcePushHint;

/// Awaits a git command started from the commit menu and says what happened: git's own stderr
/// when it refused, a short confirmation when it worked.
fn report_command(
    workspace: Entity<Workspace>,
    operation: &'static str,
    success: impl Into<SharedString>,
    receiver: oneshot::Receiver<Result<GitCommandOutput>>,
    hint: Option<ForcePushHint>,
    cx: &mut App,
) {
    let success = success.into();
    cx.spawn(async move |cx| {
        let output = await_receiver(receiver).await;
        cx.update(|cx| match output {
            Ok(output) if output.succeeded() => {
                show_success_toast(&workspace, success, cx);
                if hint.is_some() {
                    show_force_push_hint(&workspace, cx);
                }
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
        .map_err(|_| anyhow!("git の実行が中断されました。"))?
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

fn show_force_push_hint(workspace: &Entity<Workspace>, cx: &mut App) {
    workspace.update(cx, |workspace, cx| {
        let toast = StatusToast::new(
            "コミットの SHA が変わりました。このブランチをプッシュ済みなら強制プッシュが必要です。",
            cx,
            |this, _cx| {
                this.icon(
                    Icon::new(IconName::Warning)
                        .size(IconSize::Small)
                        .color(Color::Warning),
                )
                .action("強制プッシュ (lease)", |window, cx| {
                    window.dispatch_action(git::ForcePush.boxed_clone(), cx);
                })
                .dismiss_button(true)
            },
        );
        workspace.toggle_status_toast(toast, cx);
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
            Some("") => Some("名前を入力してください。".into()),
            Some(name) if name.starts_with('-') => Some("先頭に - は使えません。".into()),
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
                        Button::new("commit-text-cancel", "キャンセル").on_click(cx.listener(
                            |_, _, _window, cx| {
                                cx.emit(DismissEvent);
                            },
                        )),
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
                                        Tooltip::for_action_in("確定", &Confirm, &focus_handle, cx)
                                    })
                                },
                            )
                            .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                    ),
            )
    }
}

fn git_task_context(
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

fn git_context_menu_tasks(
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
