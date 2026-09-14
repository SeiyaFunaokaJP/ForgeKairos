use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use client::Client;
use gpui::{AppContext, TasksIncluded, profiler};
use parking_lot::Mutex;
use ui::App;

mod logging;
mod task_traces;
mod telemetry;

gpui::actions!(
    dev,
    [
        /// Causes a performance hang to test performance monitoring
        HangAction,
        /// Causes a performance hang to test performance monitoring
        HangBackground,
        /// Causes a performance hang to test performance monitoring
        HangForeground,
    ]
);

pub(crate) fn start(client: Arc<Client>, cx: &mut App) {
    let hang_time = if cfg!(debug_assertions) {
        if cfg!(windows) {
            // yes windows debug builds are horribly slow
            Duration::from_secs(30)
        } else {
            Duration::from_secs(5)
        }
    } else {
        // will be lowered over time or turned into a setting
        Duration::from_millis(100)
    };

    if cfg!(debug_assertions) {
        log::warn!("debug build, only reporting hangs longer then {hang_time:?}");
    }

    start_hang_detection(hang_time, client, cx);

    cx.on_action(move |_: &HangAction, _| {
        log::warn!(
            "Hanging the foreground for {hang_time:?} by blocking in an action. \
            Zed will be unresponsive for that time. This should trigger a report in the log",
        );
        thread::sleep(hang_time + Duration::from_micros(1));
        log::warn!("Hang ended");
    });
    cx.on_action(move |_: &HangBackground, cx| {
        cx.background_spawn(async move {
            log::warn!(
                "Hanging one background executor for {hang_time:?}. \
                This should trigger a report in the log",
            );
            thread::sleep(hang_time + Duration::from_micros(1));
            log::warn!("Hang ended");
        })
        .detach();
    });
    cx.on_action(move |_: &HangForeground, cx| {
        cx.spawn(async move |_| {
            log::warn!(
                "Hanging the foreground executor for {hang_time:?} seconds to test \
                performance monitoring! Zed will be unresponsive for that time. \
                This should trigger a report in the log"
            );
            thread::sleep(hang_time + Duration::from_micros(1));
            log::warn!("Hang ended");
        })
        .detach();
    });
}

fn start_hang_detection(report_longer_then: Duration, client: Arc<Client>, cx: &App) {
    let foreground_thread = thread::current().id();
    let monitor_interval = Duration::from_secs(1);
    let telemetry = Arc::new(Mutex::new(telemetry::Reporter::new(foreground_thread)));
    let mut log = logging::Reporter::new(monitor_interval, report_longer_then, foreground_thread);

    let telemetry2 = Arc::clone(&telemetry);
    cx.on_app_quit({
        move |_| {
            telemetry2.lock().send();
            client.telemetry().flush_events()
        }
    })
    .detach();

    // an OS thread to insulate detection and reporting from hangs on the fore
    // or background.
    thread::Builder::new()
        .name("HangDetection".to_string())
        .spawn(move || {
            // allow "bad" tasks during startup. Not because we should but since here
            // they are not observed by the user and to lower on clutter from the reporter
            thread::sleep(Duration::from_millis(200));
            let mut skipped_previous = false;
            loop {
                let slept_from = Instant::now();
                thread::sleep(monitor_interval);
                // TODO(yara) the telemetry should not include still running tasks while the
                // reports being logged should.
                let task_stats = profiler::take_all_stats(TasksIncluded::CompletedAndRunning);
                let action_stats = profiler::take_action_stats();
                // Taken all the same, so that what ran across the gap goes with
                // this sample instead of into the next one.
                let skip = skips_sample(slept_from.elapsed(), monitor_interval, skipped_previous);
                skipped_previous = skip;
                if skip {
                    continue;
                }

                {
                    let mut telemetry = telemetry.lock();
                    telemetry.update(&task_stats, &action_stats);
                    telemetry.send_periodically();
                }

                let should_write_trace = log.check_and_report(&task_stats, &action_stats);
                if should_write_trace {
                    if let Some(path) = task_traces::save_any(foreground_thread) {
                        log::info!("Task trace has been saved to: {}", path.display());
                    }
                }
            }
        })
        .expect("App can always spawn threads");
}

/// Whether to drop a sample because the monitor's own sleep ran far past its
/// interval: the machine was suspended (the clock counts that time) or this
/// thread was starved, so what ran across the gap is timed with the gap in it.
/// Never two in a row: taking the stats reset them, so the next sample no
/// longer holds the gap, and a monitor that is always late must still report.
fn skips_sample(slept: Duration, interval: Duration, skipped_previous: bool) -> bool {
    !skipped_previous && slept > interval * 2
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::skips_sample;

    #[test]
    fn a_sample_taken_after_the_machine_slept_is_skipped() {
        let interval = Duration::from_secs(1);
        assert!(!skips_sample(Duration::from_millis(1050), interval, false));
        assert!(!skips_sample(interval * 2, interval, false));
        assert!(skips_sample(
            Duration::from_secs(3 * 60 * 60),
            interval,
            false
        ));
    }

    #[test]
    fn a_monitor_that_is_always_late_still_reports() {
        let interval = Duration::from_secs(1);
        let late = interval * 3;
        let first = skips_sample(late, interval, false);
        let second = skips_sample(late, interval, first);
        let third = skips_sample(late, interval, second);
        assert_eq!([first, second, third], [true, false, true]);
    }
}
