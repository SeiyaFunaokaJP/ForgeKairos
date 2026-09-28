use parking_lot::Mutex;
use std::{
    fs::File,
    io::{BufRead, BufReader},
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

/// Zed only adds a native watch while the user's total inotify usage stays within this fraction
/// of the limit. The rest is left to other programs: build tools such as bitbake grab thousands
/// of watches at once and fail on the first ENOSPC instead of retrying.
pub(super) const NATIVE_SHARE_DIVISOR: usize = 4;
/// Once fewer than this fraction of the watches are free, other programs are about to run out,
/// so Zed hands all of its native watches over to polling.
pub(super) const YIELD_FREE_DIVISOR: usize = 8;
/// Several Zed processes can each stay within the share by their own estimate and still exceed
/// it together. Trimming back only starts this far above the share, so a process doesn't keep
/// flipping paths around the boundary.
pub(super) const OVERSHOOT_DIVISOR: usize = 16;
pub(super) const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(5);
pub(super) const PROMOTION_INTERVAL: Duration = Duration::from_secs(30);
/// Several Zed processes of the same user can promote from the same measurement at the same
/// moment, so each one takes a bounded step at a time and measures again along the way.
pub(super) const MAX_PROMOTIONS_PER_ROUND: usize = 512;
pub(super) const PROMOTIONS_PER_MEASUREMENT: usize = 64;
/// Scanning `/proc` costs milliseconds per run, which isn't worth paying every few seconds
/// while almost all watches are free.
const ROOMY_MEASUREMENT_INTERVAL: Duration = Duration::from_secs(60);
const MEASUREMENT_MAX_DRIFT: usize = 256;
/// A limit learned from ENOSPC can go stale without `/proc` showing it (another program exited,
/// or the container's limit was raised), so it is dropped after a while to try once more.
const LEARNED_LIMIT_LIFETIME: Duration = Duration::from_secs(30 * 60);
/// When the kernel refuses at nearly the reported limit, the report was right and there is
/// nothing to learn.
const ACCURATE_REPORT_PERCENT: usize = 90;
const INOTIFY_LINK_TARGET: &str = "anon_inode:inotify";
const LIMIT_OVERRIDE_VARIABLE: &str = "ZED_INOTIFY_MAX_USER_WATCHES";

pub(super) fn native_watch_allowed(estimated_usage: usize, limit: usize) -> bool {
    estimated_usage.saturating_add(1) <= limit / NATIVE_SHARE_DIVISOR
}

pub(super) fn should_yield_native_watches(usage: usize, limit: usize) -> bool {
    limit.saturating_sub(usage) < limit / YIELD_FREE_DIVISOR
}

pub(super) fn promotion_capacity(usage: usize, limit: usize) -> usize {
    (limit / NATIVE_SHARE_DIVISOR)
        .saturating_sub(usage)
        .min(MAX_PROMOTIONS_PER_ROUND)
}

pub(super) fn native_watches_over_share(
    usage: usize,
    limit: usize,
    native_watches: usize,
) -> usize {
    let share = limit / NATIVE_SHARE_DIVISOR;
    if usage > share + limit / OVERSHOOT_DIVISOR {
        (usage - share).min(native_watches)
    } else {
        0
    }
}

pub(super) fn native_watches_over_learned_limit(
    native_watches: usize,
    learned_limit: usize,
) -> usize {
    native_watches.saturating_sub(learned_limit / NATIVE_SHARE_DIVISOR)
}

pub(super) struct InotifyUsage {
    /// Watches of every process charged to the same user, this one included.
    pub(super) user_watches: usize,
    /// Watches this process holds, as `/proc` lists them.
    pub(super) own_watches: usize,
}

/// Where the budget reads the user's inotify limit and usage from. Tests substitute their own.
pub(super) trait WatchBudgetSource: Send + Sync {
    fn limit(&self) -> Option<usize>;
    fn usage(&self) -> Option<InotifyUsage>;
}

pub(super) struct ProcfsBudgetSource;

impl WatchBudgetSource for ProcfsBudgetSource {
    fn limit(&self) -> Option<usize> {
        read_inotify_watch_limit()
    }

    fn usage(&self) -> Option<InotifyUsage> {
        measure_user_inotify_watches(Path::new("/proc"), std::process::id())
    }
}

pub(super) fn read_inotify_watch_limit() -> Option<usize> {
    if let Some(limit) = std::env::var(LIMIT_OVERRIDE_VARIABLE)
        .ok()
        .and_then(|value| value.trim().parse().ok())
    {
        return Some(limit);
    }
    // The second file is the limit of the current user namespace, which can be lower than the
    // global one.
    combine_limits(
        read_limit_file(Path::new("/proc/sys/fs/inotify/max_user_watches")),
        read_limit_file(Path::new("/proc/sys/user/max_inotify_watches")),
    )
}

fn combine_limits(global_limit: Option<usize>, namespace_limit: Option<usize>) -> Option<usize> {
    match (global_limit, namespace_limit) {
        (Some(global_limit), Some(namespace_limit)) => Some(global_limit.min(namespace_limit)),
        (limit, None) | (None, limit) => limit,
    }
}

fn read_limit_file(path: &Path) -> Option<usize> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Sums the inotify watches of every process running as our effective user, since that is the
/// account the kernel charges watches to. Returns `None` when there is no readable `/proc`.
pub(super) fn measure_user_inotify_watches(
    proc_root: &Path,
    own_process_id: u32,
) -> Option<InotifyUsage> {
    let own_process_dir = proc_root.join("self");
    let own_uid = effective_uid(&own_process_dir)?;
    let own_owner = directory_owner(&own_process_dir);
    let own_process_name = own_process_id.to_string();
    let processes = std::fs::read_dir(proc_root).ok()?;
    let mut usage = InotifyUsage {
        user_watches: 0,
        own_watches: 0,
    };
    let mut saw_own_process = false;
    for process in processes.flatten() {
        let file_name = process.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        if name.is_empty() || !name.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        let process_dir = process.path();
        // The owner of `/proc/<pid>` is a cheap first filter that skips reading `status` for
        // the processes of other users.
        if own_owner.is_some() && directory_owner(&process_dir) != own_owner {
            continue;
        }
        // Processes exit while we scan and those of other users are unreadable; either way
        // they don't count against our limit.
        if effective_uid(&process_dir) != Some(own_uid) {
            continue;
        }
        let watches = count_process_inotify_watches(&process_dir);
        usage.user_watches += watches;
        if name == own_process_name {
            usage.own_watches = watches;
            saw_own_process = true;
        }
    }
    if !saw_own_process {
        usage.own_watches = count_process_inotify_watches(&own_process_dir);
    }
    Some(usage)
}

fn effective_uid(process_dir: &Path) -> Option<u32> {
    parse_effective_uid(&std::fs::read_to_string(process_dir.join("status")).ok()?)
}

#[cfg(unix)]
fn directory_owner(path: &Path) -> Option<u32> {
    use std::os::unix::fs::MetadataExt as _;
    std::fs::metadata(path).ok().map(|metadata| metadata.uid())
}

#[cfg(not(unix))]
fn directory_owner(_path: &Path) -> Option<u32> {
    None
}

pub(super) fn count_process_inotify_watches(process_dir: &Path) -> usize {
    let Ok(descriptors) = std::fs::read_dir(process_dir.join("fd")) else {
        return 0;
    };
    let mut total = 0;
    for descriptor in descriptors.flatten() {
        let Ok(target) = std::fs::read_link(descriptor.path()) else {
            continue;
        };
        if target.as_path() != Path::new(INOTIFY_LINK_TARGET) {
            continue;
        }
        let fdinfo_path = process_dir.join("fdinfo").join(descriptor.file_name());
        if let Ok(fdinfo) = File::open(fdinfo_path) {
            total += count_inotify_watch_lines(BufReader::new(fdinfo));
        }
    }
    total
}

/// Counts the watch lines of an inotify fdinfo file, which has one line per watch and can be
/// large, without holding all of it in memory.
pub(super) fn count_inotify_watch_lines(fdinfo: impl BufRead) -> usize {
    fdinfo
        .split(b'\n')
        .map_while(Result::ok)
        .filter(|line| line.starts_with(b"inotify wd:"))
        .count()
}

pub(super) fn parse_effective_uid(status: &str) -> Option<u32> {
    let ids = status.lines().find_map(|line| line.strip_prefix("Uid:"))?;
    ids.split_whitespace().nth(1)?.parse().ok()
}

#[derive(Clone, Copy)]
pub(super) struct BudgetReading {
    pub(super) limit: Option<usize>,
    pub(super) usage: usize,
}

/// Tracks this process's native watches against the user's inotify limit.
pub(super) struct WatchBudget {
    enabled: bool,
    source: Mutex<Arc<dyn WatchBudgetSource>>,
    // A leaf lock: nothing else is locked while it is held.
    state: Mutex<BudgetState>,
}

#[derive(Default)]
struct BudgetState {
    native_watches: usize,
    measurement: Option<Measurement>,
    initial_measurement_taken: bool,
    reported_limit: Option<usize>,
    learned: Option<LearnedLimit>,
    denial_logged: bool,
    yield_logged: bool,
}

#[derive(Clone, Copy)]
struct Measurement {
    usage: usize,
    native_watches: usize,
    taken_at: Instant,
}

#[derive(Clone, Copy)]
struct LearnedLimit {
    limit: usize,
    reported_limit: Option<usize>,
    learned_at: Instant,
}

impl BudgetState {
    fn effective_limit(&self) -> Option<usize> {
        let learned_limit = self.learned.map(|learned| learned.limit);
        match (self.reported_limit, learned_limit) {
            (Some(reported_limit), Some(learned_limit)) => Some(reported_limit.min(learned_limit)),
            (limit, None) | (None, limit) => limit,
        }
    }

    fn estimated_usage(&self) -> usize {
        match self.measurement {
            Some(measurement) => {
                (measurement.usage + self.native_watches).saturating_sub(measurement.native_watches)
            }
            None => self.native_watches,
        }
    }

    fn reading(&self) -> BudgetReading {
        BudgetReading {
            limit: self.effective_limit(),
            usage: self.estimated_usage(),
        }
    }

    fn measurement_due(&self, now: Instant) -> bool {
        let Some(measurement) = self.measurement else {
            return true;
        };
        let roomy = self
            .effective_limit()
            .is_some_and(|limit| measurement.usage < limit / YIELD_FREE_DIVISOR);
        let interval = if roomy {
            ROOMY_MEASUREMENT_INTERVAL
        } else {
            MAINTENANCE_INTERVAL
        };
        now.saturating_duration_since(measurement.taken_at) >= interval
            || self.native_watches.abs_diff(measurement.native_watches) >= MEASUREMENT_MAX_DRIFT
    }

    fn drop_stale_learned_limit(&mut self, now: Instant, measured_usage: Option<usize>) {
        let Some(learned) = self.learned else {
            return;
        };
        let reason = if learned.reported_limit != self.reported_limit {
            "the reported limit changed"
        } else if now.saturating_duration_since(learned.learned_at) >= LEARNED_LIMIT_LIFETIME {
            "it expired"
        } else if measured_usage.is_some_and(|usage| usage > learned.limit) {
            "more watches are in use than it allows"
        } else {
            return;
        };
        log::info!(
            "forgetting the inotify limit of {} learned from ENOSPC because {reason}",
            learned.limit
        );
        self.learned = None;
    }
}

impl WatchBudget {
    pub(super) fn new(enabled: bool, source: Arc<dyn WatchBudgetSource>) -> Self {
        Self {
            enabled,
            source: Mutex::new(source),
            state: Mutex::new(BudgetState::default()),
        }
    }

    pub(super) fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub(super) fn native_watches(&self) -> usize {
        self.state.lock().native_watches
    }

    pub(super) fn learned_limit(&self) -> Option<usize> {
        self.state.lock().learned.map(|learned| learned.limit)
    }

    pub(super) fn reading(&self) -> BudgetReading {
        self.state.lock().reading()
    }

    pub(super) fn measurement_due(&self, now: Instant) -> bool {
        self.enabled && self.state.lock().measurement_due(now)
    }

    /// Counts a native watch that is about to be added, or returns false if adding it would
    /// take more than Zed's share of the user's inotify watches.
    pub(super) fn try_reserve_native(&self) -> bool {
        if !self.enabled {
            return true;
        }
        let mut state = self.state.lock();
        if let Some(limit) = state.effective_limit() {
            let usage = state.estimated_usage();
            if !native_watch_allowed(usage, limit) {
                let first_denial = !state.denial_logged;
                state.denial_logged = true;
                drop(state);
                if first_denial {
                    log::warn!(
                        "about {usage} of this user's {limit} inotify watches are in use; watching further paths by polling to leave the rest to other programs"
                    );
                }
                return false;
            }
        }
        state.native_watches += 1;
        true
    }

    pub(super) fn release_native(&self) {
        if self.enabled {
            let mut state = self.state.lock();
            state.native_watches = state.native_watches.saturating_sub(1);
        }
    }

    /// Takes the first measurement, so the very first registrations already account for the
    /// watches of other programs. Later measurements are left to the maintenance thread, which
    /// keeps registering paths cheap.
    pub(super) fn ensure_initial_measurement(&self) {
        if !self.enabled {
            return;
        }
        {
            let mut state = self.state.lock();
            if std::mem::replace(&mut state.initial_measurement_taken, true) {
                return;
            }
        }
        self.measure();
    }

    pub(super) fn measure(&self) -> BudgetReading {
        self.measure_at(Instant::now())
    }

    fn measure_at(&self, now: Instant) -> BudgetReading {
        let source = self.source.lock().clone();
        let native_watches = self.state.lock().native_watches;
        let limit = source.limit();
        let has_learned_limit = self.state.lock().learned.is_some();
        // Without any limit there is nothing to hold the usage against.
        let usage = if limit.is_some() || has_learned_limit {
            source.usage()
        } else {
            None
        };

        let mut state = self.state.lock();
        state.reported_limit = limit;
        let usage_was_read = usage.is_some();
        // Our own watches must show up in `/proc/self`. If they don't, the scan can't see the
        // processes charged to our user either, so only our own count can be trusted.
        let measured_usage = usage
            .filter(|usage| native_watches == 0 || usage.own_watches > 0)
            .map(|usage| usage.user_watches.max(native_watches));
        if let Some(usage) = measured_usage {
            state.measurement = Some(Measurement {
                usage,
                native_watches,
                taken_at: now,
            });
        } else if usage_was_read {
            state.measurement = None;
        }
        state.drop_stale_learned_limit(now, measured_usage);
        state.reading()
    }

    /// Called after a native watch failed with ENOSPC. Inside containers and user namespaces
    /// `/proc` can report a higher limit than the one actually enforced, so the usage at which
    /// the kernel refused becomes the limit. Returns whether a limit was learned.
    pub(super) fn learn_limit_from_watch_limit_error(&self, path: &Path) -> bool {
        self.learn_limit_at(path, Instant::now())
    }

    fn learn_limit_at(&self, path: &Path, now: Instant) -> bool {
        let reading = self.measure_at(now);
        let mut state = self.state.lock();
        let reported_limit = state.reported_limit;
        let learned_limit = reading.usage;
        if reported_limit.is_some_and(|reported_limit| {
            learned_limit.saturating_mul(100)
                >= reported_limit.saturating_mul(ACCURATE_REPORT_PERCENT)
        }) {
            drop(state);
            log::info!(
                "inotify refused to watch {path:?} at a usage of {learned_limit} watches, close to the reported limit of {reported_limit:?}"
            );
            return false;
        }
        state.learned = Some(LearnedLimit {
            limit: learned_limit,
            reported_limit,
            learned_at: now,
        });
        drop(state);
        log::warn!(
            "inotify refused to watch {path:?} at a usage of {learned_limit} watches although the reported limit is {reported_limit:?}; using {learned_limit} as the limit"
        );
        true
    }

    /// Returns true the first time Zed yields its native watches within an episode.
    pub(super) fn begin_yielding(&self) -> bool {
        let mut state = self.state.lock();
        !std::mem::replace(&mut state.yield_logged, true)
    }

    pub(super) fn end_episode(&self) {
        let mut state = self.state.lock();
        state.yield_logged = false;
        state.denial_logged = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FixedSource {
        limit: Mutex<Option<usize>>,
        user_watches: Mutex<Option<usize>>,
        own_watches: Mutex<usize>,
        usage_reads: Mutex<usize>,
    }

    impl WatchBudgetSource for FixedSource {
        fn limit(&self) -> Option<usize> {
            *self.limit.lock()
        }

        fn usage(&self) -> Option<InotifyUsage> {
            *self.usage_reads.lock() += 1;
            let user_watches = (*self.user_watches.lock())?;
            Some(InotifyUsage {
                user_watches,
                own_watches: *self.own_watches.lock(),
            })
        }
    }

    fn fixed_source(limit: Option<usize>, user_watches: Option<usize>) -> Arc<FixedSource> {
        Arc::new(FixedSource {
            limit: Mutex::new(limit),
            user_watches: Mutex::new(user_watches),
            own_watches: Mutex::new(0),
            usage_reads: Mutex::new(0),
        })
    }

    #[test]
    fn native_watches_are_allowed_up_to_a_quarter_of_the_limit() {
        assert!(native_watch_allowed(0, 8192));
        assert!(native_watch_allowed(2047, 8192));
        assert!(!native_watch_allowed(2048, 8192));
        assert!(!native_watch_allowed(6000, 8192));
        assert!(!native_watch_allowed(0, 3));
    }

    #[test]
    fn native_watches_are_yielded_when_less_than_an_eighth_is_free() {
        assert!(!should_yield_native_watches(7168, 8192));
        assert!(should_yield_native_watches(7169, 8192));
        assert!(should_yield_native_watches(9000, 8192));
        assert!(!should_yield_native_watches(0, 0));
    }

    #[test]
    fn promotions_fill_the_share_in_bounded_steps() {
        assert_eq!(promotion_capacity(0, 8192), 512);
        assert_eq!(promotion_capacity(2000, 8192), 48);
        assert_eq!(promotion_capacity(2047, 8192), 1);
        assert_eq!(promotion_capacity(2048, 8192), 0);
        assert_eq!(promotion_capacity(5000, 8192), 0);
    }

    #[test]
    fn native_watches_are_trimmed_only_well_above_the_share() {
        // The share of 8192 is 2048, and trimming starts above 2048 + 512.
        assert_eq!(native_watches_over_share(2560, 8192, 1000), 0);
        assert_eq!(native_watches_over_share(2561, 8192, 1000), 513);
        assert_eq!(native_watches_over_share(6000, 8192, 1000), 1000);
    }

    #[test]
    fn learned_limit_keeps_a_quarter_of_it() {
        assert_eq!(native_watches_over_learned_limit(400, 400), 300);
        assert_eq!(native_watches_over_learned_limit(50, 400), 0);
    }

    #[test]
    fn limits_combine_to_the_lowest_known_value() {
        assert_eq!(combine_limits(Some(8192), Some(524288)), Some(8192));
        assert_eq!(combine_limits(Some(524288), Some(100)), Some(100));
        assert_eq!(combine_limits(None, Some(100)), Some(100));
        assert_eq!(combine_limits(Some(8192), None), Some(8192));
        assert_eq!(combine_limits(None, None), None);
    }

    #[test]
    fn fdinfo_watch_lines_are_counted() {
        let fdinfo = "pos:\t0\nflags:\t02004000\nmnt_id:\t15\nino:\t1057\n\
            inotify wd:3 ino:2a0b sdev:800020 mask:fc6 ignored_mask:0 fhandle-bytes:8 fhandle-type:1 f_handle:0b2a000000000000\n\
            inotify wd:2 ino:2a0a sdev:800020 mask:fc6 ignored_mask:0 fhandle-bytes:8 fhandle-type:1 f_handle:0a2a000000000000\n\
            inotify wd:1 ino:2a09 sdev:800020 mask:fc6 ignored_mask:0 fhandle-bytes:8 fhandle-type:1 f_handle:092a000000000000";
        assert_eq!(count_inotify_watch_lines(fdinfo.as_bytes()), 3);
        assert_eq!(
            count_inotify_watch_lines("pos:\t0\nflags:\t02\n".as_bytes()),
            0
        );
    }

    #[test]
    fn effective_uid_is_the_second_uid_field() {
        let status = "Name:\tbash\nUmask:\t0022\nState:\tS (sleeping)\nUid:\t1000\t1001\t1002\t1003\nGid:\t100\t100\t100\t100\n";
        assert_eq!(parse_effective_uid(status), Some(1001));
        assert_eq!(parse_effective_uid("Name:\tbash\n"), None);
    }

    #[cfg(unix)]
    #[test]
    fn usage_counts_inotify_watches_of_processes_with_the_same_effective_uid() {
        use std::os::unix::fs::symlink;

        fn process(root: &Path, pid: &str, uid: u32, descriptors: &[(&str, &str, &str)]) {
            let process_dir = root.join(pid);
            std::fs::create_dir_all(process_dir.join("fd")).expect("create fd dir");
            std::fs::create_dir_all(process_dir.join("fdinfo")).expect("create fdinfo dir");
            std::fs::write(
                process_dir.join("status"),
                format!("Name:\ttest\nUid:\t{uid}\t{uid}\t{uid}\t{uid}\n"),
            )
            .expect("write status");
            for (descriptor, target, fdinfo) in descriptors {
                symlink(target, process_dir.join("fd").join(descriptor)).expect("create fd link");
                std::fs::write(process_dir.join("fdinfo").join(descriptor), fdinfo)
                    .expect("write fdinfo");
            }
        }

        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let root = temp_dir.path();
        let two_watches = "pos:\t0\ninotify wd:1 ino:1\ninotify wd:2 ino:2\n";
        let one_watch = "pos:\t0\ninotify wd:1 ino:1\n";
        process(root, "self", 1000, &[]);
        process(
            root,
            "10",
            1000,
            &[
                ("3", INOTIFY_LINK_TARGET, two_watches),
                ("4", "/dev/null", two_watches),
            ],
        );
        process(root, "11", 1000, &[("5", INOTIFY_LINK_TARGET, one_watch)]);
        process(root, "12", 0, &[("3", INOTIFY_LINK_TARGET, two_watches)]);
        std::fs::create_dir_all(root.join("sys")).expect("create non-process dir");

        let usage = measure_user_inotify_watches(root, 11).expect("measure fake /proc");
        assert_eq!(usage.user_watches, 3);
        assert_eq!(usage.own_watches, 1);
        assert!(measure_user_inotify_watches(&root.join("missing"), 11).is_none());
    }

    #[test]
    fn reservations_follow_the_estimate_between_measurements() {
        let source = fixed_source(Some(40), Some(0));
        let budget = WatchBudget::new(true, source.clone());
        budget.ensure_initial_measurement();
        for _ in 0..10 {
            assert!(budget.try_reserve_native());
        }
        assert!(!budget.try_reserve_native());
        assert_eq!(budget.native_watches(), 10);

        budget.release_native();
        assert!(budget.try_reserve_native());

        // Other programs started using watches; the next measurement leaves no room.
        *source.user_watches.lock() = Some(30);
        *source.own_watches.lock() = 10;
        budget.measure();
        budget.release_native();
        assert!(!budget.try_reserve_native());
    }

    #[test]
    fn unknown_limit_allows_native_watches_without_scanning_proc() {
        let source = fixed_source(None, Some(1_000_000));
        let budget = WatchBudget::new(true, source.clone());
        budget.ensure_initial_measurement();
        budget.measure();
        assert!(budget.try_reserve_native());
        assert_eq!(budget.native_watches(), 1);
        assert_eq!(*source.usage_reads.lock(), 0);
    }

    #[test]
    fn disabled_budget_neither_counts_nor_limits() {
        let budget = WatchBudget::new(false, fixed_source(Some(4), Some(100)));
        budget.ensure_initial_measurement();
        assert!(budget.try_reserve_native());
        assert!(budget.try_reserve_native());
        assert_eq!(budget.native_watches(), 0);
    }

    #[test]
    fn usage_is_never_below_our_own_native_watches() {
        let source = fixed_source(Some(1000), Some(2));
        *source.own_watches.lock() = 2;
        let budget = WatchBudget::new(true, source);
        for _ in 0..5 {
            assert!(budget.try_reserve_native());
        }
        assert_eq!(budget.measure().usage, 5);
    }

    #[test]
    fn a_scan_that_misses_our_own_watches_is_not_trusted() {
        // Five native watches of our own but none visible in `/proc/self`, as when `/proc` is
        // mounted with `hidepid` or belongs to another namespace.
        let source = fixed_source(Some(40), Some(0));
        let budget = WatchBudget::new(true, source);
        for _ in 0..5 {
            assert!(budget.try_reserve_native());
        }
        assert_eq!(budget.measure().usage, 5);
        for _ in 0..5 {
            assert!(budget.try_reserve_native());
        }
        assert!(!budget.try_reserve_native());
    }

    #[test]
    fn measurements_are_rarer_while_most_watches_are_free() {
        let source = fixed_source(Some(8192), Some(100));
        *source.own_watches.lock() = 1;
        let budget = WatchBudget::new(true, source.clone());
        let start = Instant::now();
        budget.measure_at(start);
        assert!(!budget.measurement_due(start + Duration::from_secs(10)));
        assert!(budget.measurement_due(start + Duration::from_secs(61)));

        *source.user_watches.lock() = Some(4000);
        budget.measure_at(start);
        assert!(budget.measurement_due(start + Duration::from_secs(5)));
    }

    #[test]
    fn learned_limit_is_dropped_when_usage_exceeds_it() {
        let source = fixed_source(Some(1_000_000), Some(400));
        *source.own_watches.lock() = 400;
        let budget = WatchBudget::new(true, source.clone());
        assert!(budget.learn_limit_from_watch_limit_error(Path::new("/repo")));
        assert_eq!(budget.learned_limit(), Some(400));
        assert_eq!(budget.measure().limit, Some(400));

        *source.user_watches.lock() = Some(401);
        assert_eq!(budget.measure().limit, Some(1_000_000));
        assert_eq!(budget.learned_limit(), None);
    }

    #[test]
    fn learned_limit_is_dropped_when_the_reported_limit_changes() {
        let source = fixed_source(Some(1_000_000), Some(400));
        *source.own_watches.lock() = 400;
        let budget = WatchBudget::new(true, source.clone());
        assert!(budget.learn_limit_from_watch_limit_error(Path::new("/repo")));

        *source.limit.lock() = Some(2_000_000);
        budget.measure();
        assert_eq!(budget.learned_limit(), None);
    }

    #[test]
    fn learned_limit_expires() {
        let source = fixed_source(Some(1_000_000), Some(400));
        *source.own_watches.lock() = 400;
        let budget = WatchBudget::new(true, source);
        let start = Instant::now();
        assert!(budget.learn_limit_at(Path::new("/repo"), start));

        budget.measure_at(start + Duration::from_secs(29 * 60));
        assert_eq!(budget.learned_limit(), Some(400));
        budget.measure_at(start + Duration::from_secs(31 * 60));
        assert_eq!(budget.learned_limit(), None);
    }

    #[test]
    fn hitting_nearly_the_reported_limit_teaches_nothing() {
        let source = fixed_source(Some(8192), Some(7900));
        *source.own_watches.lock() = 100;
        let budget = WatchBudget::new(true, source);
        assert!(!budget.learn_limit_from_watch_limit_error(Path::new("/repo")));
        assert_eq!(budget.learned_limit(), None);
    }
}
