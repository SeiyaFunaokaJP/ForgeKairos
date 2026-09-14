//! ADR-F50: what a project's folders hold back after an agent wrote into
//! them, and the user's resume.
//!
//! The notification only leads to the dialog: resuming without seeing what is
//! paused, and without a way to what the agent changed, is what the pause
//! exists to prevent. Nothing an agent can reach resumes a folder.

use std::collections::HashMap;

use gpui::{
    Action as _, App, AppContext as _, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    Subscription, Task, actions,
};
use project::{
    Project, WorktreeCodeExecution, WorktreeId,
    project_launch_approvals::escape_for_display,
    worktree_code_execution::{
        PausedLanguageServer, PausedWorktreeDetails, RemoteHostPause, ResumeOutcome,
        WorktreeCodeExecutionEvent,
    },
};
use ui::prelude::*;
use util::ResultExt as _;
use workspace::{
    ModalView, Workspace,
    notifications::{NotificationId, simple_message_notification::MessageNotification},
};

actions!(
    lsp_tool,
    [
        /// Shows what the project's folders do not run after an agent wrote
        /// into them, with a way to review the changes and resume.
        ReviewPausedWorktreeCode
    ]
);

/// Marks the notification shown for each paused folder.
struct PausedWorktreeCodeNotification;

/// A remote server from before ADR-F50 keeps running the folder's code.
const REMOTE_SERVER_DOES_NOT_FOLLOW: &str =
    "接続先のサーバーが古いため、接続先の言語サーバーとフォーマッターは止められていません";

fn remote_server_does_not_follow(details: Option<&PausedWorktreeDetails>) -> bool {
    details.and_then(|details| details.remote_host) == Some(RemoteHostPause::NotFollowing)
}

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, cx| {
        workspace.register_action(|workspace, _: &ReviewPausedWorktreeCode, window, cx| {
            open_dialog(workspace, window, cx);
        });
        let execution = workspace
            .project()
            .read(cx)
            .worktree_code_execution()
            .clone();
        cx.subscribe(
            &execution,
            |workspace, _, event: &WorktreeCodeExecutionEvent, cx| match event {
                WorktreeCodeExecutionEvent::Paused(worktree_id) => {
                    show_notification(workspace, *worktree_id, cx);
                }
                WorktreeCodeExecutionEvent::Resumed(worktree_id) => {
                    workspace.dismiss_notification(&notification_id(*worktree_id), cx);
                }
                WorktreeCodeExecutionEvent::CheckGuardFiles(_)
                | WorktreeCodeExecutionEvent::LanguageServersAllowedWhilePausedChanged
                | WorktreeCodeExecutionEvent::DetailsChanged(_) => {}
            },
        )
        .detach();
        // A folder left paused when Zed last quit stays paused.
        for worktree_id in execution.read(cx).paused_worktrees() {
            show_notification(workspace, worktree_id, cx);
        }
    })
    .detach();
}

fn notification_id(worktree_id: WorktreeId) -> NotificationId {
    NotificationId::composite::<PausedWorktreeCodeNotification>(SharedString::from(
        worktree_id.to_proto().to_string(),
    ))
}

/// The folder's name as it can be shown: it comes from the project.
fn folder_name(project: &Project, worktree_id: WorktreeId, cx: &App) -> Option<String> {
    let worktree = project.worktree_for_id(worktree_id, cx)?;
    let abs_path = worktree.read(cx).abs_path();
    let name = abs_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| abs_path.to_string_lossy().into_owned());
    Some(escape_for_display(&name).into_owned())
}

fn show_notification(
    workspace: &mut Workspace,
    worktree_id: WorktreeId,
    cx: &mut Context<Workspace>,
) {
    let Some(name) = folder_name(workspace.project().read(cx), worktree_id, cx) else {
        return;
    };
    let execution = workspace
        .project()
        .read(cx)
        .worktree_code_execution()
        .clone();
    let weak_workspace = cx.weak_entity();
    workspace.show_notification(notification_id(worktree_id), cx, move |cx| {
        cx.new(|cx| {
            // A remote server answers after the pause, and may not follow it.
            cx.subscribe(&execution, |_, _, _: &WorktreeCodeExecutionEvent, cx| {
                cx.notify()
            })
            .detach();
            MessageNotification::new_from_builder(cx, move |_window, cx| {
                let does_not_follow =
                    remote_server_does_not_follow(execution.read(cx).details(worktree_id).as_ref());
                v_flex()
                    .gap_0p5()
                    .child(Label::new(format!("エージェントが「{name}」に書き込みました")))
                    .child(
                        Label::new(if does_not_follow {
                            REMOTE_SERVER_DOES_NOT_FOLLOW
                        } else {
                            "確認して再開するまで、このフォルダーのコード (言語サーバー・フォーマッター) を実行しません"
                        })
                        .size(LabelSize::Small)
                        .color(if does_not_follow {
                            Color::Warning
                        } else {
                            Color::Muted
                        }),
                    )
                    .into_any_element()
            })
            .primary_message("確認する")
            .primary_on_click(move |window, cx| {
                weak_workspace
                    .update(cx, |workspace, cx| open_dialog(workspace, window, cx))
                    .log_err();
            })
            .show_suppress_button(false)
        })
    });
}

fn open_dialog(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let project = workspace.project().clone();
    workspace.toggle_modal(window, cx, |window, cx| {
        PausedWorktreeCodeDialog::new(project, window, cx)
    });
}

/// What the dialog showed of each paused folder: a resume confirms only that.
fn current_generations(execution: &WorktreeCodeExecution) -> HashMap<WorktreeId, u64> {
    execution
        .paused_worktrees()
        .into_iter()
        .filter_map(|worktree_id| Some((worktree_id, execution.generation(worktree_id)?)))
        .collect()
}

struct PausedWorktreeCodeDialog {
    project: Entity<Project>,
    focus_handle: FocusHandle,
    shown_generations: HashMap<WorktreeId, u64>,
    /// An agent wrote again after the dialog showed the folders.
    written_since_shown: bool,
    resuming: Option<Task<()>>,
    _execution_subscription: Subscription,
}

impl PausedWorktreeCodeDialog {
    fn new(project: Entity<Project>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        focus_handle.focus(window, cx);
        let execution = project.read(cx).worktree_code_execution().clone();
        let execution_subscription = cx
            .subscribe(&execution, |_, _, _: &WorktreeCodeExecutionEvent, cx| {
                cx.notify()
            });
        let shown_generations = current_generations(execution.read(cx));
        Self {
            project,
            focus_handle,
            shown_generations,
            written_since_shown: false,
            resuming: None,
            _execution_subscription: execution_subscription,
        }
    }

    fn resume(&mut self, cx: &mut Context<Self>) {
        if self.resuming.is_some() {
            return;
        }
        let execution = self.project.read(cx).worktree_code_execution().clone();
        let current = current_generations(execution.read(cx));
        if current != self.shown_generations {
            self.shown_generations = current;
            self.written_since_shown = true;
            cx.notify();
            return;
        }
        let resumes = execution.update(cx, |execution, cx| {
            current
                .iter()
                .map(|(worktree_id, generation)| execution.resume(*worktree_id, *generation, cx))
                .collect::<Vec<_>>()
        });
        self.resuming = Some(cx.spawn(async move |this, cx| {
            let mut outcomes = Vec::new();
            for resume in resumes {
                outcomes.push(resume.await);
            }
            this.update(cx, |this, cx| {
                this.resuming = None;
                if outcomes.contains(&ResumeOutcome::WrittenSinceShown) {
                    let execution = this.project.read(cx).worktree_code_execution().clone();
                    this.shown_generations = current_generations(execution.read(cx));
                    this.written_since_shown = true;
                } else if !outcomes.contains(&ResumeOutcome::WriterRunning) {
                    cx.emit(DismissEvent);
                }
                cx.notify();
            })
            .log_err();
        }));
        cx.notify();
    }
}

impl EventEmitter<DismissEvent> for PausedWorktreeCodeDialog {}

impl Focusable for PausedWorktreeCodeDialog {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ModalView for PausedWorktreeCodeDialog {}

fn render_notice(icon: IconName, color: Color, message: &str) -> impl IntoElement {
    h_flex()
        .w_full()
        .min_w_0()
        .gap_1p5()
        .items_start()
        .child(Icon::new(icon).size(IconSize::XSmall).color(color))
        .child(
            div().min_w_0().flex_1().child(
                Label::new(message.to_string())
                    .size(LabelSize::Small)
                    .color(color),
            ),
        )
}

fn detail_line(text: String) -> impl IntoElement {
    Label::new(text).size(LabelSize::Small).color(Color::Muted)
}

fn language_server_names(details: &PausedWorktreeDetails, how: PausedLanguageServer) -> String {
    details
        .language_servers
        .iter()
        .filter(|(_, paused)| **paused == how)
        .map(|(name, _)| escape_for_display(name.as_ref()).into_owned())
        .collect::<Vec<_>>()
        .join(", ")
}

fn render_folder(name: String, details: PausedWorktreeDetails) -> impl IntoElement {
    let restricted = language_server_names(&details, PausedLanguageServer::Restricted);
    let stopped = language_server_names(&details, PausedLanguageServer::Stopped);
    let formatters = details
        .skipped_formatters
        .iter()
        .map(|formatter| escape_for_display(formatter).into_owned())
        .collect::<Vec<_>>()
        .join(", ");
    let nothing_held_back = restricted.is_empty()
        && stopped.is_empty()
        && formatters.is_empty()
        && details.remote_host.is_none();
    v_flex()
        .w_full()
        .min_w_0()
        .gap_0p5()
        .child(Label::new(name))
        .when(!restricted.is_empty(), |this| {
            this.child(detail_line(format!(
                "制限モードで動いている言語サーバー: {restricted}"
            )))
        })
        .when(!stopped.is_empty(), |this| {
            this.child(detail_line(format!("止めている言語サーバー: {stopped}")))
        })
        .when(!formatters.is_empty(), |this| {
            this.child(detail_line(format!(
                "実行しなかったフォーマッター: {formatters}"
            )))
        })
        .when(
            details.remote_host == Some(RemoteHostPause::Following),
            |this| {
                this.child(detail_line(
                    "接続先の言語サーバーとフォーマッターも同じように止めています。".to_string(),
                ))
            },
        )
        .when(details.remote_host == Some(RemoteHostPause::Unknown), |this| {
            this.child(detail_line(
                "接続先のサーバーの応答を待っています。".to_string(),
            ))
        })
        .when(remote_server_does_not_follow(Some(&details)), |this| {
            this.child(render_notice(
                IconName::Warning,
                Color::Warning,
                REMOTE_SERVER_DOES_NOT_FOLLOW,
            ))
        })
        .when(nothing_held_back, |this| {
            this.child(detail_line(
                "まだ止めた言語サーバー・フォーマッターはありません。起動や実行のときにこの状態に従います。"
                    .to_string(),
            ))
        })
        .when(details.agent_turn_running, |this| {
            this.child(render_notice(
                IconName::Warning,
                Color::Warning,
                "エージェントの応答が終わるまで再開できません。終わらないときは、エージェントを止めてください。",
            ))
        })
        .when(
            details.sandboxed_writer_running && !details.agent_turn_running,
            |this| {
                this.child(render_notice(
                    IconName::Warning,
                    Color::Warning,
                    "実行中のセッション・コマンドが終わるまで再開できません。",
                ))
            },
        )
}

impl Render for PausedWorktreeCodeDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let project = self.project.read(cx);
        let execution = project.worktree_code_execution().read(cx);
        let mut writer_running = false;
        let folders = execution
            .paused_worktrees()
            .into_iter()
            .filter_map(|worktree_id| {
                let details = execution.details(worktree_id)?;
                writer_running |= details.sandboxed_writer_running;
                Some(render_folder(
                    folder_name(project, worktree_id, cx)?,
                    details,
                ))
            })
            .collect::<Vec<_>>();
        let anything_paused = !folders.is_empty();
        let resumable = !writer_running && self.resuming.is_none();

        v_flex()
            .key_context("PausedWorktreeCodeDialog")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|_, _: &menu::Cancel, _window, cx| cx.emit(DismissEvent)))
            .elevation_3(cx)
            .w(rems(34.))
            .p_4()
            .gap_3()
            .child(Headline::new("フォルダーのコードを止めています").size(HeadlineSize::Small))
            .child(
                Label::new(if anything_paused {
                    "エージェントが書き込んだあと、次のフォルダーでは、そのフォルダーのコードを実行する言語サーバーとフォーマッターを止めています。エージェントの変更を確認してから再開してください。"
                } else {
                    "止めているフォルダーはありません。"
                })
                .size(LabelSize::Small)
                .color(Color::Muted),
            )
            .child(
                div()
                    .id("paused-worktree-code-folders")
                    .max_h(vh(0.5, window))
                    .overflow_y_scroll()
                    .child(v_flex().gap_2().children(folders)),
            )
            .when(self.written_since_shown, |this| {
                this.child(render_notice(
                    IconName::Warning,
                    Color::Warning,
                    "開いたあとにエージェントがさらに書き込みました。変更を確認してから、もう一度 [再開] を押してください。",
                ))
            })
            .when(anything_paused, |this| {
                this.child(
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new("paused-worktree-code-agent-diff", "エージェントの変更を見る")
                                .style(ButtonStyle::Subtle)
                                .on_click(cx.listener(|_, _, window, cx| {
                                    if let Some(action) =
                                        cx.build_action("agent::OpenAgentDiff", None).log_err()
                                    {
                                        window.dispatch_action(action, cx);
                                    }
                                })),
                        )
                        .child(
                            Button::new("paused-worktree-code-git-panel", "Git パネルを開く")
                                .style(ButtonStyle::Subtle)
                                .on_click(cx.listener(|_, _, window, cx| {
                                    window.dispatch_action(
                                        zed_actions::git_panel::ToggleFocus.boxed_clone(),
                                        cx,
                                    );
                                })),
                        ),
                )
            })
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("paused-worktree-code-close", "閉じる")
                            .style(ButtonStyle::Subtle)
                            .on_click(cx.listener(|_, _, _window, cx| cx.emit(DismissEvent))),
                    )
                    .when(anything_paused, |this| {
                        this.child(
                            Button::new("paused-worktree-code-resume", "再開")
                                .style(ButtonStyle::Filled)
                                .disabled(!resumable)
                                .on_click(cx.listener(|this, _, _window, cx| this.resume(cx))),
                        )
                    }),
            )
    }
}
