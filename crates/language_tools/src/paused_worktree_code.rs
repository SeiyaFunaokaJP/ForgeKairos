//! ADR-F50: what a project's folders hold back after an agent wrote into
//! them, and the user's resume.
//!
//! The status bar only counts the paused folders and leads to the dialog: one
//! popup per folder got in the way. Resuming without seeing what is paused,
//! and without a way to what the agent changed, is what the pause exists to
//! prevent. Nothing an agent can reach resumes a folder.

use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

use gpui::{
    Action as _, App, DismissEvent, Empty, Entity, EventEmitter, FocusHandle, Focusable,
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
use ui::{ButtonLike, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{HideStatusItem, ItemHandle, ModalView, StatusItemView, Workspace};

actions!(
    lsp_tool,
    [
        /// Shows what the project's folders do not run after an agent wrote
        /// into them, with a way to review the changes and resume.
        ReviewPausedWorktreeCode
    ]
);

/// A remote server from before ADR-F50 keeps running the folder's code.
const REMOTE_SERVER_DOES_NOT_FOLLOW: &str = i18n::msg!(
    "The remote server is too old, so language servers and formatters on the remote host are not paused"
);

fn remote_server_does_not_follow(details: Option<&PausedWorktreeDetails>) -> bool {
    details.and_then(|details| details.remote_host) == Some(RemoteHostPause::NotFollowing)
}

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ReviewPausedWorktreeCode, window, cx| {
            open_dialog(workspace, window, cx);
        });
    })
    .detach();
}

/// The status bar's count of the project's paused folders.
pub struct PausedWorktreeCodeIndicator {
    execution: Entity<WorktreeCodeExecution>,
    _subscription: Subscription,
}

impl PausedWorktreeCodeIndicator {
    pub fn new(workspace: &Workspace, cx: &mut Context<Self>) -> Self {
        let execution = workspace
            .project()
            .read(cx)
            .worktree_code_execution()
            .clone();
        // A remote server answers after the pause, and may not follow it.
        let subscription = cx.subscribe(&execution, |_, _, _: &WorktreeCodeExecutionEvent, cx| {
            cx.notify()
        });
        Self {
            execution,
            _subscription: subscription,
        }
    }
}

impl Render for PausedWorktreeCodeIndicator {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let execution = self.execution.read(cx);
        let paused_worktrees = execution.paused_worktrees();
        if paused_worktrees.is_empty() {
            return Empty.into_any_element();
        }
        let does_not_follow = paused_worktrees.iter().any(|worktree_id| {
            remote_server_does_not_follow(execution.details(*worktree_id).as_ref())
        });
        let count = paused_worktrees.len();
        let label: SharedString =
            i18n::tn!(count, "{count} folder paused", "{count} folders paused").into();
        let description: SharedString = if does_not_follow {
            i18n::tr(REMOTE_SERVER_DOES_NOT_FOLLOW).into()
        } else {
            i18n::t!("After an agent wrote to them, these folders' code (language servers and formatters) won't run until you review and resume.").into()
        };
        ButtonLike::new("paused-worktree-code-indicator")
            .tab_index(0isize)
            .aria_label(label.clone())
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Icon::new(IconName::Warning)
                            .size(IconSize::Small)
                            .color(Color::Warning),
                    )
                    .child(Label::new(label).size(LabelSize::Small)),
            )
            .tooltip(move |_, cx| {
                Tooltip::with_meta(
                    i18n::t!("Folder Code Paused"),
                    Some(&ReviewPausedWorktreeCode),
                    description.clone(),
                    cx,
                )
            })
            .on_click(|_, window, cx| {
                window.dispatch_action(ReviewPausedWorktreeCode.boxed_clone(), cx);
            })
            .into_any_element()
    }
}

impl StatusItemView for PausedWorktreeCodeIndicator {
    fn set_active_pane_item(
        &mut self,
        _: Option<&dyn ItemHandle>,
        _window: &mut Window,
        _: &mut Context<Self>,
    ) {
    }

    // Hiding it would leave a paused folder with no sign of why its language
    // servers stopped.
    fn hide_setting(&self, _: &App) -> Option<HideStatusItem> {
        None
    }
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
const STOP_DID_NOT_LAND: &str = i18n::msg!(
    "Not stopped yet. Either the host isn't responding or the processes are ignoring the request to exit. They stay in the list if they're still running at the next check."
);

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
        return i18n::t!("started at an unknown time").to_string();
    };
    let seconds = duration.as_secs();
    if seconds == 0 {
        return i18n::t!("running for under a second").to_string();
    }
    if seconds < 60 {
        return i18n::tn!(
            seconds,
            "running for {count} second",
            "running for {count} seconds"
        );
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return i18n::tn!(
            minutes,
            "running for {count} minute",
            "running for {count} minutes"
        );
    }
    i18n::t!("running for {} h {} min", minutes / 60, minutes % 60)
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
                    i18n::t!("process with an unreadable command name").to_string()
                } else {
                    command
                };
                detail_line(i18n::t!(
                    "{command} ({}, PID {}, {})",
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
            i18n::t!("Processes left behind by an agent response are running in the sandbox. You can't resume until they end."),
        ))
        .children(lines)
        .child(
            h_flex().child(
                Button::new("paused-worktree-code-stop-processes", i18n::t!("Stop Leftover Processes"))
                    .style(ButtonStyle::Subtle)
                    .disabled(stopping)
                    .on_click(move |_, window, cx| stop(window, cx)),
            ),
        )
        .when(timed_out, |this| {
            this.child(render_notice(
                IconName::Warning,
                Color::Warning,
                i18n::tr(STOP_DID_NOT_LAND),
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
            this.child(detail_line(i18n::t!(
                "Language servers running in Restricted Mode: {restricted}"
            )))
        })
        .when(!stopped.is_empty(), |this| {
            this.child(detail_line(i18n::t!("Stopped language servers: {stopped}")))
        })
        .when(!formatters.is_empty(), |this| {
            this.child(detail_line(i18n::t!(
                "Skipped formatters: {formatters}"
            )))
        })
        .when(
            details.remote_host == Some(RemoteHostPause::Following),
            |this| {
                this.child(detail_line(
                    i18n::t!("Language servers and formatters on the remote host are paused the same way.").to_string(),
                ))
            },
        )
        .when(details.remote_host == Some(RemoteHostPause::Unknown), |this| {
            this.child(detail_line(
                i18n::t!("Waiting for the remote server to respond.").to_string(),
            ))
        })
        .when(remote_server_does_not_follow(Some(&details)), |this| {
            this.child(render_notice(
                IconName::Warning,
                Color::Warning,
                i18n::tr(REMOTE_SERVER_DOES_NOT_FOLLOW),
            ))
        })
        .when(nothing_held_back, |this| {
            this.child(detail_line(
                i18n::t!("No language servers or formatters have been held back yet. They follow this state when they start or run.")
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
                i18n::t!("You can't resume until the agent's response ends. If it doesn't end, stop the agent."),
            ))
        })
        .when(
            details.sandboxed_writer_running && !details.agent_turn_running && leftovers.is_empty(),
            |this| {
                this.child(render_notice(
                    IconName::Warning,
                    Color::Warning,
                    i18n::t!("You can't resume until the running sessions and commands end."),
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
            .child(Headline::new(i18n::t!("Folder Code Paused")).size(HeadlineSize::Small))
            .child(
                Label::new(if anything_paused {
                    i18n::t!("Since an agent wrote to the following folders, the language servers and formatters that run each folder's code are paused. Review the agent's changes before you resume.")
                } else {
                    i18n::t!("No folders are paused.")
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
                    i18n::t!("An agent wrote again after this dialog opened. Review the changes, then press Resume again."),
                ))
            })
            .when(anything_paused, |this| {
                this.child(
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new("paused-worktree-code-agent-diff", i18n::t!("View Agent Changes"))
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
                            Button::new("paused-worktree-code-git-panel", i18n::t!("Open Git Panel"))
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
                        Button::new("paused-worktree-code-close", i18n::t!("Close"))
                            .style(ButtonStyle::Subtle)
                            .on_click(cx.listener(|_, _, _window, cx| cx.emit(DismissEvent))),
                    )
                    .when(anything_paused, |this| {
                        this.child(
                            Button::new("paused-worktree-code-resume", i18n::t!("Resume"))
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
            // The pause is off by default (ADR-F50); these tests are about it.
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings
                        .agent
                        .get_or_insert_default()
                        .pause_worktree_code_after_agent_writes = Some(true);
                });
            });
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
        assert_eq!(running_for(None), "started at an unknown time");
        assert_eq!(
            running_for(Some(Duration::ZERO)),
            "running for under a second"
        );
        assert_eq!(
            running_for(Some(Duration::from_millis(999))),
            "running for under a second"
        );
        assert_eq!(
            running_for(Some(Duration::from_secs(1))),
            "running for 1 second"
        );
        assert_eq!(
            running_for(Some(Duration::from_secs(59))),
            "running for 59 seconds"
        );
        assert_eq!(
            running_for(Some(Duration::from_secs(60))),
            "running for 1 minute"
        );
        assert_eq!(
            running_for(Some(Duration::from_secs(3599))),
            "running for 59 minutes"
        );
        assert_eq!(
            running_for(Some(Duration::from_secs(3600 * 2 + 60 * 3))),
            "running for 2 h 3 min"
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
