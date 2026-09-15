//! ADR-F49 decision 18 (C): follows what an external agent leaves running in
//! its sandbox, and keeps it a writer of the worktrees it can write for as
//! long as it does.
//!
//! ADR-F50 pauses a worktree's own code after an agent writes, until the user
//! has looked at the change and resumed. An agent's *turn* is a writer for the
//! whole answer, but an answer can end with a process still running inside the
//! sandbox — a development server, an MCP server it started halfway through —
//! and that process can write the worktree afterwards. So:
//!
//! * when an answer begins, what is already running is its **baseline**;
//! * when it ends, anything running that is not in the baseline holds the
//!   answer's writer registration until it is gone, so the resume stays
//!   refused and the dialog lists them with a way to stop them;
//! * between answers the same check runs, more slowly, so something that
//!   appears then makes the agent a writer again.
//!
//! The list comes from outside the sandbox (see `sandbox::sandbox_processes`),
//! never from the agent: everything a sandboxed command can write, it can also
//! lie on. Where there is no way to look — macOS's Seatbelt, a host that
//! cannot answer — the agent stays a writer for its whole session instead
//! (case B): not being able to look is not the same as having seen nothing.
//!
//! One of these belongs to the agent's **connection**, not to a thread: the
//! sandbox outlives any one conversation in it, and closing a thread must not
//! be a way to stop following what the agent left running.

use std::{cell::RefCell, collections::HashSet, path::PathBuf, rc::Rc, time::Duration};

use anyhow::Result;
use gpui::{App, AppContext as _, Context, Entity, SharedString, Task, TaskExt as _};
use project::{
    AgentWriteScope, LeftoverSandboxProcess, LeftoverSandboxProcessReports,
    LeftoverSandboxProcessesHandle, Project, SandboxedWriterGuard,
};
use sandbox::{SandboxProcess, SandboxProcessKey};
use util::ResultExt as _;

use crate::SandboxProcessTracking;

/// How often the sandbox is looked at while something an answer left behind is
/// still running: often enough that a resume becomes possible soon after the
/// last one ends.
const WATCHING_INTERVAL: Duration = Duration::from_secs(2);

/// And how often between answers, when the expectation is that nothing is
/// there. One `/proc` walk, or one small request to the host, per agent
/// connection at this rate is what this is allowed to cost.
const IDLE_INTERVAL: Duration = Duration::from_secs(10);

/// How often the sandbox is looked at while it is still being built. This is
/// made in the same breath as the agent is spawned, and the host has not
/// finished standing the sandbox up — let alone started the agent in it — by
/// the time the first look goes out.
const STARTING_INTERVAL: Duration = Duration::from_millis(500);

/// And how many times. The agent's own process runs in its sandbox, so an
/// empty sandbox is one that is not up yet rather than one that runs nothing;
/// taking it for a baseline would make the agent's own process the first
/// answer's leftover and leave the worktree paused for good. After these it is
/// taken at its word anyway, which is the safe reading of a sandbox that never
/// came up: whatever appears next counts as a leftover.
const STARTING_LOOKS: usize = 60;

/// And how many an answer's own baseline may spend, which is far fewer: the
/// prompt waits for it.
const TURN_STARTING_LOOKS: usize = 2;

/// ADR-F49 decision 18 (C): the sandbox an agent runs in, as the trusted side
/// outside it can read and act on it. Kept separate from the connection so
/// that the monitor the connection owns does not hold the connection.
pub trait SandboxProcessSource {
    /// How far this sandbox can be followed.
    fn tracking(&self) -> SandboxProcessTracking;

    /// Everything running in it now. An empty list also answers for a sandbox
    /// that is gone: its PID namespace died with it and the kernel stopped
    /// everything in it.
    fn list(&self, cx: &App) -> Task<Result<Vec<SandboxProcess>>>;

    /// Stops exactly these. Never the agent itself, which the user stops from
    /// its own panel.
    fn stop(&self, processes: Vec<SandboxProcessKey>, cx: &App) -> Task<Result<usize>>;

    /// Where the agent's process can write — its worktrees and its
    /// configuration folder — which is what a writer registration covers.
    /// `None` when it can write anywhere on its host.
    fn write_scope(&self) -> Option<Vec<PathBuf>>;

    /// The agent, for the resume dialog to say whose leftovers these are.
    fn agent(&self) -> SharedString;
}

/// ADR-F49 decision 18 (C): the per-connection follower this module describes.
/// Dropping it releases whatever writer it held, which is right: the sandbox
/// goes when the connection does, and the kernel stops everything in it.
pub struct SandboxProcessMonitor {
    source: Rc<dyn SandboxProcessSource>,
    project: Entity<Project>,
    /// What was already running when the current answer began, and so is not
    /// this answer's doing.
    baseline: HashSet<SandboxProcessKey>,
    /// Whether a look has landed that a baseline could be taken from. Until
    /// one has there is nothing to compare against, so nothing is a leftover.
    baseline_established: bool,
    /// How many more times an empty sandbox counts as one that is not up yet
    /// rather than as a baseline (see [`STARTING_LOOKS`]).
    starting_looks_left: usize,
    /// What an answer left running that has not ended yet, and what the
    /// dialog's stop button signals. Shared with the report's stop closure so
    /// that pressing it stops what the dialog is showing. Never moves into a
    /// later baseline: a process that outlives two answers is still the first
    /// answer's leftover.
    followed: Rc<RefCell<Vec<SandboxProcessKey>>>,
    /// Held while `followed` is not empty: the answer's own registration for
    /// as long as it ran over, or a fresh one for something that appeared
    /// between answers.
    writer: Option<SandboxedWriterGuard>,
    /// Case B: the sandbox cannot be looked into, so this is held for as long
    /// as the connection lasts.
    session_writer: Option<SandboxedWriterGuard>,
    report: Option<LeftoverSandboxProcessesHandle>,
    poll: Task<()>,
}

impl SandboxProcessMonitor {
    /// `None` for an agent whose turns are all there is to follow: one that
    /// changes files only through Zed's own tools, or one the user chose to
    /// start outside the sandbox.
    pub fn new(
        source: Rc<dyn SandboxProcessSource>,
        project: &Entity<Project>,
        cx: &mut App,
    ) -> Option<Entity<Self>> {
        let tracking = source.tracking();
        if tracking == SandboxProcessTracking::NotSandboxed {
            return None;
        }
        Some(cx.new(|cx| {
            let mut monitor = Self {
                source,
                project: project.clone(),
                baseline: HashSet::default(),
                baseline_established: false,
                starting_looks_left: STARTING_LOOKS,
                followed: Rc::new(RefCell::new(Vec::new())),
                writer: None,
                session_writer: None,
                report: None,
                poll: Task::ready(()),
            };
            match tracking {
                SandboxProcessTracking::Untrackable => monitor.follow_the_whole_session(cx),
                // The agent's own process and whatever its start-up brought
                // with it are the first baseline: they are not something an
                // answer left behind.
                SandboxProcessTracking::Trackable => {
                    monitor.poll = monitor.take_baseline(IDLE_INTERVAL, STARTING_LOOKS, cx)
                }
                SandboxProcessTracking::NotSandboxed => {}
            }
            monitor
        }))
    }

    /// The answer is about to be sent: what runs now is its baseline. The
    /// caller awaits this before the prompt reaches the agent, so that nothing
    /// the answer starts can end up in its own baseline.
    pub fn begin_turn(&mut self, cx: &mut Context<Self>) -> Task<()> {
        if self.session_writer.is_some() {
            return Task::ready(());
        }
        // The prompt waits for this, and an agent that is answering is an
        // agent whose process is up, so an empty sandbox here is a route that
        // cannot see it rather than one that is still starting: worth a look
        // or two, never the whole wait.
        self.take_baseline(IDLE_INTERVAL, TURN_STARTING_LOOKS, cx)
    }

    /// Reads the sandbox until it has something a baseline can be taken from —
    /// at most `looks` times over an empty one — keeps that as the baseline,
    /// then goes back to looking every `next`.
    fn take_baseline(
        &mut self,
        next: Duration,
        mut looks: usize,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        let mut listing = self.source.list(cx);
        cx.spawn(async move |this, cx| {
            loop {
                let processes = match listing.await {
                    Ok(processes) => processes,
                    Err(error) => {
                        this.update(cx, |this, cx| this.cannot_look(error, cx))
                            .log_err();
                        return;
                    }
                };
                let Ok(still_starting) = this.update(cx, |this, _| {
                    looks > 0 && this.spend_a_starting_look(&processes)
                }) else {
                    return;
                };
                if !still_starting {
                    this.update(cx, |this, cx| {
                        this.adopt_baseline(&processes);
                        this.poll_after(next, cx);
                    })
                    .log_err();
                    return;
                }
                looks -= 1;
                cx.background_executor().timer(STARTING_INTERVAL).await;
                let Ok(next_listing) = this.update(cx, |this, cx| this.source.list(cx)) else {
                    return;
                };
                listing = next_listing;
            }
        })
    }

    /// Whether an empty sandbox is still to be read as one that is not up yet,
    /// and spends one of the looks that says so.
    fn spend_a_starting_look(&mut self, processes: &[SandboxProcess]) -> bool {
        if !processes.is_empty() || self.baseline_established || self.starting_looks_left == 0 {
            return false;
        }
        self.starting_looks_left -= 1;
        true
    }

    /// What runs now becomes the answer's baseline. After the first one the
    /// baseline can only shrink: a process an earlier answer started that the
    /// last look has not taken in yet must never be absorbed into a later
    /// baseline, or stopping an answer and asking again would be all it takes
    /// to leave a leftover unaccounted for.
    fn adopt_baseline(&mut self, processes: &[SandboxProcess]) {
        let previous = std::mem::take(&mut self.baseline);
        let established = self.baseline_established;
        {
            let followed = self.followed.borrow();
            self.baseline = processes
                .iter()
                .map(SandboxProcess::key)
                .filter(|key| !followed.contains(key) && (!established || previous.contains(key)))
                .collect();
        }
        self.baseline_established = true;
    }

    /// The answer is over. `writer` is its registration, kept alive here for
    /// as long as anything the answer left running is still running.
    pub fn end_turn(&mut self, writer: SandboxedWriterGuard, cx: &mut Context<Self>) {
        if self.session_writer.is_some() {
            return;
        }
        // Held while the list is taken: releasing it first would let a resume
        // through in the gap.
        self.writer.get_or_insert(writer);
        self.look_now(cx);
    }

    /// Looks now instead of waiting for the next interval — after an answer,
    /// and after the user asked for the leftovers to be stopped.
    pub fn look_now(&mut self, cx: &mut Context<Self>) {
        if self.session_writer.is_some() {
            return;
        }
        let listing = self.source.list(cx);
        self.poll = cx.spawn(async move |this, cx| {
            let processes = listing.await;
            this.update(cx, |this, cx| this.settle(processes, cx)).ok();
        });
    }

    fn poll_after(&mut self, interval: Duration, cx: &mut Context<Self>) {
        self.poll = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(interval).await;
            let Some(listing) = this.update(cx, |this, cx| this.source.list(cx)).log_err() else {
                return;
            };
            let processes = listing.await;
            this.update(cx, |this, cx| this.settle(processes, cx)).ok();
        });
    }

    /// Takes in what the sandbox holds now: keeps or releases the writer, says
    /// what the resume dialog shows, and arranges the next look.
    fn settle(&mut self, processes: Result<Vec<SandboxProcess>>, cx: &mut Context<Self>) {
        let processes = match processes {
            Ok(processes) => processes,
            Err(error) => return self.cannot_look(error, cx),
        };
        if !self.baseline_established {
            // A turn ended before its baseline landed, so there is nothing to
            // compare against: what runs now is the baseline rather than an
            // answer's leftovers.
            self.adopt_baseline(&processes);
        }
        {
            let running = processes
                .iter()
                .map(SandboxProcess::key)
                .collect::<HashSet<_>>();
            let mut followed = self.followed.borrow_mut();
            followed.retain(|key| running.contains(key));
            for process in &processes {
                let key = process.key();
                if !self.baseline.contains(&key) && !followed.contains(&key) {
                    followed.push(key);
                }
            }
        }
        if self.followed.borrow().is_empty() {
            self.writer = None;
            self.report = None;
            self.poll_after(IDLE_INTERVAL, cx);
            return;
        }
        // Something appeared between two answers: the agent becomes a writer
        // again, which pauses the worktrees it can write just as its start
        // did.
        if self.writer.is_none() {
            self.writer = Some(self.register_writer(cx));
        }
        self.publish(&processes, cx);
        self.poll_after(WATCHING_INTERVAL, cx);
    }

    fn register_writer(&self, cx: &mut App) -> SandboxedWriterGuard {
        crate::register_agent_writer(self.write_scope(), &self.project, cx)
    }

    fn write_scope(&self) -> AgentWriteScope {
        match self.source.write_scope() {
            Some(paths) => AgentWriteScope::Paths(paths),
            None => AgentWriteScope::Anywhere,
        }
    }

    fn publish(&mut self, processes: &[SandboxProcess], cx: &mut App) {
        if self.report.is_none() {
            let Some(host) = self
                .project
                .read(cx)
                .worktree_code_execution()
                .read(cx)
                .host_key()
                .map(str::to_string)
            else {
                return;
            };
            // Weak, so that a report nothing has pruned yet never keeps the
            // agent's connection — and with it the agent's process — alive.
            let source = Rc::downgrade(&self.source);
            let followed = self.followed.clone();
            let stop = Rc::new(move |cx: &mut App| {
                let Some(source) = source.upgrade() else {
                    return;
                };
                let processes = followed.borrow().clone();
                source.stop(processes, cx).detach_and_log_err(cx);
            }) as Rc<dyn Fn(&mut App)>;
            self.report = Some(LeftoverSandboxProcessReports::publish(
                &host,
                self.write_scope(),
                self.source.agent(),
                stop,
                cx,
            ));
        }
        let Some(report) = &self.report else {
            return;
        };
        let followed = self.followed.borrow();
        let listed = processes
            .iter()
            .filter(|process| followed.contains(&process.key()))
            .map(|process| LeftoverSandboxProcess {
                pid: process.pid,
                command: process.command.clone().into(),
                running_for: process.running_for,
            })
            .collect();
        LeftoverSandboxProcessReports::report(report.id(), listed, cx);
    }

    /// The sandbox could not be looked into. That is not "nothing is running":
    /// it is the case the pause exists for, so the agent becomes a writer of
    /// everything it can write until its connection ends (case B), and nothing
    /// looks again.
    fn cannot_look(&mut self, error: anyhow::Error, cx: &mut App) {
        log::warn!(
            "could not read the processes of the agent's sandbox, so it stays a writer for the \
             rest of this session: {error:#}"
        );
        self.follow_the_whole_session(cx);
    }

    fn follow_the_whole_session(&mut self, cx: &mut App) {
        self.poll = Task::ready(());
        // Registered before the answer's own registration goes, so that the
        // worktrees this covers are never briefly without a writer.
        if self.session_writer.is_none() {
            self.session_writer = Some(self.register_writer(cx));
        }
        self.writer = None;
        self.report = None;
        self.followed.borrow_mut().clear();
        self.baseline.clear();
    }

    /// Whether the agent is a writer because its sandbox cannot be looked
    /// into, rather than because something it left is still running.
    pub fn follows_the_whole_session(&self) -> bool {
        self.session_writer.is_some()
    }

    /// What an answer left running and has not ended, for a test.
    #[cfg(any(test, feature = "test-support"))]
    pub fn leftovers(&self) -> Vec<SandboxProcessKey> {
        self.followed.borrow().clone()
    }
}
