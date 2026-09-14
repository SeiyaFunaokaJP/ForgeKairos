use std::thread::ThreadId;
use std::time::{Duration, Instant};

use collections::HashMap;
use hdrhistogram::Histogram;
use itertools::Itertools;

use crate::STARTUP_TIME;

/// Microseconds since app start
type MicroSeconds = u64;

// TODO(yara) some crazy ideas:
// - track most recent action?
// - Action that this task was spawned from?
// - flag that enables tracking more for a specific task
// - task backtrace? (who spawned who etc)

#[derive(Debug, Clone, serde::Serialize)]
struct HangReport {
    location: String,
    hang_density: f64,
    mean_hang_duration: MicroSeconds,

    /// since app start
    slowest_start: MicroSeconds,
    /// since app start
    slowest_end: MicroSeconds,

    /// 50% of bad polls where faster then this value
    /// This also approximate the median
    p50: MicroSeconds,
    /// 75% of bad polls where faster then this value
    p75: MicroSeconds,
    /// 95% of bad polls where faster then this value
    p95: MicroSeconds,

    /// A slow poll here hung with a native dialog it opened still open, so
    /// Windows' own dialog code (a slow network folder, a shell extension) may
    /// be what was slow.
    inside_native_dialog: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
struct TelemetryReport {
    foreground: Vec<HangReport>,
    background: Vec<HangReport>,
    actions: Vec<HangReport>,
}

struct Item<T: Hang> {
    total_hanged: Duration,
    slowest_poll: T,
    /// saturates if more then 256 measurements end up in the same bin
    histogram: Histogram<u8>,
    inside_native_dialog: bool,
}

impl<T: Hang> Item<T> {
    fn hang_density(&self, period: Duration) -> f64 {
        self.total_hanged.div_duration_f64(period)
    }
    fn into_report(&self, location: &T::Descriptor, period: Duration) -> HangReport {
        let now = Instant::now();
        let startup = STARTUP_TIME.get().unwrap_or(&now);
        HangReport {
            location: T::format_location(location),
            hang_density: self.hang_density(period),
            mean_hang_duration: self.histogram.mean() as u64,
            slowest_start: self
                .slowest_poll
                .start()
                .duration_since(*startup)
                .as_micros() as u64,
            slowest_end: self.slowest_poll.end().duration_since(*startup).as_micros() as u64,
            p50: self.histogram.value_at_quantile(0.5),
            p75: self.histogram.value_at_quantile(0.75),
            p95: self.histogram.value_at_quantile(0.95),
            inside_native_dialog: self.inside_native_dialog,
        }
    }
}

struct Hangs<T: Hang> {
    last_reset: Instant,
    hangs: HashMap<T::Descriptor, Item<T>>,
}

trait Hang: Clone {
    type Descriptor: std::hash::Hash + PartialEq + Eq;
    fn poll_duration(&self) -> Duration;
    fn descriptor(&self) -> Self::Descriptor;
    fn format_location(location: &Self::Descriptor) -> String;
    fn start(&self) -> Instant;
    fn end(&self) -> Instant;
    fn inside_native_dialog(&self) -> bool {
        false
    }
}

impl Hang for gpui::TaskTiming {
    type Descriptor = std::panic::Location<'static>;

    fn poll_duration(&self) -> Duration {
        gpui::TaskTiming::poll_duration(self)
    }
    fn descriptor(&self) -> Self::Descriptor {
        *self.location
    }
    fn format_location(location: &Self::Descriptor) -> String {
        format!(
            "{}:{}:{}",
            location.file(),
            location.line(),
            location.column()
        )
    }
    fn start(&self) -> Instant {
        self.start
    }
    fn end(&self) -> Instant {
        self.end.0
    }
}

impl Hang for gpui::ActionTiming {
    type Descriptor = &'static str;

    fn poll_duration(&self) -> Duration {
        self.duration()
    }
    fn descriptor(&self) -> Self::Descriptor {
        self.name
    }
    fn format_location(location: &Self::Descriptor) -> String {
        location.to_string()
    }
    fn start(&self) -> Instant {
        self.start
    }
    fn end(&self) -> Instant {
        self.end
    }
}

/// A foreground poll, timed without the waits in native modal dialogs it
/// opened: the user answering a dialog is not a hang.
#[derive(Clone)]
struct ForegroundPoll {
    timing: gpui::TaskTiming,
    busy: Duration,
    inside_native_dialog: bool,
}

impl Hang for ForegroundPoll {
    type Descriptor = std::panic::Location<'static>;

    fn poll_duration(&self) -> Duration {
        self.busy
    }
    fn descriptor(&self) -> Self::Descriptor {
        *self.timing.location
    }
    fn format_location(location: &Self::Descriptor) -> String {
        <gpui::TaskTiming as Hang>::format_location(location)
    }
    fn start(&self) -> Instant {
        self.timing.start
    }
    fn end(&self) -> Instant {
        self.timing.end.0
    }
    fn inside_native_dialog(&self) -> bool {
        self.inside_native_dialog
    }
}

impl<T: Hang> Hangs<T> {
    fn new() -> Self {
        Self {
            last_reset: Instant::now(),
            hangs: HashMap::default(),
        }
    }
    fn add(&mut self, new: T, min_recorded_us: u64) {
        const MICROSECONDS_MINUTE: u64 = 60 * 1000 * 1000;

        if self.hangs.len() > 1000 {
            log::warn!("Too many hanging tasks to track, can not add new");
            return;
        }

        let inside_native_dialog = new.inside_native_dialog();
        self.hangs
            .entry(new.descriptor())
            .and_modify(|item| {
                item.inside_native_dialog |= inside_native_dialog;
                item.total_hanged += new.poll_duration();
                item.histogram
                    .saturating_record(new.poll_duration().as_micros() as u64);
                if new.poll_duration() > item.slowest_poll.poll_duration() {
                    item.slowest_poll = new.clone();
                }
            })
            .or_insert({
                Item {
                    total_hanged: new.poll_duration(),
                    slowest_poll: new,
                    inside_native_dialog,
                    histogram: Histogram::new_with_bounds(min_recorded_us, MICROSECONDS_MINUTE, 3)
                        .expect("function parameters are constants and correct"),
                }
            });
    }

    fn report_and_reset(&mut self) -> Vec<HangReport> {
        let period = self.last_reset.elapsed();
        self.last_reset = Instant::now();

        let lowest_density_to_report = self
            .hangs
            .values()
            .map(|item| item.total_hanged.div_duration_f64(period))
            .k_largest_relaxed_by(5, f64::total_cmp)
            .nth(5)
            .unwrap_or(0.0);

        let report = self
            .hangs
            .drain()
            .filter(|(_, item)| item.hang_density(period) >= lowest_density_to_report)
            .map(|(location, item)| item.into_report(&location, period))
            .collect();

        report
    }

    fn is_empty(&self) -> bool {
        self.hangs.is_empty()
    }
}

pub struct Reporter {
    record_slower_then: Duration,
    foreground_thread: ThreadId,
    last_send: Instant,

    foreground: Hangs<ForegroundPoll>,
    background: Hangs<gpui::TaskTiming>,
    actions: Hangs<gpui::ActionTiming>,
}

impl Reporter {
    pub fn new(foreground_thread: ThreadId) -> Self {
        Self {
            record_slower_then: Duration::from_millis(1),
            foreground_thread,
            last_send: Instant::now(),
            foreground: Hangs::new(),
            background: Hangs::new(),
            actions: Hangs::new(),
        }
    }

    pub fn update(
        &mut self,
        task_stats: &[gpui::ThreadTaskStatistics],
        action_stats: &gpui::ActionStatistics,
    ) {
        self.process_foreground(task_stats);
        self.process_background(task_stats);
        self.process_actions(action_stats);
    }

    pub fn send_periodically(&mut self) {
        // this should be a long period otherwise things like
        // hang density get
        if self.last_send.elapsed() > Duration::from_mins(30) {
            self.send()
        }
    }

    pub fn send(&mut self) {
        self.last_send = Instant::now();
        if self.nothing_to_report() {
            return;
        }
        let report = TelemetryReport {
            foreground: self.foreground.report_and_reset(),
            background: self.background.report_and_reset(),
            actions: self.actions.report_and_reset(),
        };

        telemetry::event!("Hang Report", report);
    }

    fn process_foreground(&mut self, task_stats: &[gpui::ThreadTaskStatistics]) {
        let foreground_thread = self.foreground_thread;
        let Some(foreground) = task_stats.iter().find(|t| t.thread_id == foreground_thread) else {
            // during startup foreground thread might not have statistics yet
            return;
        };

        for timing in foreground.stats.longest_poll_times {
            let wait = timing.native_modal_wait(foreground_thread);
            let busy = timing.poll_duration().saturating_sub(wait.waited);
            if busy > self.record_slower_then {
                self.foreground.add(
                    ForegroundPoll {
                        timing,
                        busy,
                        inside_native_dialog: wait.stuck_inside,
                    },
                    self.record_slower_then.as_micros() as u64,
                );
            }
        }
    }
    fn process_background(&mut self, task_stats: &[gpui::ThreadTaskStatistics]) {
        let foreground_thread = self.foreground_thread;
        let background = task_stats
            .iter()
            .filter(|t| t.thread_id != foreground_thread);

        for worker in background {
            for hang in worker
                .stats
                .longest_poll_times
                .into_iter()
                .filter(|task| task.poll_duration() > self.record_slower_then)
            {
                self.background
                    .add(hang, self.record_slower_then.as_micros() as u64);
            }
        }
    }

    fn process_actions(&mut self, action_stats: &gpui::ActionStatistics) {
        for hang in action_stats
            .longest_runtimes(false)
            .filter(|action| action.runtime() > self.record_slower_then)
        {
            self.actions
                .add(hang, self.record_slower_then.as_micros() as u64);
        }
    }

    fn nothing_to_report(&self) -> bool {
        self.actions.is_empty() && self.foreground.is_empty() && self.background.is_empty()
    }
}
