use std::fmt::Display;
use std::panic::Location;
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use collections::HashMap;
use itertools::Itertools;
use log::info;

#[derive(Debug, Hash, Eq, PartialEq)]
enum PerfIssue {
    Foreground(&'static Location<'static>),
    Background(&'static Location<'static>),
    Action(&'static str),
}

pub struct Reporter {
    monitor_interval: Duration,
    forget_after: Duration,
    history: HashMap<PerfIssue, Instant>,
    report_longer_then: Duration,
    foreground_thread: ThreadId,
}

type ReportMade = bool;
impl Reporter {
    pub fn new(
        monitor_interval: Duration,
        report_longer_then: Duration,
        foreground_thread: ThreadId,
    ) -> Self {
        Self {
            monitor_interval,
            forget_after: Duration::from_mins(5),
            history: HashMap::default(),
            report_longer_then,
            foreground_thread,
        }
    }
    pub fn check_and_report(
        &mut self,
        task_stats: &[gpui::ThreadTaskStatistics],
        action_stats: &gpui::ActionStatistics,
    ) -> ReportMade {
        let mut reported_task_hangs = false;
        reported_task_hangs |= self.report_hanging_foreground(&task_stats);
        reported_task_hangs |= self.report_hanging_background(&task_stats);

        self.report_hanging_actions(action_stats);
        reported_task_hangs
    }

    fn hold_report(&self, issue: PerfIssue) -> bool {
        self.history
            .get(&issue)
            .map(Instant::elapsed)
            .is_some_and(|since_report| since_report > self.monitor_interval)
    }
    fn update_reported(&mut self, new: impl Iterator<Item = PerfIssue>) {
        let now = Instant::now();
        for issue in new {
            if self
                .history
                .get(&issue)
                .is_none_or(|s| s.elapsed() > self.forget_after)
            {
                self.history.insert(issue, now);
            }
        }
    }
}

impl Reporter {
    fn report_hanging_foreground(
        &mut self,
        task_stats: &[gpui::ThreadTaskStatistics],
    ) -> ReportMade {
        let foreground_thread = self.foreground_thread;
        let Some(foreground) = task_stats.iter().find(|t| t.thread_id == foreground_thread) else {
            return false; // during startup the foreground may not yet have statistics
        };

        let hangs: Vec<_> = foreground
            .stats
            .longest_poll_times
            .into_iter()
            // A native dialog runs its modal loop inside the poll that opened
            // it: waiting for the user to answer is not a hang.
            .map(|task| {
                let wait = task.native_modal_wait(foreground_thread);
                ReportedPoll {
                    location: task.location,
                    duration: task.poll_duration().saturating_sub(wait.waited),
                    inside_native_dialog: wait.stuck_inside,
                }
            })
            .filter(|poll| poll.duration > self.report_longer_then)
            .filter(|poll| !self.hold_report(PerfIssue::Foreground(poll.location)))
            .collect();
        self.update_reported(
            hangs
                .iter()
                .map(|poll| PerfIssue::Foreground(poll.location)),
        );
        if !hangs.is_empty() {
            info!("New foreground hang detected:\n{}", DisplayTasks(&hangs));
        }

        !hangs.is_empty()
    }

    fn report_hanging_background(
        &mut self,
        task_stats: &[gpui::ThreadTaskStatistics],
    ) -> ReportMade {
        let foreground = self.foreground_thread;
        let background = task_stats.iter().filter(move |t| t.thread_id != foreground);

        let mut report_made = false;
        for worker in background {
            let hangs: Vec<_> = worker
                .stats
                .longest_poll_times
                .into_iter()
                .map(|task| ReportedPoll {
                    location: task.location,
                    duration: task.poll_duration(),
                    inside_native_dialog: false,
                })
                .filter(|poll| poll.duration > self.report_longer_then)
                .filter(|poll| !self.hold_report(PerfIssue::Background(poll.location)))
                .collect();

            if hangs.is_empty() {
                continue;
            }

            self.update_reported(
                hangs
                    .iter()
                    .map(|poll| PerfIssue::Background(poll.location)),
            );

            info!(
                "Background hang detected on {}:\n{}",
                worker.thread_name.as_deref().unwrap_or_else(|| "Unknown"),
                DisplayTasks(&hangs)
            );
            report_made = true;
        }
        report_made
    }

    fn report_hanging_actions(&mut self, action_stats: &gpui::ActionStatistics) {
        let hangs: Vec<_> = action_stats
            .longest_runtimes(true)
            .filter(|action| action.runtime() > self.report_longer_then)
            .filter(|action| !self.hold_report(PerfIssue::Action(action.name)))
            .collect();

        self.update_reported(hangs.iter().map(|action| PerfIssue::Action(action.name)));
        if !hangs.is_empty() {
            info!("Action hang detected:\n{}", DisplayActions(hangs));
        }
    }
}

struct DisplayActions(Vec<gpui::profiler::ActionTiming>);
struct DisplayTasks<'a>(&'a [ReportedPoll]);

/// A poll as reported: on the foreground, without its waits in native dialogs.
#[derive(Clone, Copy)]
struct ReportedPoll {
    location: &'static Location<'static>,
    duration: Duration,
    /// The poll hung with a native dialog it opened still open, so Windows'
    /// own dialog code (a slow network folder, a shell extension) may be what
    /// hung rather than Zed.
    inside_native_dialog: bool,
}

impl Display for DisplayActions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Actions(s) that ran too long\n")?;
        for action in self.0.iter().sorted_by_key(|action| action.runtime()).rev() {
            f.write_fmt(format_args!(
                "{:<20} - {}",
                format!("{:?}", action.runtime()), // impl debug does not support alignment
                action.name
            ))?;
            writeln!(f)?;
        }
        Ok(())
    }
}

impl<'a> Display for DisplayTasks<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Tasks(s) that ran too long\n")?;
        for poll in self.0.iter().sorted_by_key(|poll| poll.duration).rev() {
            f.write_fmt(format_args!(
                "{:<20} - {}{}",
                format!("{:?}", poll.duration), // impl debug does not support alignment
                poll.location,
                if poll.inside_native_dialog {
                    " (inside a native dialog)"
                } else {
                    ""
                }
            ))?;
            writeln!(f)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::thread;

    use super::*;

    fn foreground_stats(
        thread_id: ThreadId,
        timing: gpui::TaskTiming,
    ) -> gpui::ThreadTaskStatistics {
        let mut stats = gpui::TaskStatistics::default();
        stats.longest_poll_times[0] = timing;
        gpui::ThreadTaskStatistics {
            thread_name: None,
            thread_id,
            stats,
        }
    }

    #[test]
    fn a_hang_inside_a_native_dialog_is_tagged_in_the_log() {
        let polls = [
            ReportedPoll {
                location: Location::caller(),
                duration: Duration::from_millis(150),
                inside_native_dialog: true,
            },
            ReportedPoll {
                location: Location::caller(),
                duration: Duration::from_millis(120),
                inside_native_dialog: false,
            },
        ];
        let log = DisplayTasks(&polls).to_string();
        let lines = log.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 3, "{log}");
        assert!(
            lines[1].starts_with("150ms") && lines[1].ends_with(" (inside a native dialog)"),
            "{log}"
        );
        assert!(
            lines[2].starts_with("120ms") && !lines[2].contains("native dialog"),
            "{log}"
        );
    }

    fn poll_since(start: Instant) -> gpui::TaskTiming {
        poll_between(start, Instant::now())
    }

    fn poll_between(start: Instant, end: Instant) -> gpui::TaskTiming {
        let mut timing = gpui::TaskTiming::placeholder();
        timing.start = start;
        timing.end = gpui::YieldTime(end);
        timing
    }

    #[test]
    fn an_open_dialog_whose_thread_stopped_handling_messages_is_a_hang() {
        let foreground_thread = thread::current().id();
        let mut reporter = Reporter::new(
            Duration::from_secs(1),
            Duration::from_millis(10),
            foreground_thread,
        );
        let no_actions = gpui::ActionStatistics::default();
        let start = Instant::now();
        let dialog = gpui::profiler::native_modal_opened();
        gpui::profiler::note_message_handled();

        // The watchdog times a running poll up to when it looks, so looking
        // ahead stands in for waiting.
        let half_a_second_on = Instant::now() + Duration::from_millis(500);
        let stats = [foreground_stats(
            foreground_thread,
            poll_between(start, half_a_second_on),
        )];
        assert!(
            !reporter.check_and_report(&stats, &no_actions),
            "messages came just now, so the dialog is waiting for the user"
        );

        let three_seconds_on = Instant::now() + Duration::from_secs(3);
        let stats = [foreground_stats(
            foreground_thread,
            poll_between(start, three_seconds_on),
        )];
        assert!(
            reporter.check_and_report(&stats, &no_actions),
            "no message for well past the slack: the thread is stuck, not waiting"
        );
        drop(dialog);
    }

    #[test]
    fn waiting_in_a_native_dialog_is_not_a_hang() {
        let foreground_thread = thread::current().id();
        let mut reporter = Reporter::new(
            Duration::from_secs(1),
            Duration::from_millis(10),
            foreground_thread,
        );
        let no_actions = gpui::ActionStatistics::default();

        let start = Instant::now();
        let dialog = gpui::profiler::native_modal_opened();
        thread::sleep(Duration::from_millis(30));
        drop(dialog);
        let stats = [foreground_stats(foreground_thread, poll_since(start))];
        assert!(!reporter.check_and_report(&stats, &no_actions));

        let start = Instant::now();
        thread::sleep(Duration::from_millis(30));
        let stats = [foreground_stats(foreground_thread, poll_since(start))];
        assert!(
            reporter.check_and_report(&stats, &no_actions),
            "blocking without a dialog is still a hang"
        );
    }
}
