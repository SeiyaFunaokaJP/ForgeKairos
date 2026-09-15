//! ADR-F50: what a project's folders hold back after an agent wrote into
//! them, and the user's resume.
//!
//! The notification only leads to the dialog: resuming without seeing what is
//! paused, and without a way to what the agent changed, is what the pause
//! exists to prevent. Nothing an agent can reach resumes a folder.

use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

use gpui::{
    Action as _, App, AppContext as _, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    Subscription, Task, actions,
};
use project::{
    Project, WorktreeCodeExecution, WorktreeId,
    project_launch_approvals::escape_for_display,
    worktree_code_execution::{
        LeftoverSandboxProcessReports, LeftoverSandboxProcesses, LeftoverSandboxProcessesId,
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

/// ADR-F49 decision 18 (C): how long the dialog waits for a stop it asked for
/// to show in the list before it says it has not landed.
///
/// Longer than the agent's monitor gives the request itself (`acp_thread`'s
/// own `STOP_TIMEOUT`, 10 s) plus the listing it takes straight afterwards: a
/// host that answers at 9.9 s would otherwise be reported as not having
/// answered, a tenth of a second before its answer could reach the list.
const STOP_TIMEOUT: Duration = Duration::from_secs(15);

/// How often the list is looked at while that wait runs, so the button comes
/// back as soon as the stop lands rather than at the end of the wait. The
/// agent's own monitor is what refreshes the list; this only reads it.
const STOP_CHECK_INTERVAL: Duration = Duration::from_millis(500);

/// What the dialog says once [`STOP_TIMEOUT`] has passed and the processes are
/// still listed. It names no culprit: the host may not be answering, and the
/// process may be ignoring the signal to end (it is killed after the grace
/// period, which the next look reports). Giving up on the wait is not giving
/// up on the stop — the monitor keeps looking, and the list is what says
/// whether it worked.
const STOP_DID_NOT_LAND: &str = "まだ止まっていません。ホストが応答していないか、プロセスが終了を無視しています。次の確認でまだ動いていれば一覧に残ります。";

struct PausedWorktreeCodeDialog {
    project: Entity<Project>,
    focus_handle: FocusHandle,
    shown_generations: HashMap<WorktreeId, u64>,
    /// An agent wrote again after the dialog showed the folders.
    written_since_shown: bool,
    resuming: Option<Task<()>>,
    /// ADR-F49 decision 18 (C): one wait per folder whose stop the user asked
    /// for and which has neither shown in the list nor been given up on. Per
    /// folder because the button and the warning are: a slow host behind one
    /// agent must not disable the other folder's button or warn about it.
    stopping: HashMap<WorktreeId, Task<()>>,
    /// And the folders whose last stop had not landed when its wait ran out.
    stops_that_did_not_land: HashSet<WorktreeId>,
    _execution_subscription: Subscription,
    /// ADR-F49 decision 18 (C): what agents left running in their sandboxes
    /// changes on its own while the dialog is open.
    _leftover_processes_subscription: Subscription,
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
        let leftover_processes = LeftoverSandboxProcessReports::global(cx);
        let leftover_processes_subscription =
            cx.observe(&leftover_processes, |_, _, cx| cx.notify());
        Self {
            project,
            focus_handle,
            shown_generations,
            written_since_shown: false,
            resuming: None,
            stopping: HashMap::default(),
            stops_that_did_not_land: HashSet::default(),
            _execution_subscription: execution_subscription,
            _leftover_processes_subscription: leftover_processes_subscription,
        }
    }

    /// ADR-F49 decision 18 (C): asks the agents that left them to stop exactly
    /// what `worktree_id`'s block is showing, and gives up waiting for it to
    /// show after [`STOP_TIMEOUT`]. Giving up only ends the wait: the agent's
    /// monitor keeps looking at its sandbox, so a stop that lands late still
    /// clears the list and lets the resume through.
    fn stop_leftovers(
        &mut self,
        worktree_id: WorktreeId,
        reports: Vec<LeftoverSandboxProcessesId>,
        cx: &mut Context<Self>,
    ) {
        self.stops_that_did_not_land.remove(&worktree_id);
        for report in &reports {
            LeftoverSandboxProcessReports::stop(*report, cx);
        }
        let wait = cx.spawn(async move |this, cx| {
            // The reports go away on their own once the processes do, so what
            // is still listed is what the stop has not reached yet.
            let mut waited = Duration::ZERO;
            while waited < STOP_TIMEOUT {
                cx.background_executor().timer(STOP_CHECK_INTERVAL).await;
                waited += STOP_CHECK_INTERVAL;
                let Ok(still_listed) = this.update(cx, |this, cx| this.still_listed(&reports, cx))
                else {
                    return;
                };
                if !still_listed {
                    this.update(cx, |this, cx| {
                        this.stopping.remove(&worktree_id);
                        cx.notify();
                    })
                    .log_err();
                    return;
                }
            }
            this.update(cx, |this, cx| {
                this.stopping.remove(&worktree_id);
                this.stops_that_did_not_land.insert(worktree_id);
                cx.notify();
            })
            .log_err();
        });
        self.stopping.insert(worktree_id, wait);
        cx.notify();
    }

    /// Whether a stop the user asked for on `worktree_id` is still being
    /// waited on, which is what disables that folder's button.
    fn is_stopping(&self, worktree_id: WorktreeId) -> bool {
        self.stopping.contains_key(&worktree_id)
    }

    /// And whether the last one there was given up on.
    fn stop_did_not_land(&self, worktree_id: WorktreeId) -> bool {
        self.stops_that_did_not_land.contains(&worktree_id)
    }

    /// Whether any of `reports` still lists a process of a folder this dialog
    /// shows.
    fn still_listed(&self, reports: &[LeftoverSandboxProcessesId], cx: &App) -> bool {
        let project = self.project.read(cx);
        let execution = project.worktree_code_execution().read(cx);
        execution.paused_worktrees().into_iter().any(|worktree_id| {
            execution
                .leftover_sandbox_processes(worktree_id, cx)
                .iter()
                .any(|listed| reports.contains(&listed.id))
        })
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

/// ADR-F49 decision 18 (C): how long a leftover process has been running, in
/// the units a user reads at a glance. A process the host could not date at
/// all reads differently from one that started within the last second — which
/// is what an answer's own leftover looks like the moment it is first listed.
fn running_for(duration: Option<Duration>) -> String {
    let Some(duration) = duration else {
        return "起動時間不明".to_string();
    };
    let seconds = duration.as_secs();
    if seconds == 0 {
        return "1 秒未満前から".to_string();
    }
    if seconds < 60 {
        return format!("{seconds} 秒前から");
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{minutes} 分前から");
    }
    format!("{} 時間 {} 分前から", minutes / 60, minutes % 60)
}

fn render_leftover_processes(
    leftovers: &[LeftoverSandboxProcesses],
    stopping: bool,
    timed_out: bool,
    stop: impl Fn(&mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let lines = leftovers
        .iter()
        .flat_map(|report| {
            let agent = escape_for_display(report.agent.as_ref()).into_owned();
            report.processes.iter().map(move |process| {
                // A process that cleared its own command line, or one whose
                // host could not read it, is named by what is left: its pid.
                let command = escape_for_display(process.command.as_ref()).into_owned();
                let command = if command.is_empty() {
                    "コマンド名が読めないプロセス".to_string()
                } else {
                    command
                };
                detail_line(format!(
                    "{command} ({}、PID {}、{})",
                    agent,
                    process.pid,
                    running_for(process.running_for),
                ))
            })
        })
        .collect::<Vec<_>>();
    v_flex()
        .w_full()
        .min_w_0()
        .gap_0p5()
        .child(render_notice(
            IconName::Warning,
            Color::Warning,
            "エージェントの応答が残したプロセスがサンドボックスの中で動いています。終わるまで再開できません。",
        ))
        .children(lines)
        .child(
            h_flex().child(
                Button::new("paused-worktree-code-stop-processes", "残ったプロセスを止める")
                    .style(ButtonStyle::Subtle)
                    .disabled(stopping)
                    .on_click(move |_, window, cx| stop(window, cx)),
            ),
        )
        .when(timed_out, |this| {
            this.child(render_notice(
                IconName::Warning,
                Color::Warning,
                STOP_DID_NOT_LAND,
            ))
        })
}

fn render_folder(
    name: String,
    details: PausedWorktreeDetails,
    leftovers: Vec<LeftoverSandboxProcesses>,
    stopping: bool,
    stop_timed_out: bool,
    stop_leftovers: impl Fn(&mut Window, &mut App) + 'static,
) -> impl IntoElement {
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
        .when(!leftovers.is_empty(), |this| {
            this.child(render_leftover_processes(
                &leftovers,
                stopping,
                stop_timed_out,
                stop_leftovers,
            ))
        })
        .when(details.agent_turn_running && leftovers.is_empty(), |this| {
            this.child(render_notice(
                IconName::Warning,
                Color::Warning,
                "エージェントの応答が終わるまで再開できません。終わらないときは、エージェントを止めてください。",
            ))
        })
        .when(
            details.sandboxed_writer_running && !details.agent_turn_running && leftovers.is_empty(),
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
        let dialog = cx.weak_entity();
        let folders = execution
            .paused_worktrees()
            .into_iter()
            .filter_map(|worktree_id| {
                let details = execution.details(worktree_id)?;
                writer_running |= details.sandboxed_writer_running;
                let leftovers = execution.leftover_sandbox_processes(worktree_id, cx);
                let reports = leftovers.iter().map(|report| report.id).collect::<Vec<_>>();
                let dialog = dialog.clone();
                Some(render_folder(
                    folder_name(project, worktree_id, cx)?,
                    details,
                    leftovers,
                    self.is_stopping(worktree_id),
                    self.stop_did_not_land(worktree_id),
                    move |_window, cx| {
                        dialog
                            .update(cx, |this, cx| {
                                this.stop_leftovers(worktree_id, reports.clone(), cx)
                            })
                            .log_err();
                    },
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

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use project::{AgentWriteScope, FakeFs, LeftoverSandboxProcess};
    use serde_json::json;
    use settings::SettingsStore;
    use std::{path::PathBuf, rc::Rc};
    use util::path;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            release_channel::init(semver::Version::new(0, 0, 0), cx);
        });
    }

    /// ADR-F49 decision 18 (C): a leftover the host could not date reads
    /// differently from one that has only just started. They used to read the
    /// same, which said "起動時間不明" about the ordinary case — a process an
    /// answer started in the second before it ended.
    #[test]
    fn an_unknown_age_is_not_a_process_that_just_started() {
        assert_eq!(running_for(None), "起動時間不明");
        assert_eq!(running_for(Some(Duration::ZERO)), "1 秒未満前から");
        assert_eq!(
            running_for(Some(Duration::from_millis(999))),
            "1 秒未満前から"
        );
        assert_eq!(running_for(Some(Duration::from_secs(1))), "1 秒前から");
        assert_eq!(running_for(Some(Duration::from_secs(59))), "59 秒前から");
        assert_eq!(running_for(Some(Duration::from_secs(60))), "1 分前から");
        assert_eq!(running_for(Some(Duration::from_secs(3599))), "59 分前から");
        assert_eq!(
            running_for(Some(Duration::from_secs(3600 * 2 + 60 * 3))),
            "2 時間 3 分前から"
        );
    }

    /// ADR-F49 decision 18 (C): the wait on a stop belongs to the folder it
    /// was asked for.
    ///
    /// It is longer than the monitor's own cap on the request plus the listing
    /// that follows it (M1: at 10 s both ran out together and a host that
    /// answered at 9.9 s was reported as silent), it ends the moment the
    /// processes leave the list, and neither it nor the warning it ends with
    /// touches another folder's block (M2).
    #[gpui::test]
    async fn a_stop_is_waited_on_per_folder_and_outlasts_the_request(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/one"), json!({ "a.rs": "" })).await;
        fs.insert_tree(path!("/two"), json!({ "b.rs": "" })).await;
        let project = Project::test(
            fs.clone(),
            [path!("/one").as_ref(), path!("/two").as_ref()],
            cx,
        )
        .await;
        let (one, two) = project.read_with(cx, |project, cx| {
            let mut worktrees = project.worktrees(cx);
            let one = worktrees.next().expect("the first folder").read(cx).id();
            let two = worktrees.next().expect("the second folder").read(cx).id();
            (one, two)
        });
        let host = project
            .read_with(cx, |project, cx| {
                project
                    .worktree_code_execution()
                    .read(cx)
                    .host_key()
                    .map(str::to_string)
            })
            .expect("a local project has a host key");

        // Both folders are paused; only the first has an agent's leftover, and
        // the stop it offers never takes effect.
        let report = cx.update(|cx| {
            WorktreeCodeExecution::note_agent_write(&host, &AgentWriteScope::Anywhere, cx);
            let report = LeftoverSandboxProcessReports::publish(
                &host,
                AgentWriteScope::Paths(vec![PathBuf::from(path!("/one"))]),
                "fake-agent".into(),
                Rc::new(|_cx: &mut App| {}),
                cx,
            );
            LeftoverSandboxProcessReports::report(
                report.id(),
                vec![LeftoverSandboxProcess {
                    pid: 7,
                    command: "npm run dev".into(),
                    running_for: Some(Duration::from_secs(3)),
                }],
                cx,
            );
            report
        });
        cx.run_until_parked();
        let paused = project.read_with(cx, |project, cx| {
            project
                .worktree_code_execution()
                .read(cx)
                .paused_worktrees()
        });
        assert!(
            paused.contains(&one) && paused.contains(&two),
            "both folders are paused: {paused:?}"
        );

        let window =
            cx.add_window(|window, cx| PausedWorktreeCodeDialog::new(project.clone(), window, cx));
        let state = |cx: &mut TestAppContext| {
            window
                .read_with(cx, |dialog, _| {
                    (
                        dialog.is_stopping(one),
                        dialog.stop_did_not_land(one),
                        dialog.is_stopping(two),
                        dialog.stop_did_not_land(two),
                    )
                })
                .expect("the dialog is open")
        };
        let press_stop = |cx: &mut TestAppContext| {
            window
                .update(cx, |dialog, _window, cx| {
                    dialog.stop_leftovers(one, vec![report.id()], cx)
                })
                .expect("the dialog is open");
        };

        press_stop(cx);
        assert_eq!(
            state(cx),
            (true, false, false, false),
            "the folder the stop was asked for waits; the other is untouched"
        );

        // Past what the monitor gives the request itself, and still waiting.
        cx.executor().advance_clock(Duration::from_secs(12));
        cx.run_until_parked();
        assert_eq!(state(cx), (true, false, false, false));

        cx.executor().advance_clock(Duration::from_secs(4));
        cx.run_until_parked();
        assert_eq!(
            state(cx),
            (false, true, false, false),
            "the wait ends on that folder alone"
        );

        // Asked again, and this time the processes leave the list: the button
        // comes back at the next look rather than at the end of the wait.
        press_stop(cx);
        assert_eq!(state(cx), (true, false, false, false));
        cx.update(|cx| LeftoverSandboxProcessReports::report(report.id(), Vec::new(), cx));
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        assert_eq!(state(cx), (false, false, false, false));
    }
}
