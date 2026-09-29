use crate::{
    branch_diff::RefComparison,
    git_graph::{DraggedCommit, GitGraph},
    git_panel::GitPanel,
    picker_prompt, rewrite_range_modal,
};
use collections::{HashMap, HashSet};
use db::kvp::KeyValueStore;
use editor::{Editor, EditorEvent};
use futures::channel::oneshot::Canceled;
use git::{
    Oid, SHORT_SHA_LENGTH,
    repository::{
        Branch, LogSource, UpstreamTracking, is_merge_request_scratch_branch,
        is_rewrite_scratch_branch, merge_request_scratch_branch_name,
        validate_merge_request_branch_name,
    },
};
use gpui::{
    App, Context, Entity, FocusHandle, Focusable, PromptLevel, SharedString, Stateful,
    Subscription, Task, WeakEntity, Window, px,
};
use notifications::status_toast::StatusToast;
use project::git_store::{
    GitStore, GitStoreEvent, MergeRequestBuild, MergeRequestBuildOutcome, MergeRequestContinuation,
    Repository, RepositoryEvent, RepositoryId,
};
use serde::{Deserialize, Serialize};
use std::{path::Path, rc::Rc, sync::Arc, time::Duration};
use ui::{Checkbox, ToggleState, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::Workspace;

/// How many commits of the development branch the box reads. A range longer than this almost
/// always means the base is wrong, and the box says so instead of counting thousands of commits.
const MAX_RANGE_COMMITS: usize = 2000;

/// How long the box waits after the last change before writing itself to the database, so that
/// typing the message is not a write per keystroke.
const SAVE_DELAY: Duration = Duration::from_millis(300);

/// One commit of the development range, as `git log` reported it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RangeCommit {
    pub sha: Oid,
    pub subject: SharedString,
    pub message: SharedString,
    pub is_merge: bool,
}

/// What the merge request is measured against: the commits of the development branch that the
/// base does not have (`base..development`), oldest first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct DevelopmentRange {
    pub commits: Vec<RangeCommit>,
    /// More than [`MAX_RANGE_COMMITS`]; only the newest ones are in `commits`.
    pub truncated: bool,
}

/// `git log` of `base..development_branch`, oldest first. `%B` goes last so that nothing in a
/// message can shift the fields before it.
fn development_range_args(base: &str, development_branch: &str) -> Vec<String> {
    vec![
        "log".to_string(),
        "--topo-order".to_string(),
        "--reverse".to_string(),
        "-z".to_string(),
        "--format=%H%x1f%P%x1f%s%x1f%B".to_string(),
        format!("--max-count={}", MAX_RANGE_COMMITS + 1),
        format!("{base}..{development_branch}"),
    ]
}

/// `git log --no-walk` of commits the box holds from outside the development range, in the same
/// format as [`development_range_args`], to fill in what was not known when they were added.
fn outside_commit_args(shas: &[Oid]) -> Vec<String> {
    let mut args = vec![
        "log".to_string(),
        "--no-walk=unsorted".to_string(),
        "-z".to_string(),
        "--format=%H%x1f%P%x1f%s%x1f%B".to_string(),
    ];
    args.extend(shas.iter().map(Oid::to_string));
    args
}

/// What the box says when the development range is longer than it reads.
fn range_too_wide_message() -> String {
    i18n::t!(
        "The development branch's range is too wide (more than {MAX_RANGE_COMMITS} commits). Check that the base is right."
    )
}

fn parse_development_range(stdout: &str) -> DevelopmentRange {
    let mut commits = Vec::new();
    for record in stdout.split('\0') {
        let record = record.trim_start_matches(['\r', '\n']);
        if record.is_empty() {
            continue;
        }
        let mut fields = record.splitn(4, '\x1f');
        let (Some(sha), Some(parents), Some(subject)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let Ok(sha) = sha.trim().parse::<Oid>() else {
            continue;
        };
        let message = fields.next().unwrap_or_default().trim_end();
        commits.push(RangeCommit {
            sha,
            subject: SharedString::from(subject.to_string()),
            message: SharedString::from(message.to_string()),
            is_merge: parents.split_whitespace().count() > 1,
        });
    }
    // `--max-count` keeps the newest commits and `--reverse` puts them oldest first, so the one
    // over the limit is at the front.
    let truncated = commits.len() > MAX_RANGE_COMMITS;
    if truncated {
        let excess = commits.len() - MAX_RANGE_COMMITS;
        commits.drain(..excess);
    }
    DevelopmentRange { commits, truncated }
}

/// A commit handed to the box: dragged from the graph or picked from its menus and buttons.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MergeRequestCandidate {
    pub sha: Oid,
    pub subject: SharedString,
    pub message: SharedString,
    pub is_merge: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PlacedCommit {
    sha: Oid,
    subject: SharedString,
    message: SharedString,
    /// Whether the commit was in the development range when the range was last read. A commit
    /// that was, and no longer is, went away with a rewrite of the development branch.
    in_range: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct AddOutcome {
    added: usize,
    already_placed: usize,
    merges: usize,
}

impl AddOutcome {
    fn notice(self) -> Option<SharedString> {
        let merges = (self.merges > 0).then(|| {
            i18n::tn!(
                self.merges,
                "Merge commits cannot go into the box ({count} was left out).",
                "Merge commits cannot go into the box ({count} were left out)."
            )
        });
        let already_placed = (self.already_placed > 0).then(|| {
            i18n::tn!(
                self.already_placed,
                "{count} commit is already in the box.",
                "{count} commits are already in the box."
            )
        });
        match (merges, already_placed) {
            (Some(merges), Some(already_placed)) => {
                Some(i18n::tc!("two sentences", "{0} {1}", merges, already_placed).into())
            }
            (Some(sentence), None) | (None, Some(sentence)) => Some(sentence.into()),
            (None, None) => None,
        }
    }
}

/// What the graph's menus offer for some rows while the box is open.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct MergeRequestMenuChoices {
    /// Not in the box yet. Whether a row is a merge is the graph's to say.
    pub(crate) addable: Vec<Oid>,
    pub(crate) excludable: Vec<Oid>,
    pub(crate) restorable: Vec<Oid>,
}

/// How a row of the graph is marked while the box is open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RowMark {
    Placed,
    Excluded,
    /// In the development range and neither placed nor excluded.
    Unplaced,
}

/// The marks for every row, rebuilt whenever the box changes so that drawing a row only looks
/// one commit up.
#[derive(Debug, Default)]
pub(crate) struct RowMarks {
    placed: HashSet<Oid>,
    excluded: HashSet<Oid>,
    unplaced: HashSet<Oid>,
}

impl RowMarks {
    pub(crate) fn mark(&self, sha: &Oid) -> Option<RowMark> {
        if self.placed.contains(sha) {
            Some(RowMark::Placed)
        } else if self.excluded.contains(sha) {
            Some(RowMark::Excluded)
        } else if self.unplaced.contains(sha) {
            Some(RowMark::Unplaced)
        } else {
            None
        }
    }
}

/// The box as plain data: what is in it, what was set aside, and the development range that
/// tells the two apart from what nobody has looked at yet.
#[derive(Clone, Debug, Default)]
struct MergeRequestBoxState {
    range: Option<DevelopmentRange>,
    /// Where each commit of `range` sits in it.
    range_positions: HashMap<Oid, usize>,
    /// The base and development branch the `in_range` marks were worked out for. Only a re-read
    /// of the same pair may take commits out of the box: a different base simply measures a
    /// different range.
    range_for: Option<(String, String)>,
    /// Oldest first in the development branch's history, then everything from outside it in the
    /// order it was added.
    placed: Vec<PlacedCommit>,
    excluded: Vec<Oid>,
    message: String,
    message_edited: bool,
}

impl MergeRequestBoxState {
    fn is_placed(&self, sha: &Oid) -> bool {
        self.placed.iter().any(|placed| placed.sha == *sha)
    }

    fn is_outside_range(&self, sha: &Oid) -> bool {
        self.range.is_some() && !self.range_positions.contains_key(sha)
    }

    /// Only what the development range counts can be set aside: a merge is never counted, and a
    /// commit from elsewhere was never missing from the box.
    fn can_exclude(&self, sha: &Oid) -> bool {
        let Some(range) = &self.range else {
            return false;
        };
        !self.excluded.contains(sha)
            && self
                .range_positions
                .get(sha)
                .and_then(|position| range.commits.get(*position))
                .is_some_and(|commit| !commit.is_merge)
    }

    fn add(&mut self, candidates: impl IntoIterator<Item = MergeRequestCandidate>) -> AddOutcome {
        let mut outcome = AddOutcome::default();
        for candidate in candidates {
            let in_range = self
                .range_positions
                .get(&candidate.sha)
                .and_then(|position| self.range.as_ref()?.commits.get(*position));
            let is_merge = in_range.map_or(candidate.is_merge, |commit| commit.is_merge);
            if is_merge {
                outcome.merges += 1;
                continue;
            }
            if self.is_placed(&candidate.sha) {
                outcome.already_placed += 1;
                continue;
            }
            let (subject, message) = match in_range {
                Some(commit) => (commit.subject.clone(), commit.message.clone()),
                None => (candidate.subject, candidate.message),
            };
            self.excluded.retain(|sha| *sha != candidate.sha);
            self.placed.push(PlacedCommit {
                sha: candidate.sha,
                subject,
                message,
                in_range: in_range.is_some(),
            });
            outcome.added += 1;
        }
        self.sort_placed();
        self.refresh_message();
        outcome
    }

    fn remove(&mut self, sha: &Oid) {
        self.placed.retain(|placed| placed.sha != *sha);
        self.refresh_message();
    }

    fn exclude(&mut self, sha: &Oid) {
        self.placed.retain(|placed| placed.sha != *sha);
        if !self.excluded.contains(sha) {
            self.excluded.push(*sha);
        }
        self.refresh_message();
    }

    fn restore(&mut self, sha: &Oid) {
        self.excluded.retain(|excluded| excluded != sha);
    }

    /// Cherry-picking replays commits in the order it is given them, so the box keeps the
    /// development branch's order rather than the order things were dropped in.
    fn sort_placed(&mut self) {
        let positions = &self.range_positions;
        self.placed
            .sort_by_key(|placed| positions.get(&placed.sha).copied().unwrap_or(usize::MAX));
    }

    /// Takes a freshly read range, returning how many commits left the box because they are no
    /// longer on the development branch.
    fn set_range(&mut self, range: DevelopmentRange, range_for: (String, String)) -> usize {
        let positions: HashMap<Oid, usize> = range
            .commits
            .iter()
            .enumerate()
            .map(|(position, commit)| (commit.sha, position))
            .collect();
        // A truncated read cannot tell a commit that went away from one past the limit.
        let prune = self.range_for.as_ref() == Some(&range_for) && !range.truncated;
        let mut pruned = 0;
        if prune {
            let before = self.placed.len();
            self.placed
                .retain(|placed| !placed.in_range || positions.contains_key(&placed.sha));
            pruned = before - self.placed.len();
            self.excluded.retain(|sha| positions.contains_key(sha));
        }
        for placed in &mut self.placed {
            placed.in_range = false;
            if let Some(commit) = positions
                .get(&placed.sha)
                .and_then(|position| range.commits.get(*position))
            {
                placed.in_range = true;
                placed.subject = commit.subject.clone();
                placed.message = commit.message.clone();
            }
        }
        self.range_positions = positions;
        self.range = Some(range);
        self.range_for = Some(range_for);
        self.sort_placed();
        self.refresh_message();
        pruned
    }

    /// Forgets the range, for one that could not be read whole: nothing may be marked, listed as
    /// unplaced, or taken out of the box on the strength of part of it.
    fn clear_range(&mut self) {
        self.range = None;
        self.range_positions.clear();
    }

    /// The commits in the box from outside the development range, whose subjects are whatever
    /// the graph knew when they were added.
    fn outside_range_shas(&self) -> Vec<Oid> {
        self.placed
            .iter()
            .filter(|placed| !self.range_positions.contains_key(&placed.sha))
            .map(|placed| placed.sha)
            .take(MAX_RANGE_COMMITS)
            .collect()
    }

    /// Takes what git says about commits from outside the range (see
    /// [`Self::outside_range_shas`]). Returns whether anything in the box changed.
    fn update_outside_commits(&mut self, commits: &[RangeCommit]) -> bool {
        let mut changed = false;
        for placed in &mut self.placed {
            if self.range_positions.contains_key(&placed.sha) {
                continue;
            }
            let Some(commit) = commits.iter().find(|commit| commit.sha == placed.sha) else {
                continue;
            };
            if placed.subject != commit.subject || placed.message != commit.message {
                placed.subject = commit.subject.clone();
                placed.message = commit.message.clone();
                changed = true;
            }
        }
        if changed {
            self.refresh_message();
        }
        changed
    }

    fn unplaced(&self) -> Vec<&RangeCommit> {
        let Some(range) = &self.range else {
            return Vec::new();
        };
        // Sets rather than the lists themselves: this runs on every render of the box, and both
        // the range and the box can hold thousands of commits.
        let placed: HashSet<Oid> = self.placed.iter().map(|placed| placed.sha).collect();
        let excluded: HashSet<Oid> = self.excluded.iter().copied().collect();
        range
            .commits
            .iter()
            .filter(|commit| {
                !commit.is_merge && !placed.contains(&commit.sha) && !excluded.contains(&commit.sha)
            })
            .collect()
    }

    /// Whether every commit of the development range has been placed or excluded. A truncated
    /// read never qualifies: the commits past the limit were never looked at.
    fn everything_checked(&self) -> bool {
        self.range.as_ref().is_some_and(|range| !range.truncated) && self.unplaced().is_empty()
    }

    fn merge_count(&self) -> usize {
        self.range.as_ref().map_or(0, |range| {
            range
                .commits
                .iter()
                .filter(|commit| commit.is_merge)
                .count()
        })
    }

    /// The excluded commits that are in the current range, in its order.
    fn excluded_in_range(&self) -> Vec<&RangeCommit> {
        let Some(range) = &self.range else {
            return Vec::new();
        };
        range
            .commits
            .iter()
            .filter(|commit| self.excluded.contains(&commit.sha))
            .collect()
    }

    fn placed_in_range_count(&self) -> usize {
        self.placed
            .iter()
            .filter(|placed| self.range_positions.contains_key(&placed.sha))
            .count()
    }

    /// The same default the history rewrite gives a group of folded commits: the first commit's
    /// whole message, then the subject of each commit after it.
    fn default_message(&self) -> String {
        let mut placed = self.placed.iter();
        let Some(first) = placed.next() else {
            return String::new();
        };
        let mut message = if first.message.trim().is_empty() {
            first.subject.to_string()
        } else {
            first.message.trim_end().to_string()
        };
        for folded in placed {
            message.push('\n');
            message.push_str(folded.subject.as_ref());
        }
        message
    }

    /// Follows the box until the user has written a message of their own. Returns whether the
    /// message changed.
    fn refresh_message(&mut self) -> bool {
        if self.message_edited {
            return false;
        }
        let message = self.default_message();
        if message == self.message {
            return false;
        }
        self.message = message;
        true
    }

    /// What the message field now says. Text that differs from what the box last put there is the
    /// user's own, and from then on the box leaves the message alone.
    fn set_message_from_editor(&mut self, text: String) -> bool {
        if text == self.message {
            return false;
        }
        self.message = text;
        self.message_edited = true;
        true
    }

    fn placed_shas_oldest_first(&self) -> Vec<String> {
        self.placed
            .iter()
            .map(|placed| placed.sha.to_string())
            .collect()
    }

    fn excluded_shas(&self) -> Vec<Oid> {
        self.excluded_in_range()
            .into_iter()
            .map(|commit| commit.sha)
            .collect()
    }

    fn row_marks(&self) -> RowMarks {
        RowMarks {
            placed: self.placed.iter().map(|placed| placed.sha).collect(),
            excluded: self.excluded.iter().copied().collect(),
            unplaced: self
                .unplaced()
                .into_iter()
                .map(|commit| commit.sha)
                .collect(),
        }
    }

    fn to_saved(
        &self,
        base: Option<&str>,
        merge_request_branch: Option<&str>,
    ) -> SavedMergeRequestBox {
        SavedMergeRequestBox {
            base: base.map(str::to_string),
            merge_request_branch: merge_request_branch.map(str::to_string),
            placed: self
                .placed
                .iter()
                .map(|placed| SavedPlacedCommit {
                    sha: placed.sha.to_string(),
                    subject: placed.subject.to_string(),
                    message: placed.message.to_string(),
                    in_range: placed.in_range,
                })
                .collect(),
            excluded: self.excluded.iter().map(Oid::to_string).collect(),
            message: self.message.clone(),
            message_edited: self.message_edited,
        }
    }

    /// The box as it was saved. The marks were worked out for the saved base, so the first read
    /// of that same range is what takes out the commits a rewrite of the development branch
    /// removed in the meantime.
    fn from_saved(saved: &SavedMergeRequestBox, development_branch: &str) -> Self {
        Self {
            range: None,
            range_positions: HashMap::default(),
            range_for: saved
                .base
                .as_ref()
                .map(|base| (base.clone(), development_branch.to_string())),
            placed: saved
                .placed
                .iter()
                .filter_map(|placed| {
                    Some(PlacedCommit {
                        sha: placed.sha.parse().ok()?,
                        subject: placed.subject.clone().into(),
                        message: placed.message.clone().into(),
                        in_range: placed.in_range,
                    })
                })
                .collect(),
            excluded: saved
                .excluded
                .iter()
                .filter_map(|sha| sha.parse().ok())
                .collect(),
            message: saved.message.clone(),
            message_edited: saved.message_edited,
        }
    }
}

/// What the database keeps of a box, per repository and development branch.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct SavedMergeRequestBox {
    #[serde(default)]
    base: Option<String>,
    #[serde(default)]
    merge_request_branch: Option<String>,
    #[serde(default)]
    placed: Vec<SavedPlacedCommit>,
    #[serde(default)]
    excluded: Vec<String>,
    #[serde(default)]
    message: String,
    #[serde(default)]
    message_edited: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct SavedPlacedCommit {
    sha: String,
    #[serde(default)]
    subject: String,
    #[serde(default)]
    message: String,
    #[serde(default)]
    in_range: bool,
}

/// A build that stopped on a conflict, kept until it is finished or abandoned: the conflict can
/// take a while to resolve, across a restart.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct PendingMergeRequest {
    scratch_branch: String,
    origin_branch: String,
    development_branch: String,
    base: String,
    base_sha: String,
    merge_request_branch: String,
    message: String,
    recreate: bool,
    excluded: Vec<String>,
}

impl PendingMergeRequest {
    fn excluded_oids(&self) -> Vec<Oid> {
        self.excluded
            .iter()
            .filter_map(|sha| sha.parse().ok())
            .collect()
    }
}

/// A build that made the merge-request branch but could not take the user back to where they
/// started: they are left on its scratch branch, which points at the merge request's commit.
#[derive(Clone, Debug, PartialEq, Eq)]
struct StrandedBuild {
    scratch_branch: String,
    origin_branch: String,
    merge_request_branch: String,
    merge_request_sha: String,
}

/// Whether leaving `stranded`'s scratch branch (and deleting it) loses nothing: it must still be
/// at the merge request's commit, which the merge-request branch keeps.
fn can_leave_stranded(stranded: &StrandedBuild, head_sha: Option<&str>) -> bool {
    head_sha == Some(stranded.merge_request_sha.as_str())
}

/// Where a repository's boxes are kept: its path, and for a remote project the machine as well
/// (`RemoteConnectionOptions::machine_key`), so that the same path on this machine and on a
/// server does not share one box. A local path never starts with `@`, and stays as it was so
/// that boxes saved before are still found.
pub(crate) fn merge_request_storage_location(
    work_directory_abs_path: &Path,
    remote_machine: Option<&str>,
) -> String {
    match remote_machine {
        None => work_directory_abs_path.to_string_lossy().into_owned(),
        Some(machine) => format!("@{machine}:{}", work_directory_abs_path.to_string_lossy()),
    }
}

fn box_key(location: &str, development_branch: &str) -> String {
    format!("git-merge-request-box:{location}:{development_branch}")
}

fn pending_key(location: &str, scratch_branch: &str) -> String {
    format!("git-merge-request-pending:{location}:{scratch_branch}")
}

/// The default base: the repository's default branch, on the remote when the local branch tracks
/// one, because the merge request is going to be measured against the remote.
fn preferred_base(default_branch: Option<&str>, branches: &[Branch]) -> Option<String> {
    let default_branch = default_branch?;
    let upstream = branches
        .iter()
        .find(|branch| !branch.is_remote() && branch.name() == default_branch)
        .and_then(|branch| branch.upstream.as_ref())
        .filter(|upstream| matches!(upstream.tracking, UpstreamTracking::Tracked(_)))
        .and_then(|upstream| upstream.stripped_ref_name());
    Some(upstream.unwrap_or(default_branch).to_string())
}

/// A local base that the remote has moved past gives a merge request that looks bigger than it
/// is, and one that conflicts when it lands.
fn stale_base_warning(base: &str, branches: &[Branch]) -> Option<String> {
    let upstream = branches
        .iter()
        .find(|branch| !branch.is_remote() && branch.name() == base)?
        .upstream
        .as_ref()?;
    let UpstreamTracking::Tracked(status) = upstream.tracking else {
        return None;
    };
    if status.behind == 0 {
        return None;
    }
    let remote = upstream.stripped_ref_name()?;
    Some(i18n::tn!(
        status.behind,
        "The local {base} is {count} commit behind the remote ({remote}). Use {remote} as the base, or pull first.",
        "The local {base} is {count} commits behind the remote ({remote}). Use {remote} as the base, or pull first."
    ))
}

/// Draws a row's mark in the graph. An excluded row is only dimmed, which the graph does itself.
pub(crate) fn render_row_mark(mark: RowMark, cx: &App) -> Option<AnyElement> {
    match mark {
        RowMark::Placed => Some(
            Icon::new(IconName::Check)
                .size(IconSize::XSmall)
                .color(Color::Accent)
                .into_any_element(),
        ),
        RowMark::Unplaced => Some(
            div()
                .flex_none()
                .size(px(6.))
                .rounded_full()
                .bg(cx.theme().status().warning)
                .into_any_element(),
        ),
        RowMark::Excluded => None,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RangeStatus {
    Idle,
    Loading,
    Loaded,
    Failed(SharedString),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Notice {
    text: SharedString,
    color: Color,
}

impl Notice {
    fn warning(text: impl Into<SharedString>) -> Self {
        Self {
            text: text.into(),
            color: Color::Warning,
        }
    }
}

/// Everything a build needs, checked while the user can still be told what is wrong.
struct BuildPlan {
    build: MergeRequestBuild,
    work_directory_abs_path: Arc<Path>,
    development_branch: String,
    excluded: Vec<Oid>,
    unplaced: usize,
    /// Why the unplaced count cannot be trusted to have covered the whole development range.
    unverified_range: Option<String>,
    previous_sha: Option<String>,
}

/// The "MR の箱": commits dragged in from the graph, a message, and the merge-request branch
/// built out of them, next to the list of the development branch's commits nobody has placed or
/// excluded yet.
pub(crate) struct MergeRequestBox {
    git_store: Entity<GitStore>,
    repo_id: RepositoryId,
    workspace: WeakEntity<Workspace>,
    graph: WeakEntity<GitGraph>,
    focus_handle: FocusHandle,
    development_branch: Option<SharedString>,
    base: Option<SharedString>,
    state: MergeRequestBoxState,
    row_marks: Rc<RowMarks>,
    message_editor: Entity<Editor>,
    branch_name_editor: Entity<Editor>,
    recreate: bool,
    excluded_expanded: bool,
    range_status: RangeStatus,
    box_notice: Option<Notice>,
    create_notice: Option<Notice>,
    pending: Option<PendingMergeRequest>,
    stranded: Option<StrandedBuild>,
    /// The repository's default branch by its local name (`main` for `origin/main`), which is
    /// never the merge-request branch.
    local_default_branch: Option<SharedString>,
    building: bool,
    /// The saved box for the development branch is still being read, and a save now would
    /// replace it with the empty one on screen.
    loading_saved: bool,
    /// Commits handed over while the saved box was still being read (the multi-selection menu
    /// opens the box and fills it in one go), added once it has been.
    queued_candidates: Vec<MergeRequestCandidate>,
    /// `RemoteConnectionOptions::machine_key` of a remote project.
    remote_machine: Option<String>,
    _range_task: Option<Task<()>>,
    _outside_task: Option<Task<()>>,
    _load_task: Option<Task<()>>,
    _save_task: Option<Task<()>>,
    _pending_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl MergeRequestBox {
    pub(crate) fn new(
        git_store: Entity<GitStore>,
        repo_id: RepositoryId,
        workspace: WeakEntity<Workspace>,
        graph: WeakEntity<GitGraph>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let message_editor = cx.new(|cx| {
            let mut editor = Editor::auto_height(3, 12, window, cx);
            editor.set_placeholder_text(i18n::t!("Commit message for the MR"), window, cx);
            editor
        });
        let branch_name_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text(i18n::t!("e.g. mr/new-feature"), window, cx);
            editor
        });
        let subscriptions = vec![
            cx.subscribe(
                &message_editor,
                |this: &mut Self, editor, event: &EditorEvent, cx| {
                    if matches!(event, EditorEvent::BufferEdited) {
                        let text = editor.read(cx).text(cx);
                        if this.state.set_message_from_editor(text) {
                            this.schedule_save(cx);
                        }
                    }
                },
            ),
            cx.subscribe(
                &branch_name_editor,
                |this: &mut Self, _, event: &EditorEvent, cx| {
                    if matches!(event, EditorEvent::BufferEdited) {
                        this.create_notice = None;
                        this.schedule_save(cx);
                        cx.notify();
                    }
                },
            ),
            cx.subscribe_in(
                &git_store,
                window,
                |this: &mut Self, _, event: &GitStoreEvent, window, cx| {
                    let GitStoreEvent::RepositoryUpdated(repo_id, event, _) = event else {
                        return;
                    };
                    if *repo_id != this.repo_id {
                        return;
                    }
                    match event {
                        RepositoryEvent::HeadChanged | RepositoryEvent::BranchListChanged => {
                            this.refresh_pending(cx);
                            this.reload_range(window, cx);
                            cx.notify();
                        }
                        RepositoryEvent::StatusesChanged => cx.notify(),
                        _ => {}
                    }
                },
            ),
        ];
        let remote_machine = workspace
            .upgrade()
            .and_then(|workspace| {
                workspace
                    .read(cx)
                    .project()
                    .read(cx)
                    .remote_connection_options(cx)
            })
            .map(|options| options.machine_key());
        let mut this = Self {
            git_store,
            repo_id,
            workspace,
            graph,
            focus_handle: cx.focus_handle(),
            development_branch: None,
            base: None,
            state: MergeRequestBoxState::default(),
            row_marks: Rc::default(),
            message_editor,
            branch_name_editor,
            recreate: false,
            excluded_expanded: false,
            range_status: RangeStatus::Idle,
            box_notice: None,
            create_notice: None,
            pending: None,
            stranded: None,
            local_default_branch: None,
            building: false,
            loading_saved: false,
            queued_candidates: Vec::new(),
            remote_machine,
            _range_task: None,
            _outside_task: None,
            _load_task: None,
            _save_task: None,
            _pending_task: None,
            _subscriptions: subscriptions,
        };
        this.start(window, cx);
        this
    }

    fn repository(&self, cx: &App) -> Option<Entity<Repository>> {
        self.git_store
            .read(cx)
            .repositories()
            .get(&self.repo_id)
            .cloned()
    }

    fn current_branch(&self, cx: &App) -> Option<String> {
        let repository = self.repository(cx)?;
        let repository = repository.read(cx);
        repository
            .branch
            .as_ref()
            .map(|branch| branch.name().to_string())
    }

    fn branches(&self, cx: &App) -> Vec<Branch> {
        self.repository(cx)
            .map(|repository| repository.read(cx).branch_list.to_vec())
            .unwrap_or_default()
    }

    fn storage_location(&self, work_directory_abs_path: &Path) -> String {
        merge_request_storage_location(work_directory_abs_path, self.remote_machine.as_deref())
    }

    fn work_directory_abs_path(&self, cx: &App) -> Option<Arc<Path>> {
        Some(
            self.repository(cx)?
                .read(cx)
                .work_directory_abs_path
                .clone(),
        )
    }

    pub(crate) fn row_marks(&self) -> Rc<RowMarks> {
        self.row_marks.clone()
    }

    /// Opens on the branch that is checked out, which is what the user is developing on. On the
    /// scratch branch of a build that stopped, it opens on the branch that build came from.
    fn start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let current = self.current_branch(cx);
        let scratch = current
            .clone()
            .filter(|branch| is_merge_request_scratch_branch(branch));
        let (Some(scratch), Some(work_directory_abs_path)) =
            (scratch, self.work_directory_abs_path(cx))
        else {
            let development_branch = current.filter(|branch| !is_rewrite_scratch_branch(branch));
            self.set_development_branch(development_branch, window, cx);
            return;
        };
        let pending = read_pending(
            &self.storage_location(&work_directory_abs_path),
            &scratch,
            cx,
        );
        self.loading_saved = true;
        self._load_task = Some(cx.spawn_in(window, async move |this, cx| {
            let pending = pending.await;
            this.update_in(cx, |this, window, cx| {
                let development_branch = pending
                    .as_ref()
                    .map(|pending| pending.development_branch.clone());
                this.pending = pending;
                this.set_development_branch(development_branch, window, cx);
            })
            .ok();
        }));
    }

    /// Switches the box to `development_branch`'s own saved box.
    fn set_development_branch(
        &mut self,
        development_branch: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.development_branch = development_branch.clone().map(SharedString::from);
        self.state = MergeRequestBoxState::default();
        self.base = None;
        self.recreate = false;
        self.range_status = RangeStatus::Idle;
        self.box_notice = None;
        self.create_notice = None;
        self._range_task = None;
        self._save_task = None;
        self.rebuild_row_marks();
        let (Some(development_branch), Some(repository), Some(work_directory_abs_path)) = (
            development_branch,
            self.repository(cx),
            self.work_directory_abs_path(cx),
        ) else {
            self.loading_saved = false;
            if !self.queued_candidates.is_empty() {
                self.queued_candidates.clear();
                self.box_notice = Some(Notice::warning(i18n::t!(
                    "The commits were not put in the box because no development branch is chosen. Choose a development branch first."
                )));
            }
            cx.notify();
            return;
        };
        self.loading_saved = true;
        let key = box_key(
            &self.storage_location(&work_directory_abs_path),
            &development_branch,
        );
        let store = KeyValueStore::global(cx);
        let saved = cx.background_spawn(async move { store.read_kvp(&key).log_err().flatten() });
        let default_branch = repository.update(cx, |repository, _| repository.default_branch(true));
        let local_default_branch =
            repository.update(cx, |repository, _| repository.default_branch(false));
        self._load_task = Some(cx.spawn_in(window, async move |this, cx| {
            let saved = saved
                .await
                .and_then(|json| serde_json::from_str::<SavedMergeRequestBox>(&json).log_err())
                .unwrap_or_default();
            let read_default_branch =
                |result: Result<anyhow::Result<Option<SharedString>>, Canceled>| match result {
                    Ok(Ok(branch)) => branch,
                    Ok(Err(error)) => {
                        log::warn!("could not look up the default branch: {error:#}");
                        None
                    }
                    Err(_) => None,
                };
            let default_branch = read_default_branch(default_branch.await);
            let local_default_branch = read_default_branch(local_default_branch.await);
            this.update_in(cx, |this, window, cx| {
                if this.development_branch.as_deref() != Some(development_branch.as_str()) {
                    return;
                }
                this.local_default_branch = local_default_branch;
                this.apply_saved(saved, default_branch, &development_branch, window, cx);
            })
            .ok();
        }));
        cx.notify();
    }

    fn apply_saved(
        &mut self,
        saved: SavedMergeRequestBox,
        default_branch: Option<SharedString>,
        development_branch: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.state = MergeRequestBoxState::from_saved(&saved, development_branch);
        let branches = self.branches(cx);
        self.base = saved
            .base
            .clone()
            .or_else(|| preferred_base(default_branch.as_deref(), &branches))
            .map(SharedString::from);
        let branch_name = saved
            .merge_request_branch
            .unwrap_or_else(|| format!("mr/{development_branch}"));
        self.branch_name_editor.update(cx, |editor, cx| {
            editor.set_text(branch_name, window, cx);
        });
        let queued = std::mem::take(&mut self.queued_candidates);
        let added_queued = !queued.is_empty();
        if added_queued {
            self.box_notice = self.state.add(queued).notice().map(Notice::warning);
        }
        self.state.refresh_message();
        self.sync_message_editor(window, cx);
        self.loading_saved = false;
        if added_queued {
            self.schedule_save(cx);
        }
        self.rebuild_row_marks();
        self.reload_range(window, cx);
        cx.notify();
    }

    fn sync_message_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let message = self.state.message.clone();
        self.message_editor.update(cx, |editor, cx| {
            if editor.text(cx) != message {
                editor.set_text(message, window, cx);
            }
        });
    }

    fn rebuild_row_marks(&mut self) {
        self.row_marks = Rc::new(self.state.row_marks());
    }

    /// What every change to the box goes through: the message follows the box (unless the user
    /// wrote their own), the graph's marks follow the message, and the whole lot is saved.
    fn after_state_change(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_message_editor(window, cx);
        self.rebuild_row_marks();
        self.create_notice = None;
        self.schedule_save(cx);
        cx.notify();
    }

    fn reload_range(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(base), Some(development_branch), Some(repository)) = (
            self.base.clone(),
            self.development_branch.clone(),
            self.repository(cx),
        ) else {
            self.range_status = RangeStatus::Idle;
            return;
        };
        if self.loading_saved {
            return;
        }
        let receiver = repository.update(cx, |repository, _| {
            repository.run_git_command(development_range_args(&base, &development_branch), None)
        });
        self.range_status = RangeStatus::Loading;
        self._range_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = receiver.await;
            this.update_in(cx, |this, window, cx| {
                if this.base.as_ref() != Some(&base)
                    || this.development_branch.as_ref() != Some(&development_branch)
                {
                    return;
                }
                this.range_status = match result {
                    Ok(Ok(output)) if output.succeeded() => {
                        let range = parse_development_range(&output.stdout);
                        // Refused rather than shown in part: a commit past the limit cannot be
                        // told from one a rewrite took away, so nothing could be taken out of
                        // the box, and what is listed as left over would not be all of it.
                        if range.truncated {
                            this.state.clear_range();
                            this.after_state_change(window, cx);
                            this.refresh_outside_commits(window, cx);
                            this.range_status =
                                RangeStatus::Failed(range_too_wide_message().into());
                            cx.notify();
                            return;
                        }
                        let pruned = this.state.set_range(
                            range,
                            (base.to_string(), development_branch.to_string()),
                        );
                        if pruned > 0 {
                            this.box_notice = Some(Notice::warning(i18n::tn!(
                                pruned,
                                "The development branch was rewritten (or merged into the base), so {count} commit was taken out of the box.",
                                "The development branch was rewritten (or merged into the base), so {count} commits were taken out of the box."
                            )));
                        }
                        this.after_state_change(window, cx);
                        this.refresh_outside_commits(window, cx);
                        RangeStatus::Loaded
                    }
                    Ok(Ok(output)) => RangeStatus::Failed(
                        i18n::t!(
                            "Could not read the development branch's range: {}",
                            output.failure_message()
                        )
                        .into(),
                    ),
                    Ok(Err(error)) => RangeStatus::Failed(
                        i18n::t!(
                            "Could not read the development branch's range: {}",
                            format!("{error:#}")
                        )
                        .into(),
                    ),
                    Err(_) => return,
                };
                cx.notify();
            })
            .ok();
        }));
    }

    /// Re-reads the subjects of the commits from outside the development range, which a commit
    /// the graph had not loaded yet when it was dropped into the box would otherwise keep as its
    /// short SHA. Runs with every read of the range, which follows every change of the refs.
    fn refresh_outside_commits(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let shas = self.state.outside_range_shas();
        let Some(repository) = self.repository(cx).filter(|_| !shas.is_empty()) else {
            self._outside_task = None;
            return;
        };
        let receiver = repository.update(cx, |repository, _| {
            repository.run_git_command(outside_commit_args(&shas), None)
        });
        self._outside_task = Some(cx.spawn_in(window, async move |this, cx| {
            let output = match receiver.await {
                Ok(Ok(output)) if output.succeeded() => output,
                Ok(Ok(output)) => {
                    log::warn!(
                        "could not read the commits outside the development range: {}",
                        output.failure_message()
                    );
                    return;
                }
                Ok(Err(error)) => {
                    log::warn!(
                        "could not read the commits outside the development range: {error:#}"
                    );
                    return;
                }
                Err(_) => return,
            };
            let commits = parse_development_range(&output.stdout).commits;
            this.update_in(cx, |this, window, cx| {
                if this.state.update_outside_commits(&commits) {
                    this.after_state_change(window, cx);
                }
            })
            .ok();
        }));
    }

    fn schedule_save(&mut self, cx: &mut Context<Self>) {
        if self.loading_saved {
            return;
        }
        let (Some(development_branch), Some(work_directory_abs_path)) = (
            self.development_branch.clone(),
            self.work_directory_abs_path(cx),
        ) else {
            return;
        };
        let branch_name = self.branch_name_editor.read(cx).text(cx);
        let saved = self
            .state
            .to_saved(self.base.as_deref(), Some(branch_name.trim()));
        let key = box_key(
            &self.storage_location(&work_directory_abs_path),
            &development_branch,
        );
        let store = KeyValueStore::global(cx);
        self._save_task = Some(cx.spawn(async move |_, cx| {
            cx.background_executor().timer(SAVE_DELAY).await;
            let Some(json) = serde_json::to_string(&saved).log_err() else {
                return;
            };
            store.write_kvp(key, json).await.log_err();
        }));
    }

    /// Re-reads the build this box has to finish, when the checked-out branch is its scratch
    /// branch.
    fn refresh_pending(&mut self, cx: &mut Context<Self>) {
        let current = self.current_branch(cx);
        let Some(scratch) = current.filter(|branch| is_merge_request_scratch_branch(branch)) else {
            if self.pending.take().is_some() {
                cx.notify();
            }
            self._pending_task = None;
            return;
        };
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.scratch_branch == scratch)
        {
            return;
        }
        let Some(work_directory_abs_path) = self.work_directory_abs_path(cx) else {
            return;
        };
        let pending = read_pending(
            &self.storage_location(&work_directory_abs_path),
            &scratch,
            cx,
        );
        self._pending_task = Some(cx.spawn(async move |this, cx| {
            let pending = pending.await;
            this.update(cx, |this, cx| {
                this.pending = pending;
                cx.notify();
            })
            .ok();
        }));
    }

    pub(crate) fn add_candidates(
        &mut self,
        candidates: Vec<MergeRequestCandidate>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.loading_saved {
            self.queued_candidates.extend(candidates);
            return;
        }
        if self.development_branch.is_none() {
            self.box_notice = Some(Notice::warning(i18n::t!(
                "No development branch is chosen. Choose a development branch first."
            )));
            cx.notify();
            return;
        }
        let outcome = self.state.add(candidates);
        self.after_state_change(window, cx);
        self.box_notice = outcome.notice().map(Notice::warning);
    }

    fn drop_commits(
        &mut self,
        dragged: &DraggedCommit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let shas = dragged.selection.to_vec();
        let candidates = self
            .graph
            .update(cx, |graph, cx| graph.merge_request_candidates(&shas, cx))
            .unwrap_or_default();
        self.add_candidates(candidates, window, cx);
    }

    fn remove(&mut self, sha: Oid, window: &mut Window, cx: &mut Context<Self>) {
        self.state.remove(&sha);
        self.box_notice = None;
        self.after_state_change(window, cx);
    }

    /// Which of `shas` the graph's menus may put in the box, set aside, or bring back.
    pub(crate) fn menu_choices(&self, shas: &[Oid]) -> MergeRequestMenuChoices {
        MergeRequestMenuChoices {
            addable: shas
                .iter()
                .copied()
                .filter(|sha| !self.state.is_placed(sha))
                .collect(),
            excludable: shas
                .iter()
                .copied()
                .filter(|sha| self.state.can_exclude(sha))
                .collect(),
            restorable: shas
                .iter()
                .copied()
                .filter(|sha| self.state.excluded.contains(sha))
                .collect(),
        }
    }

    pub(crate) fn exclude_commits(
        &mut self,
        shas: &[Oid],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The range can change between opening the graph's menu and picking the entry.
        for sha in shas {
            if self.state.can_exclude(sha) {
                self.state.exclude(sha);
            }
        }
        self.box_notice = None;
        self.after_state_change(window, cx);
    }

    pub(crate) fn restore_commits(
        &mut self,
        shas: &[Oid],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for sha in shas {
            self.state.restore(sha);
        }
        self.after_state_change(window, cx);
    }

    fn select_first_unplaced(&mut self, cx: &mut Context<Self>) {
        let Some(sha) = self.state.unplaced().first().map(|commit| commit.sha) else {
            return;
        };
        self.graph
            .update(cx, |graph, cx| graph.select_commit_by_sha(sha, cx))
            .ok();
    }

    fn pick_development_branch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut options: Vec<SharedString> = self
            .branches(cx)
            .iter()
            .filter(|branch| !branch.is_remote() && !is_rewrite_scratch_branch(branch.name()))
            .map(|branch| SharedString::from(branch.name().to_string()))
            .collect();
        options.sort();
        options.dedup();
        let prompt = picker_prompt::prompt(
            i18n::t!("Choose the Development Branch (Where the Commits Come From)"),
            options.clone(),
            self.workspace.clone(),
            window,
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            let Some(branch) = prompt.await.and_then(|index| options.get(index).cloned()) else {
                return;
            };
            this.update_in(cx, |this, window, cx| {
                if this.development_branch.as_ref() != Some(&branch) {
                    this.set_development_branch(Some(branch.to_string()), window, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Every branch, remote branch and tag, searchable: the base is often a remote branch or a
    /// release tag far down a long list.
    fn pick_base(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repository) = self.repository(cx) else {
            return;
        };
        let branches = self.branches(cx);
        let tags = repository.update(cx, |repository, _| repository.tags());
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |this, cx| {
            let tags = match tags.await {
                Ok(Ok(tags)) => tags,
                Ok(Err(error)) => {
                    log::warn!("could not list the tags: {error:#}");
                    Vec::new()
                }
                Err(_) => return,
            };
            let mut local = Vec::new();
            let mut remote = Vec::new();
            for branch in &branches {
                if is_rewrite_scratch_branch(branch.name()) {
                    continue;
                }
                let name = SharedString::from(branch.name().to_string());
                if branch.is_remote() {
                    remote.push(name);
                } else {
                    local.push(name);
                }
            }
            remote.sort();
            local.sort();
            let mut options = remote;
            options.extend(local);
            options.extend(tags.into_iter().map(|tag| SharedString::from(tag.name)));
            let Ok(prompt) = cx.update(|window, cx| {
                picker_prompt::prompt(
                    i18n::t!("Choose the Base Branch or Tag (Where the MR Goes)"),
                    options.clone(),
                    workspace,
                    window,
                    cx,
                )
            }) else {
                return;
            };
            let Some(base) = prompt.await.and_then(|index| options.get(index).cloned()) else {
                return;
            };
            this.update_in(cx, |this, window, cx| {
                this.base = Some(base);
                this.box_notice = None;
                this.schedule_save(cx);
                this.reload_range(window, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn branch_name(&self, cx: &App) -> String {
        self.branch_name_editor.read(cx).text(cx).trim().to_string()
    }

    fn branch_exists(&self, name: &str, cx: &App) -> bool {
        self.branches(cx)
            .iter()
            .any(|branch| !branch.is_remote() && branch.name() == name)
    }

    fn worktree_is_dirty(&self, cx: &App) -> bool {
        self.repository(cx).is_some_and(|repository| {
            repository
                .read(cx)
                .cached_status()
                .any(|entry| entry.status.has_changes())
        })
    }

    /// Everything that would make the build refuse, checked here so that the user is told why
    /// with the box still as they left it.
    fn prepare_build(&self, cx: &App) -> Result<BuildPlan, SharedString> {
        let development_branch = self
            .development_branch
            .as_ref()
            .ok_or(i18n::t!("Choose a development branch."))?
            .to_string();
        let base = self
            .base
            .as_ref()
            .ok_or(i18n::t!("Choose a base."))?
            .to_string();
        let newest = self
            .state
            .placed
            .last()
            .ok_or(i18n::t!("The box is empty. Drag commits into it."))?
            .sha;
        let merge_request_branch = self.branch_name(cx);
        validate_merge_request_branch_name(&merge_request_branch)
            .map_err(|error| SharedString::from(error.to_string()))?;
        // Before the existing-branch check, so that the reason given is this one whether or not
        // "作り直す" is ticked.
        git::repository::ensure_merge_request_branch_is_not_default(
            &merge_request_branch,
            self.local_default_branch.as_deref(),
        )
        .map_err(|error| SharedString::from(error.to_string()))?;
        let message = self.state.message.clone();
        if message.trim().is_empty() {
            return Err(i18n::t!("Enter a commit message.").into());
        }
        let exists = self.branch_exists(&merge_request_branch, cx);
        if exists && !self.recreate {
            return Err(i18n::t!(
                "Branch \"{merge_request_branch}\" already exists. Check the recreate option to rebuild it."
            )
            .into());
        }
        let origin_branch = self.current_branch(cx).ok_or(i18n::t!(
            "Cannot create an MR branch on a detached HEAD. Switch to a branch first."
        ))?;
        if is_rewrite_scratch_branch(&origin_branch) {
            return Err(i18n::t!(
                "Cannot create it on the scratch branch \"{origin_branch}\". Go back to the original branch first."
            )
            .into());
        }
        if merge_request_branch == origin_branch || merge_request_branch == development_branch {
            return Err(i18n::t!(
                "\"{merge_request_branch}\" is a branch under development. Give the MR branch another name."
            )
            .into());
        }
        if merge_request_branch == base {
            return Err(i18n::t!(
                "The base branch \"{merge_request_branch}\" cannot be the MR branch. Use another name."
            )
            .into());
        }
        // An older git moves a branch another worktree has checked out without a word, leaving
        // that worktree's files looking like changes against the new commit.
        let checked_out_elsewhere = self.repository(cx).is_some_and(|repository| {
            let ref_name = format!("refs/heads/{merge_request_branch}");
            repository
                .read(cx)
                .linked_worktrees()
                .iter()
                .any(|worktree| worktree.ref_name.as_deref() == Some(ref_name.as_str()))
        });
        if exists && checked_out_elsewhere {
            return Err(i18n::t!(
                "\"{merge_request_branch}\" is checked out in another worktree, so it cannot be recreated. Use another name."
            )
            .into());
        }
        if self.worktree_is_dirty(cx) {
            return Err(i18n::t!(
                "There are uncommitted changes. Commit or stash them before creating the MR branch (the box stays as it is)."
            )
            .into());
        }
        let scratch_branch = merge_request_scratch_branch_name(&newest.to_string());
        if self.branch_exists(&scratch_branch, cx) {
            return Err(i18n::t!(
                "The scratch branch \"{scratch_branch}\" of the previous attempt is still there. Check its contents and delete it, then try again."
            )
            .into());
        }
        let work_directory_abs_path = self
            .work_directory_abs_path(cx)
            .ok_or(i18n::t!("Repository not found."))?;
        let previous_sha = exists
            .then(|| {
                self.branches(cx)
                    .into_iter()
                    .find(|branch| !branch.is_remote() && branch.name() == merge_request_branch)
                    .and_then(|branch| {
                        branch
                            .most_recent_commit
                            .map(|commit| commit.sha.to_string())
                    })
            })
            .flatten();
        Ok(BuildPlan {
            build: MergeRequestBuild {
                base,
                shas_oldest_first: self.state.placed_shas_oldest_first(),
                merge_request_branch,
                message,
                recreate: exists && self.recreate,
                origin_branch,
                scratch_branch,
            },
            work_directory_abs_path,
            development_branch,
            excluded: self.state.excluded_shas(),
            unplaced: self.state.unplaced().len(),
            unverified_range: self.unverified_range(),
            previous_sha,
        })
    }

    fn unverified_range(&self) -> Option<String> {
        match (&self.state.range, &self.range_status) {
            (None, _) | (_, RangeStatus::Failed(_)) => Some(
                i18n::t!(
                    "The development branch's range could not be read, so left-out commits were not checked."
                )
                .to_string(),
            ),
            (Some(range), _) if range.truncated => Some(i18n::t!(
                "The development branch's range has more than {MAX_RANGE_COMMITS} commits, so older commits were not checked for being left out."
            )),
            _ => None,
        }
    }

    fn create(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.building {
            return;
        }
        let plan = match self.prepare_build(cx) {
            Ok(plan) => plan,
            Err(reason) => {
                self.create_notice = Some(Notice::warning(reason));
                cx.notify();
                return;
            }
        };
        let Some(repository) = self.repository(cx) else {
            return;
        };
        let build = &plan.build;
        let title = if plan.unplaced > 0 {
            i18n::tn!(
                plan.unplaced,
                "{count} commit is not placed. Create the MR branch anyway?",
                "{count} commits are not placed. Create the MR branch anyway?"
            )
        } else if plan.unverified_range.is_some() {
            i18n::t!(
                "Not every commit could be checked for being left out. Create the MR branch anyway?"
            )
            .to_string()
        } else {
            i18n::t!("Create the MR branch?").to_string()
        };
        let mut detail = i18n::tn!(
            build.shas_oldest_first.len(),
            "Creates \"{branch}\" on top of the base \"{base}\" from the box's {count} commit.\n\
             The work is done on the scratch branch \"{scratch}\", and then Zed returns to \"{origin}\".",
            "Creates \"{branch}\" on top of the base \"{base}\" by squashing the box's {count} commits into one.\n\
             The work is done on the scratch branch \"{scratch}\", and then Zed returns to \"{origin}\".",
            base = build.base,
            branch = build.merge_request_branch,
            scratch = build.scratch_branch,
            origin = build.origin_branch,
        );
        if build.recreate {
            let previous = plan
                .previous_sha
                .as_deref()
                .map(short_sha)
                .unwrap_or_default();
            detail.push('\n');
            detail.push_str(&i18n::t!(
                "The existing \"{}\" ({previous}) is recreated to point at the new commit. If it was pushed, it needs a force push.",
                build.merge_request_branch
            ));
        }
        if let Some(note) = &plan.unverified_range {
            detail.push('\n');
            detail.push_str(note);
        }
        let confirmation = window.prompt(
            PromptLevel::Info,
            &title,
            Some(&detail),
            &[i18n::tc!("merge request box", "Create"), i18n::t!("Cancel")],
            cx,
        );
        self.building = true;
        self.create_notice = None;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            if confirmation.await.unwrap_or(1) != 0 {
                this.update(cx, |this, cx| {
                    this.building = false;
                    cx.notify();
                })
                .ok();
                return;
            }
            // Written before anything moves and awaited: a build that stops on a conflict parks
            // the user on the scratch branch, and the git panel's banner can only take them back
            // to a branch that was remembered first.
            let remembered = cx.update(|_, cx| {
                rewrite_range_modal::remember_rewrite_origin(
                    &plan.work_directory_abs_path,
                    &plan.build.scratch_branch,
                    &plan.build.origin_branch,
                    cx,
                )
            });
            if let Ok(remembered) = remembered {
                remembered.await;
            }
            let Ok(receiver) = cx.update(|_, cx| {
                repository.update(cx, |repository, cx| {
                    repository.build_merge_request_branch(plan.build.clone(), cx)
                })
            }) else {
                return;
            };
            let result = receiver.await;
            this.update_in(cx, |this, window, cx| {
                this.building = false;
                this.handle_build_result(result, repository, plan, window, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn handle_build_result(
        &mut self,
        result: Result<anyhow::Result<MergeRequestBuildOutcome>, Canceled>,
        repository: Entity<Repository>,
        plan: BuildPlan,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let BuildPlan {
            build,
            work_directory_abs_path,
            development_branch,
            excluded,
            ..
        } = plan;
        match result {
            Ok(Ok(MergeRequestBuildOutcome::Created {
                merge_request_sha,
                previous_sha,
                return_failure,
            })) => {
                if return_failure.is_none() {
                    forget_origin(&work_directory_abs_path, &build.scratch_branch, cx);
                }
                self.recreate = false;
                self.report_created(
                    repository.clone(),
                    &build.merge_request_branch,
                    previous_sha,
                    cx,
                );
                if let Some(failure) = return_failure {
                    self.report_error(
                        i18n::t!(
                            "Created the MR branch {}, but could not go back to {}. You are on the scratch branch \"{}\" (\"Go Back\" in the MR box returns you).\n{failure}",
                            build.merge_request_branch, build.origin_branch, build.scratch_branch
                        ),
                        cx,
                    );
                    self.stranded = Some(StrandedBuild {
                        scratch_branch: build.scratch_branch.clone(),
                        origin_branch: build.origin_branch.clone(),
                        merge_request_branch: build.merge_request_branch.clone(),
                        merge_request_sha,
                    });
                }
                self.open_comparison(
                    repository,
                    build.base,
                    development_branch,
                    build.merge_request_branch,
                    excluded,
                    window,
                    cx,
                );
            }
            Ok(Ok(MergeRequestBuildOutcome::Stopped { base_sha, reason })) => {
                let pending = PendingMergeRequest {
                    scratch_branch: build.scratch_branch.clone(),
                    origin_branch: build.origin_branch.clone(),
                    development_branch,
                    base: build.base.clone(),
                    base_sha,
                    merge_request_branch: build.merge_request_branch.clone(),
                    message: build.message.clone(),
                    recreate: build.recreate,
                    excluded: excluded.iter().map(Oid::to_string).collect(),
                };
                write_pending(
                    &self.storage_location(&work_directory_abs_path),
                    &pending,
                    cx,
                );
                self.pending = Some(pending);
                self.report_error(
                    i18n::t!(
                        "A conflict happened while applying the box's commits. Stopped on the scratch branch \"{}\".\n\
                         Resolve it and press \"Continue\" in the Git panel's banner, then finish with \"Next Steps\" in the MR box. To give up, \"Abort\" in the banner returns to \"{}\".\n{reason}",
                        build.scratch_branch, build.origin_branch
                    ),
                    cx,
                );
            }
            Ok(Ok(MergeRequestBuildOutcome::Refused {
                reason,
                left_on_scratch_branch,
            })) => {
                let mut message = i18n::t!("Could not create the MR branch\n{reason}");
                if left_on_scratch_branch {
                    message.push('\n');
                    message.push_str(&i18n::t!(
                        "Still on the scratch branch \"{}\".",
                        build.scratch_branch
                    ));
                } else {
                    forget_origin(&work_directory_abs_path, &build.scratch_branch, cx);
                }
                self.report_error(message, cx);
            }
            Ok(Err(error)) => {
                self.report_error(
                    i18n::t!(
                        "Could not create the MR branch\n{reason}",
                        reason = format!("{error:#}")
                    ),
                    cx,
                );
            }
            Err(_) => {}
        }
    }

    /// The rest of a build that stopped on a conflict, once the user has resolved it and
    /// continued the cherry-pick from the git panel.
    fn finish_pending(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.building {
            return;
        }
        let (Some(pending), Some(repository), Some(work_directory_abs_path)) = (
            self.pending.clone(),
            self.repository(cx),
            self.work_directory_abs_path(cx),
        ) else {
            return;
        };
        let continuation = MergeRequestContinuation {
            base_sha: pending.base_sha.clone(),
            merge_request_branch: pending.merge_request_branch.clone(),
            message: pending.message.clone(),
            recreate: pending.recreate,
            origin_branch: pending.origin_branch.clone(),
            scratch_branch: pending.scratch_branch.clone(),
        };
        let receiver = repository.update(cx, |repository, cx| {
            repository.finish_merge_request_branch(continuation, cx)
        });
        self.building = true;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = receiver.await;
            this.update_in(cx, |this, window, cx| {
                this.building = false;
                match result {
                    Ok(Ok(MergeRequestBuildOutcome::Created {
                        merge_request_sha,
                        previous_sha,
                        return_failure,
                    })) => {
                        // The merge-request branch is made either way, so "続きの手順" is done;
                        // only the way back to the origin branch may be left.
                        if return_failure.is_none() {
                            forget_origin(&work_directory_abs_path, &pending.scratch_branch, cx);
                        } else {
                            this.stranded = Some(StrandedBuild {
                                scratch_branch: pending.scratch_branch.clone(),
                                origin_branch: pending.origin_branch.clone(),
                                merge_request_branch: pending.merge_request_branch.clone(),
                                merge_request_sha,
                            });
                        }
                        forget_pending(
                            &this.storage_location(&work_directory_abs_path),
                            &pending.scratch_branch,
                            cx,
                        );
                        this.pending = None;
                        this.report_created(
                            repository.clone(),
                            &pending.merge_request_branch,
                            previous_sha,
                            cx,
                        );
                        if let Some(failure) = return_failure {
                            this.report_error(
                                i18n::t!(
                                    "Created the MR branch {}, but could not go back to {} (\"Go Back\" in the MR box returns you).\n{failure}",
                                    pending.merge_request_branch, pending.origin_branch
                                ),
                                cx,
                            );
                        }
                        let excluded = pending.excluded_oids();
                        this.open_comparison(
                            repository,
                            pending.base,
                            pending.development_branch,
                            pending.merge_request_branch,
                            excluded,
                            window,
                            cx,
                        );
                    }
                    Ok(Ok(MergeRequestBuildOutcome::Refused { reason, .. })) => {
                        this.report_error(
                            i18n::t!(
                                "Could not update the MR branch. Still on the scratch branch \"{}\".\n{reason}",
                                pending.scratch_branch
                            ),
                            cx,
                        );
                    }
                    Ok(Ok(MergeRequestBuildOutcome::Stopped { reason, .. })) => {
                        this.report_error(reason, cx);
                    }
                    Ok(Err(error)) => {
                        this.report_error(
                            i18n::t!(
                                "Could not update the MR branch\n{reason}",
                                reason = format!("{error:#}")
                            ),
                            cx,
                        );
                    }
                    Err(_) => {}
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Gives up on a stopped build: back to the branch it started from, with the scratch branch
    /// (and whatever was resolved on it) thrown away.
    fn abandon_pending(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(pending), Some(repository), Some(work_directory_abs_path)) = (
            self.pending.clone(),
            self.repository(cx),
            self.work_directory_abs_path(cx),
        ) else {
            return;
        };
        let detail = i18n::t!(
            "Deletes the scratch branch \"{}\" and returns to \"{}\". The commits on the scratch branch (conflict resolutions included) are thrown away (they stay in the reflog).\n\
             If a cherry-pick is still stopped, press \"Abort\" in the Git panel's banner first.",
            pending.scratch_branch,
            pending.origin_branch
        );
        let confirmation = window.prompt(
            PromptLevel::Warning,
            i18n::t!("Stop creating the MR branch?"),
            Some(&detail),
            &[i18n::t!("Stop and Go Back"), i18n::t!("Cancel")],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if confirmation.await.unwrap_or(1) != 0 {
                return;
            }
            let Ok(receiver) = cx.update(|_, cx| {
                repository.update(cx, |repository, cx| {
                    repository.leave_rewrite_scratch_branch(
                        pending.origin_branch.clone(),
                        pending.scratch_branch.clone(),
                        cx,
                    )
                })
            }) else {
                return;
            };
            let result = receiver.await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(Ok(output)) if output.succeeded() => {
                        forget_origin(&work_directory_abs_path, &pending.scratch_branch, cx);
                        forget_pending(
                            &this.storage_location(&work_directory_abs_path),
                            &pending.scratch_branch,
                            cx,
                        );
                        this.pending = None;
                        this.report_info(
                            i18n::t!(
                                "Went back to {} and deleted the scratch branch \"{}\".",
                                pending.origin_branch,
                                pending.scratch_branch
                            ),
                            cx,
                        );
                    }
                    Ok(Ok(output)) => this.report_error(
                        i18n::t!(
                            "Could not go back to {}. Still on the scratch branch \"{}\".\n{}",
                            pending.origin_branch,
                            pending.scratch_branch,
                            output.failure_message()
                        ),
                        cx,
                    ),
                    Ok(Err(error)) => this.report_error(format!("{error:#}"), cx),
                    Err(_) => {}
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Takes the user back from the scratch branch of a build that made the merge-request branch
    /// but could not switch back on its own.
    fn leave_stranded(&mut self, cx: &mut Context<Self>) {
        let (Some(stranded), Some(repository), Some(work_directory_abs_path)) = (
            self.stranded.clone(),
            self.repository(cx),
            self.work_directory_abs_path(cx),
        ) else {
            return;
        };
        let head_sha = repository
            .read(cx)
            .head_commit
            .as_ref()
            .map(|commit| commit.sha.to_string());
        // `branch -D` is final, so anything committed on the scratch branch since would go with
        // it.
        if !can_leave_stranded(&stranded, head_sha.as_deref()) {
            self.report_error(
                i18n::t!(
                    "The scratch branch \"{}\" has moved away from the commit of the MR branch \"{}\", so it is not deleted. Check its contents, then switch to \"{}\" yourself.",
                    stranded.scratch_branch, stranded.merge_request_branch, stranded.origin_branch
                ),
                cx,
            );
            return;
        }
        let receiver = repository.update(cx, |repository, cx| {
            repository.leave_rewrite_scratch_branch(
                stranded.origin_branch.clone(),
                stranded.scratch_branch.clone(),
                cx,
            )
        });
        cx.spawn(async move |this, cx| {
            let result = receiver.await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(Ok(output)) if output.succeeded() => {
                        forget_origin(&work_directory_abs_path, &stranded.scratch_branch, cx);
                        this.stranded = None;
                        this.report_info(
                            i18n::t!(
                                "Went back to {} and deleted the scratch branch \"{}\".",
                                stranded.origin_branch,
                                stranded.scratch_branch
                            ),
                            cx,
                        );
                    }
                    Ok(Ok(output)) => this.report_error(
                        i18n::t!(
                            "Could not go back to {}. Still on the scratch branch \"{}\".\n{}",
                            stranded.origin_branch,
                            stranded.scratch_branch,
                            output.failure_message()
                        ),
                        cx,
                    ),
                    Ok(Err(error)) => this.report_error(format!("{error:#}"), cx),
                    Err(_) => {}
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn render_stranded(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let stranded = self.stranded.as_ref()?;
        if self.current_branch(cx).as_deref() != Some(stranded.scratch_branch.as_str()) {
            return None;
        }
        Some(
            v_flex()
                .w_full()
                .gap_1()
                .p_1p5()
                .rounded_md()
                .border_1()
                .border_color(cx.theme().status().warning_border)
                .bg(cx.theme().status().warning_background)
                .child(
                    Label::new(i18n::t!(
                        "Created the MR branch \"{}\", but still on the scratch branch \"{}\".",
                        stranded.merge_request_branch,
                        stranded.scratch_branch
                    ))
                    .size(LabelSize::Small),
                )
                .child(
                    Button::new(
                        "merge-request-box-leave-stranded",
                        i18n::t!(
                            "Go Back to \"{}\" (Delete the Scratch Branch)",
                            stranded.origin_branch
                        ),
                    )
                    .style(ButtonStyle::Filled)
                    .label_size(LabelSize::Small)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.leave_stranded(cx);
                    })),
                )
                .into_any_element(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn open_comparison(
        &self,
        repository: Entity<Repository>,
        base: String,
        development_branch: String,
        merge_request_branch: String,
        excluded: Vec<Oid>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.workspace
            .update(cx, |workspace, cx| {
                RefComparison::open_merge_request_check(
                    workspace,
                    repository,
                    base.into(),
                    development_branch.into(),
                    merge_request_branch.into(),
                    excluded,
                    window,
                    cx,
                );
            })
            .ok();
    }

    fn report_created(
        &self,
        repository: Entity<Repository>,
        merge_request_branch: &str,
        previous_sha: Option<String>,
        cx: &mut App,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let message = match &previous_sha {
            Some(previous_sha) => i18n::t!(
                "Recreated the MR branch {merge_request_branch}\nPrevious MR branch: {} (recoverable from the reflog)",
                short_sha(previous_sha)
            ),
            None => i18n::t!("Created the MR branch {merge_request_branch}"),
        };
        let force_with_lease = previous_sha.is_some();
        let push_label = if force_with_lease {
            i18n::t!("Force Push (with lease)")
        } else {
            i18n::t!("Push")
        };
        let branch_name = SharedString::from(merge_request_branch.to_string());
        let workspace_handle = workspace.downgrade();
        workspace.update(cx, |workspace, cx| {
            let toast = StatusToast::new(message, cx, move |this, _cx| {
                this.icon(
                    Icon::new(IconName::GitBranch)
                        .size(IconSize::Small)
                        .color(Color::Muted),
                )
                .auto_dismiss(false)
                .action(push_label, move |window, cx| {
                    let Some(panel) = workspace_handle
                        .upgrade()
                        .and_then(|workspace| workspace.read(cx).panel::<GitPanel>(cx))
                    else {
                        return;
                    };
                    panel.update(cx, |panel, cx| {
                        panel.push_branch(
                            repository.clone(),
                            branch_name.clone(),
                            force_with_lease,
                            window,
                            cx,
                        );
                    });
                })
                .dismiss_button(true)
            });
            workspace.toggle_status_toast(toast, cx);
        });
    }

    fn report_error(&self, message: String, cx: &mut App) {
        self.report(message, IconName::XCircle, Color::Error, cx);
    }

    fn report_info(&self, message: String, cx: &mut App) {
        self.report(message, IconName::GitBranch, Color::Muted, cx);
    }

    fn report(&self, message: String, icon: IconName, color: Color, cx: &mut App) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            let toast = StatusToast::new(message, cx, move |this, _cx| {
                this.icon(Icon::new(icon).size(IconSize::Small).color(color))
                    .dismiss_button(true)
            });
            workspace.toggle_status_toast(toast, cx);
        });
    }

    /// Enter in the message field is a new line, and anywhere else in the box it does nothing:
    /// the graph around the box would otherwise take it as "open the selected commit".
    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if self.message_editor.focus_handle(cx).is_focused(window) {
            self.message_editor.update(cx, |editor, cx| {
                editor.newline(&editor::actions::Newline, window, cx);
            });
        }
    }

    fn render_ref_picker(
        &self,
        id: &'static str,
        label: &'static str,
        value: Option<SharedString>,
        tooltip: &'static str,
        on_click: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        h_flex()
            .w_full()
            .gap_1()
            .child(
                div()
                    .w(rems(5.))
                    .flex_none()
                    .child(Label::new(label).size(LabelSize::Small).color(Color::Muted)),
            )
            .child(
                Button::new(
                    id,
                    value.unwrap_or_else(|| i18n::t!("(not selected)").into()),
                )
                .label_size(LabelSize::Small)
                .end_icon(
                    Icon::new(IconName::ChevronDown)
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                )
                .tooltip(Tooltip::text(tooltip))
                .on_click(cx.listener(move |this, _, window, cx| on_click(this, window, cx))),
            )
    }

    fn render_placed(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let is_empty = self.state.placed.is_empty();
        v_flex()
            .id("merge-request-box-drop")
            .debug_selector(|| "merge-request-box-drop".into())
            .w_full()
            .min_h(rems(4.))
            .p_1p5()
            .gap_1()
            .rounded_md()
            .border_2()
            .border_dashed()
            .border_color(colors.border)
            .drag_over::<DraggedCommit>(|style, _, _, cx| {
                style
                    .bg(cx.theme().colors().drop_target_background)
                    .border_color(cx.theme().colors().border_focused)
            })
            .on_drop(cx.listener(|this, dragged: &DraggedCommit, window, cx| {
                this.drop_commits(dragged, window, cx);
            }))
            .when(is_empty, |this| {
                this.justify_center().items_center().child(
                    Label::new(i18n::t!(
                        "Drag commits here (a multiple selection moves together)"
                    ))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
            })
            .children(self.state.placed.iter().map(|placed| {
                let sha = placed.sha;
                let outside = self.state.is_outside_range(&sha);
                h_flex()
                    .id(SharedString::from(format!("merge-request-box-card-{sha}")))
                    .debug_selector(|| format!("merge-request-box-card-{sha}"))
                    .w_full()
                    .gap_1()
                    .px_1()
                    .py_0p5()
                    .rounded_sm()
                    .bg(colors.element_background)
                    .border_1()
                    .border_color(colors.border_variant)
                    .child(
                        Label::new(sha.display_short())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .buffer_font(cx),
                    )
                    .child(
                        div().flex_1().min_w_0().child(
                            Label::new(placed.subject.clone())
                                .size(LabelSize::Small)
                                .truncate(),
                        ),
                    )
                    .when(outside, |this| {
                        this.child(
                            Label::new(i18n::t!("Outside the development branch"))
                                .size(LabelSize::XSmall)
                                .color(Color::Warning),
                        )
                    })
                    .child(
                        IconButton::new(
                            SharedString::from(format!("merge-request-box-remove-{sha}")),
                            IconName::Close,
                        )
                        .icon_size(IconSize::XSmall)
                        .tooltip(Tooltip::text(i18n::t!("Take Out of the Box")))
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.remove(sha, window, cx);
                            },
                        )),
                    )
            }))
    }

    fn render_range(&self, cx: &mut Context<Self>) -> AnyElement {
        match &self.range_status {
            RangeStatus::Failed(error) => {
                return Label::new(error.clone())
                    .size(LabelSize::Small)
                    .color(Color::Error)
                    .into_any_element();
            }
            RangeStatus::Idle if self.state.range.is_none() => {
                return Label::new(i18n::t!(
                    "Choose a development branch and a base to check for left-out commits."
                ))
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element();
            }
            _ => {}
        }
        let Some(range) = &self.state.range else {
            return Label::new(i18n::t!("Loading the development branch's commits…"))
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element();
        };
        let unplaced = self.state.unplaced();
        let excluded = self.state.excluded_in_range();
        let merge_count = self.state.merge_count();
        let has_commits = range.commits.iter().any(|commit| !commit.is_merge);

        v_flex()
            .debug_selector(|| "merge-request-box-range".into())
            .w_full()
            .gap_1()
            .when(range.truncated, |this| {
                this.child(
                    Label::new(i18n::t!(
                        "The development branch's range has more than {MAX_RANGE_COMMITS} commits. Check that the base is right (older commits are not shown)."
                    ))
                    .size(LabelSize::Small)
                    .color(Color::Warning),
                )
            })
            .map(|this| {
                if !has_commits {
                    this.child(
                        Label::new(i18n::t!("The development branch has no commits newer than the base."))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                } else if unplaced.is_empty() && !self.state.everything_checked() {
                    this.child(
                        Label::new(i18n::t!(
                            "The newest {MAX_RANGE_COMMITS} commits are all placed or excluded, but older commits were not checked."
                        ))
                        .size(LabelSize::Small)
                        .color(Color::Warning),
                    )
                } else if unplaced.is_empty() {
                    this.child(
                        Label::new(i18n::t!(
                            "Every commit of the development branch is checked ({} placed / {} excluded)",
                            self.state.placed_in_range_count(),
                            excluded.len()
                        ))
                        .size(LabelSize::Small)
                        .color(Color::Success),
                    )
                } else {
                    // The graph marks each unplaced row itself, so the box only counts them and
                    // says where they are.
                    this.child(
                        h_flex()
                            .w_full()
                            .gap_1()
                            .child(
                                div().flex_1().min_w_0().child(
                                    Label::new(i18n::tn!(unplaced.len(), "{count} unplaced commit", "{count} unplaced commits"))
                                        .size(LabelSize::Small)
                                        .color(Color::Warning),
                                ),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .debug_selector(|| "merge-request-box-first-unplaced".into())
                                    .child(
                                        Button::new(
                                            "merge-request-box-first-unplaced",
                                            i18n::t!("Go to First Unplaced"),
                                        )
                                        .label_size(LabelSize::XSmall)
                                        .tooltip(Tooltip::text(i18n::t!(
                                            "Selects the oldest unplaced commit in the graph"
                                        )))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.select_first_unplaced(cx);
                                        })),
                                    ),
                            ),
                    )
                    .child(
                        Label::new(i18n::t!(
                            "They are the rows with a small ● in the graph. Drag them into the box, or set them aside with \"Exclude from MR\" in the right-click menu."
                        ))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                    )
                }
            })
            .when(merge_count > 0, |this| {
                this.child(
                    Label::new(i18n::tn!(merge_count, "{count} merge commit (not included)", "{count} merge commits (not included)"))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
            .when(!excluded.is_empty(), |this| {
                let expanded = self.excluded_expanded;
                this.child(
                    Button::new(
                        "merge-request-box-excluded-toggle",
                        i18n::t!("{} Excluded", excluded.len()),
                    )
                    .label_size(LabelSize::Small)
                    .start_icon(
                        Icon::new(if expanded {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.excluded_expanded = !this.excluded_expanded;
                        cx.notify();
                    })),
                )
                .when(expanded, |this| {
                    this.children(excluded.iter().map(|commit| {
                        let sha = commit.sha;
                        self.render_commit_row(commit, cx).child(
                            Button::new(
                                SharedString::from(format!("merge-request-box-restore-{sha}")),
                                i18n::t!("Put Back"),
                            )
                            .label_size(LabelSize::XSmall)
                            .tooltip(Tooltip::text(i18n::t!("Stop Excluding and Mark as Unplaced")))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.restore_commits(&[sha], window, cx);
                            })),
                        )
                    }))
                })
            })
            .into_any_element()
    }

    fn render_commit_row(&self, commit: &RangeCommit, cx: &App) -> Stateful<Div> {
        h_flex()
            .id(SharedString::from(format!(
                "merge-request-box-row-{}",
                commit.sha
            )))
            .w_full()
            .gap_1()
            .child(
                Label::new(commit.sha.display_short())
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
                    .buffer_font(cx),
            )
            .child(
                div().flex_1().min_w_0().child(
                    Label::new(commit.subject.clone())
                        .size(LabelSize::Small)
                        .truncate(),
                ),
            )
    }

    fn render_labelled_editor(
        &self,
        selector: &'static str,
        label: &'static str,
        editor: Entity<Editor>,
        cx: &App,
    ) -> impl IntoElement {
        v_flex()
            .w_full()
            .gap_0p5()
            .child(Label::new(label).size(LabelSize::Small))
            .child(
                div()
                    .debug_selector(|| selector.into())
                    .w_full()
                    .px_1p5()
                    .py_1()
                    .border_1()
                    .border_color(cx.theme().colors().border_variant)
                    .rounded_md()
                    .bg(cx.theme().colors().editor_background)
                    .child(editor),
            )
    }

    fn render_notice(notice: &Option<Notice>) -> Option<impl IntoElement> {
        notice.as_ref().map(|notice| {
            Label::new(notice.text.clone())
                .size(LabelSize::Small)
                .color(notice.color)
        })
    }

    fn render_pending(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let pending = self.pending.as_ref()?;
        if self.current_branch(cx).as_deref() != Some(pending.scratch_branch.as_str()) {
            return None;
        }
        Some(
            v_flex()
                .w_full()
                .gap_1()
                .p_1p5()
                .rounded_md()
                .border_1()
                .border_color(cx.theme().status().warning_border)
                .bg(cx.theme().status().warning_background)
                .child(
                    Label::new(i18n::t!(
                        "The MR branch \"{}\" is half created (on the scratch branch \"{}\").",
                        pending.merge_request_branch, pending.scratch_branch
                    ))
                    .size(LabelSize::Small),
                )
                .child(
                    Label::new(i18n::t!(
                        "After resolving the conflict and pressing \"Continue\" in the Git panel's banner, \"Next Steps\" updates the MR branch and goes back to \"{}\".",
                        pending.origin_branch
                    ))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
                )
                .child(
                    h_flex()
                        .gap_1()
                        .child(
                            Button::new(
                                "merge-request-box-finish",
                                i18n::t!("Next Steps (Update the MR Branch and Go Back)"),
                            )
                            .style(ButtonStyle::Filled)
                            .label_size(LabelSize::Small)
                            .disabled(self.building)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.finish_pending(window, cx);
                            })),
                        )
                        .child(
                            Button::new("merge-request-box-abandon", i18n::t!("Stop and Go Back…"))
                                .label_size(LabelSize::Small)
                                .disabled(self.building)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.abandon_pending(window, cx);
                                })),
                        ),
                )
                .into_any_element(),
        )
    }
}

impl Render for MergeRequestBox {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let branches = self.branches(cx);
        let stale_base = self
            .base
            .as_deref()
            .and_then(|base| stale_base_warning(base, &branches));
        let branch_name = self.branch_name(cx);
        let exists = !branch_name.is_empty()
            && branches
                .iter()
                .any(|branch| !branch.is_remote() && branch.name() == branch_name);
        let can_create =
            !self.state.placed.is_empty() && !self.building && (!exists || self.recreate);
        let recreate = self.recreate;
        let entity = cx.weak_entity();

        // Directly inside the scrolling body, every part would be a flex item shrunk to fit it, and
        // a full box's cards would be drawn over what follows the frame.
        let content = v_flex()
            .w_full()
            .p_2()
            .gap_2()
            .children(self.render_stranded(cx))
            .children(self.render_pending(cx))
            .child(self.render_ref_picker(
                "merge-request-box-development-branch",
                i18n::t!("Development Branch"),
                self.development_branch.clone(),
                i18n::t!(
                    "Where the commits come from. Used to check for left-out commits and for the final comparison"
                ),
                |this, window, cx| this.pick_development_branch(window, cx),
                cx,
            ))
            .child(self.render_ref_picker(
                "merge-request-box-base",
                i18n::tc!("merge request box", "Base"),
                self.base.clone(),
                i18n::t!("Where the MR goes. The MR branch is created from here"),
                |this, window, cx| this.pick_base(window, cx),
                cx,
            ))
            .children(stale_base.map(|warning| {
                Label::new(warning)
                    .size(LabelSize::Small)
                    .color(Color::Warning)
            }))
            .child(self.render_placed(cx))
            .children(Self::render_notice(&self.box_notice))
            .child(self.render_range(cx))
            .child(self.render_labelled_editor(
                "merge-request-box-message",
                i18n::tc!("merge request box", "Message"),
                self.message_editor.clone(),
                cx,
            ))
            .child(self.render_labelled_editor(
                "merge-request-box-branch-name",
                i18n::t!("MR Branch Name"),
                self.branch_name_editor.clone(),
                cx,
            ))
            .when(exists, |this| {
                this.child(
                    Checkbox::new(
                        "merge-request-box-recreate",
                        if recreate {
                            ToggleState::Selected
                        } else {
                            ToggleState::Unselected
                        },
                    )
                    .label(i18n::t!(
                        "Recreate the existing {branch_name} (needs a force push)"
                    ))
                    .label_size(LabelSize::Small)
                    .on_click(move |state, _, cx| {
                        let selected = *state == ToggleState::Selected;
                        entity
                            .update(cx, |this, cx| {
                                this.recreate = selected;
                                this.create_notice = None;
                                cx.notify();
                            })
                            .ok();
                    }),
                )
            })
            .children(Self::render_notice(&self.create_notice))
            .child(
                h_flex().w_full().justify_end().child(
                    Button::new("merge-request-box-create", i18n::t!("Create and Compare"))
                        .style(ButtonStyle::Filled)
                        .disabled(!can_create)
                        .tooltip(Tooltip::text(i18n::t!(
                            "Creates an MR branch that squashes the box's commits into one on top of the base, and compares it with the development branch"
                        )))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.create(window, cx);
                        })),
                ),
            );

        v_flex()
            .key_context("MergeRequestBox")
            .debug_selector(|| "merge-request-box".into())
            .track_focus(&self.focus_handle)
            .size_full()
            .on_action(cx.listener(Self::confirm))
            .child(
                h_flex()
                    .w_full()
                    .px_2()
                    .py_1()
                    .gap_1()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(
                        Icon::new(IconName::GitBranch)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(Label::new(i18n::t!("MR Box")))
                    .child(div().flex_1())
                    .child(crate::git_guide::guide_button(
                        "merge-request-box-guide",
                        Some(crate::git_guide::MERGE_REQUEST_TOPIC),
                    ))
                    .child(
                        IconButton::new("merge-request-box-close", IconName::Close)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text(i18n::t!(
                                "Close the Box (Its Contents Are Kept)"
                            )))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.graph
                                    .update(cx, |graph, cx| graph.close_merge_request_box(cx))
                                    .ok();
                            })),
                    ),
            )
            .child(
                div()
                    .id("merge-request-box-body")
                    .debug_selector(|| "merge-request-box-body".into())
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .overflow_y_scroll()
                    .child(content),
            )
    }
}

fn short_sha(sha: &str) -> String {
    sha.chars().take(SHORT_SHA_LENGTH).collect()
}

fn read_pending(
    location: &str,
    scratch_branch: &str,
    cx: &App,
) -> Task<Option<PendingMergeRequest>> {
    let key = pending_key(location, scratch_branch);
    let store = KeyValueStore::global(cx);
    cx.background_spawn(async move {
        store
            .read_kvp(&key)
            .log_err()
            .flatten()
            .and_then(|json| serde_json::from_str(&json).log_err())
    })
}

fn write_pending(location: &str, pending: &PendingMergeRequest, cx: &App) {
    let key = pending_key(location, &pending.scratch_branch);
    let Some(json) = serde_json::to_string(pending).log_err() else {
        return;
    };
    let store = KeyValueStore::global(cx);
    cx.background_spawn(async move {
        store.write_kvp(key, json).await.log_err();
    })
    .detach();
}

/// Drops what a stopped build left for the box to finish, once its scratch branch is gone.
pub(crate) fn forget_pending(location: &str, scratch_branch: &str, cx: &App) {
    let key = pending_key(location, scratch_branch);
    let store = KeyValueStore::global(cx);
    cx.background_spawn(async move {
        store.delete_kvp(key).await.log_err();
    })
    .detach();
}

fn forget_origin(work_directory_abs_path: &Path, scratch_branch: &str, cx: &App) {
    rewrite_range_modal::forget_rewrite_origin(work_directory_abs_path, scratch_branch, cx)
        .detach();
}

/// `git::ToggleMergeRequestBox` and `git::OpenMergeRequestBox`: the box of the commit graph the
/// user is looking at, or of the active repository's graph, opened first when there is none.
pub(crate) fn register(workspace: &mut Workspace) {
    workspace.register_action(|workspace, _: &git::ToggleMergeRequestBox, window, cx| {
        let Some(graph) = graph_for_merge_request_box(workspace, window, cx) else {
            return;
        };
        // The box reports through the workspace, which is still leased to this action handler.
        window.defer(cx, move |window, cx| {
            graph.update(cx, |graph, cx| graph.toggle_merge_request_box(window, cx));
        });
    });
    // What the Git menu and the guide name "MR を作る": a box that is already open stays open.
    workspace.register_action(|workspace, _: &git::OpenMergeRequestBox, window, cx| {
        let Some(graph) = graph_for_merge_request_box(workspace, window, cx) else {
            return;
        };
        window.defer(cx, move |window, cx| {
            graph.update(cx, |graph, cx| {
                graph.open_merge_request_box(window, cx);
            });
        });
    });
}

/// The graph whose box the two actions act on, brought forward (or opened) in the workspace.
fn graph_for_merge_request_box(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Option<Entity<GitGraph>> {
    let graph = workspace
        .active_item_as::<GitGraph>(cx)
        .filter(|graph| graph.read(cx).can_hold_merge_request_box());
    let graph = match graph {
        Some(graph) => graph,
        None => {
            let repository = workspace.project().read(cx).active_repository(cx)?;
            let repo_id = repository.read(cx).id;
            let existing = workspace.items_of_type::<GitGraph>(cx).find(|graph| {
                let graph = graph.read(cx);
                graph.repository_id() == repo_id && graph.can_hold_merge_request_box()
            });
            match existing {
                Some(graph) => {
                    workspace.activate_item(&graph, true, true, window, cx);
                    graph
                }
                None => {
                    let git_store = workspace.project().read(cx).git_store().clone();
                    crate::git_graph::open_or_reuse_graph(
                        workspace,
                        repo_id,
                        git_store,
                        LogSource::All,
                        None,
                        window,
                        cx,
                    );
                    workspace.items_of_type::<GitGraph>(cx).find(|graph| {
                        let graph = graph.read(cx);
                        graph.repository_id() == repo_id && graph.can_hold_merge_request_box()
                    })?
                }
            }
        }
    };
    Some(graph)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;
    use git::repository::{
        CommitSummary, InitialGraphCommitData, Upstream, UpstreamTrackingStatus,
    };
    use gpui::{Modifiers, MouseButton, TestAppContext, VisualTestContext, point};
    use project::Project;
    use util::path;

    fn oid(byte: u8) -> Oid {
        Oid::from_bytes(&[byte; 20]).unwrap()
    }

    fn range_commit(byte: u8, is_merge: bool) -> RangeCommit {
        RangeCommit {
            sha: oid(byte),
            subject: format!("件名 {byte}").into(),
            message: format!("件名 {byte}\n\n本文 {byte}").into(),
            is_merge,
        }
    }

    fn range_candidate(commit: &RangeCommit) -> MergeRequestCandidate {
        MergeRequestCandidate {
            sha: commit.sha,
            subject: commit.subject.clone(),
            message: commit.message.clone(),
            is_merge: commit.is_merge,
        }
    }

    fn candidate(byte: u8) -> MergeRequestCandidate {
        MergeRequestCandidate {
            sha: oid(byte),
            subject: format!("外の件名 {byte}").into(),
            message: SharedString::default(),
            is_merge: false,
        }
    }

    /// Commits 1..=5 oldest first, 4 being a merge.
    fn state_with_range() -> MergeRequestBoxState {
        let mut state = MergeRequestBoxState::default();
        state.set_range(
            DevelopmentRange {
                commits: vec![
                    range_commit(1, false),
                    range_commit(2, false),
                    range_commit(3, false),
                    range_commit(4, true),
                    range_commit(5, false),
                ],
                truncated: false,
            },
            ("origin/main".to_string(), "dev".to_string()),
        );
        state
    }

    fn placed(state: &MergeRequestBoxState) -> Vec<Oid> {
        state.placed.iter().map(|placed| placed.sha).collect()
    }

    /// A commit from outside the development range keeps what the graph knew when it was added
    /// (a short SHA while the graph was still loading it) until git says more; the range's own
    /// commits are left to the range.
    #[test]
    fn test_outside_commits_take_what_git_says() {
        let mut state = state_with_range();
        state.add([range_candidate(&range_commit(1, false)), candidate(9)]);
        assert_eq!(state.outside_range_shas(), vec![oid(9)]);
        assert_eq!(
            outside_commit_args(&[oid(9)]),
            [
                "log".to_string(),
                "--no-walk=unsorted".to_string(),
                "-z".to_string(),
                "--format=%H%x1f%P%x1f%s%x1f%B".to_string(),
                oid(9).to_string(),
            ]
        );
        git::repository::validate_git_command(&outside_commit_args(&[oid(9)])).unwrap();

        let told = RangeCommit {
            sha: oid(9),
            subject: "本当の件名 9".into(),
            message: "本当の件名 9\n\n本文 9".into(),
            is_merge: false,
        };
        let not_for_the_range = RangeCommit {
            subject: "別の件名 1".into(),
            ..range_commit(1, false)
        };
        assert!(state.update_outside_commits(&[told.clone(), not_for_the_range]));
        assert_eq!(state.placed[0].subject.as_ref(), "件名 1");
        assert_eq!(state.placed[1].subject.as_ref(), "本当の件名 9");
        assert_eq!(state.message, "件名 1\n\n本文 1\n本当の件名 9");
        assert!(
            !state.update_outside_commits(&[told]),
            "nothing new, nothing to save"
        );
    }

    /// A range read only in part is dropped whole: nothing is marked or listed from it.
    #[test]
    fn test_cleared_range_marks_and_lists_nothing() {
        let mut state = state_with_range();
        state.add([range_candidate(&range_commit(1, false))]);
        state.exclude(&oid(2));
        state.clear_range();
        assert!(state.unplaced().is_empty());
        assert!(!state.everything_checked());
        assert!(!state.is_outside_range(&oid(1)));
        assert_eq!(state.outside_range_shas(), vec![oid(1)]);
        assert_eq!(placed(&state), vec![oid(1)], "the box keeps what is in it");
        assert_eq!(state.row_marks().mark(&oid(3)), None);
        assert!(
            !state.can_exclude(&oid(3)),
            "without a range there is nothing a commit could be missing from"
        );
    }

    /// The same path on this machine and on a server keeps two boxes; a local path's key is what
    /// it was before remote machines were told apart.
    #[test]
    fn test_storage_location_tells_machines_apart() {
        let path = Path::new("/home/me/repo");
        let local = merge_request_storage_location(path, None);
        let remote = merge_request_storage_location(path, Some("ssh:me@server:22"));
        assert_eq!(
            box_key(&local, "dev"),
            "git-merge-request-box:/home/me/repo:dev"
        );
        assert_eq!(
            box_key(&remote, "dev"),
            "git-merge-request-box:@ssh:me@server:22:/home/me/repo:dev"
        );
        assert_ne!(pending_key(&local, "x"), pending_key(&remote, "x"));
        assert_ne!(
            merge_request_storage_location(path, Some("ssh:me@other:22")),
            remote
        );
    }

    #[test]
    fn test_add_single_several_duplicates_merges_and_outsiders() {
        let mut state = state_with_range();

        let outcome = state.add([candidate(3)]);
        assert_eq!(
            outcome,
            AddOutcome {
                added: 1,
                ..Default::default()
            }
        );
        assert_eq!(
            state.placed[0].subject.as_ref(),
            "件名 3",
            "a commit of the range takes what the range says about it"
        );

        let outcome = state.add([candidate(1), candidate(3), candidate(4), candidate(9)]);
        assert_eq!(
            outcome,
            AddOutcome {
                added: 2,
                already_placed: 1,
                merges: 1,
            }
        );
        assert_eq!(placed(&state), vec![oid(1), oid(3), oid(9)]);
        assert!(state.is_outside_range(&oid(9)));
        assert!(!state.is_outside_range(&oid(1)));
        assert_eq!(state.placed[2].subject.as_ref(), "外の件名 9");

        let merge_outside = MergeRequestCandidate {
            is_merge: true,
            ..candidate(8)
        };
        assert_eq!(state.add([merge_outside]).merges, 1);
        assert!(!state.is_placed(&oid(8)));
        assert!(
            outcome
                .notice()
                .is_some_and(|notice| notice.contains("Merge commits")),
            "a refused merge is said out loud"
        );
    }

    /// Dropped in any order, the box replays the development branch's order; commits from
    /// elsewhere go last, in the order they came.
    #[test]
    fn test_placed_follow_the_development_history() {
        let mut state = state_with_range();
        state.add([candidate(9), candidate(5)]);
        state.add([candidate(8), candidate(2), candidate(1)]);
        assert_eq!(placed(&state), vec![oid(1), oid(2), oid(5), oid(9), oid(8)]);
        assert_eq!(
            state.placed_shas_oldest_first(),
            [1u8, 2, 5, 9, 8]
                .iter()
                .map(|byte| oid(*byte).to_string())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_exclude_restore_and_unplaced() {
        let mut state = state_with_range();
        assert_eq!(
            state
                .unplaced()
                .iter()
                .map(|commit| commit.sha)
                .collect::<Vec<_>>(),
            vec![oid(1), oid(2), oid(3), oid(5)],
            "the merge is counted apart, not listed as unplaced"
        );
        assert_eq!(state.merge_count(), 1);

        state.add([candidate(1)]);
        state.exclude(&oid(2));
        assert_eq!(
            state
                .unplaced()
                .iter()
                .map(|commit| commit.sha)
                .collect::<Vec<_>>(),
            vec![oid(3), oid(5)]
        );
        assert_eq!(state.excluded_shas(), vec![oid(2)]);

        state.restore(&oid(2));
        assert!(state.excluded_shas().is_empty());
        assert_eq!(state.unplaced().len(), 3);

        // Excluding a placed commit takes it out of the box, and putting an excluded commit in
        // the box takes it off the excluded list.
        state.exclude(&oid(1));
        assert!(!state.is_placed(&oid(1)));
        assert_eq!(state.excluded_shas(), vec![oid(1)]);
        state.add([candidate(1)]);
        assert!(state.excluded_shas().is_empty());

        assert!(state.can_exclude(&oid(3)), "unplaced");
        assert!(state.can_exclude(&oid(1)), "placed, from the range");
        assert!(!state.can_exclude(&oid(4)), "a merge is never counted");
        state.add([candidate(9)]);
        assert!(!state.can_exclude(&oid(9)), "never missing from the box");
        state.remove(&oid(9));

        state.add([candidate(2), candidate(3)]);
        state.exclude(&oid(5));
        assert!(!state.can_exclude(&oid(5)), "already excluded");
        assert!(state.unplaced().is_empty());
        assert_eq!(state.placed_in_range_count(), 3);

        let marks = state.row_marks();
        assert_eq!(marks.mark(&oid(1)), Some(RowMark::Placed));
        assert_eq!(marks.mark(&oid(5)), Some(RowMark::Excluded));
        assert_eq!(marks.mark(&oid(4)), None, "a merge is neither");
        state.remove(&oid(3));
        assert_eq!(state.row_marks().mark(&oid(3)), Some(RowMark::Unplaced));
    }

    #[test]
    fn test_message_follows_the_box_until_edited() {
        let mut state = state_with_range();
        state.add([candidate(2)]);
        assert_eq!(state.message, "件名 2\n\n本文 2");
        state.add([candidate(1), candidate(9)]);
        assert_eq!(
            state.message, "件名 1\n\n本文 1\n件名 2\n外の件名 9",
            "the oldest commit's whole message, then the subjects after it"
        );

        assert!(!state.set_message_from_editor(state.message.clone()));
        assert!(!state.message_edited, "the box's own text is not an edit");

        assert!(state.set_message_from_editor("自分で書いた".to_string()));
        state.add([candidate(3)]);
        state.remove(&oid(1));
        assert_eq!(state.message, "自分で書いた");

        let mut empty = MergeRequestBoxState::default();
        empty.add([candidate(7)]);
        assert_eq!(
            empty.message, "外の件名 7",
            "without a message the subject stands in"
        );
    }

    /// Saved and read back, then the development branch is rewritten: the commits that went with
    /// the rewrite leave the box, commits from outside stay, and choosing another base never
    /// takes anything out.
    #[test]
    fn test_saved_box_is_restored_and_pruned_after_a_rewrite() {
        let mut state = state_with_range();
        state.add([candidate(1), candidate(2), candidate(9)]);
        state.exclude(&oid(3));
        state.set_message_from_editor("MR".to_string());

        let saved = state.to_saved(Some("origin/main"), Some("mr/dev"));
        let json = serde_json::to_string(&saved).unwrap();
        let saved: SavedMergeRequestBox = serde_json::from_str(&json).unwrap();
        assert_eq!(saved.merge_request_branch.as_deref(), Some("mr/dev"));
        let mut restored = MergeRequestBoxState::from_saved(&saved, "dev");
        assert_eq!(placed(&restored), vec![oid(1), oid(2), oid(9)]);
        assert_eq!(restored.message, "MR");
        assert!(restored.message_edited);

        // The rewrite replaced 2 and 3 with 6.
        let rewritten = DevelopmentRange {
            commits: vec![range_commit(1, false), range_commit(6, false)],
            truncated: false,
        };
        let pruned = restored.set_range(rewritten, ("origin/main".to_string(), "dev".to_string()));
        assert_eq!(pruned, 1);
        assert_eq!(placed(&restored), vec![oid(1), oid(9)]);
        assert!(restored.excluded_shas().is_empty());
        assert_eq!(
            restored
                .unplaced()
                .iter()
                .map(|commit| commit.sha)
                .collect::<Vec<_>>(),
            vec![oid(6)]
        );

        // Another base measures another range; the box keeps what the user put in it.
        let mut state = state_with_range();
        state.add([candidate(1), candidate(2)]);
        let pruned = state.set_range(
            DevelopmentRange {
                commits: vec![range_commit(2, false)],
                truncated: false,
            },
            ("v1.0".to_string(), "dev".to_string()),
        );
        assert_eq!(pruned, 0);
        assert_eq!(placed(&state), vec![oid(2), oid(1)]);
        assert!(state.is_outside_range(&oid(1)));

        // A truncated read cannot tell a rewrite from the limit.
        let mut state = state_with_range();
        state.add([candidate(1)]);
        let pruned = state.set_range(
            DevelopmentRange {
                commits: vec![range_commit(5, false)],
                truncated: true,
            },
            ("origin/main".to_string(), "dev".to_string()),
        );
        assert_eq!(pruned, 0);
        assert!(state.is_placed(&oid(1)));
    }

    /// Commits past the read limit were never looked at, so a truncated range is never reported
    /// as fully checked, however much of what was read is placed or excluded.
    #[test]
    fn test_truncated_range_is_never_all_checked() {
        let mut state = state_with_range();
        assert!(!state.everything_checked());
        state.add([candidate(1), candidate(2), candidate(3)]);
        state.exclude(&oid(5));
        assert!(state.unplaced().is_empty());
        assert!(state.everything_checked());

        let mut truncated = state_with_range();
        truncated.set_range(
            DevelopmentRange {
                commits: vec![range_commit(5, false)],
                truncated: true,
            },
            ("origin/main".to_string(), "dev".to_string()),
        );
        truncated.exclude(&oid(5));
        assert!(truncated.unplaced().is_empty());
        assert!(
            !truncated.everything_checked(),
            "the commits past the limit are unchecked"
        );
        assert!(!MergeRequestBoxState::default().everything_checked());
    }

    /// The scratch branch of a build that could not switch back is only deleted while it is still
    /// at the merge request's commit.
    #[test]
    fn test_can_leave_stranded_only_at_the_merge_request_commit() {
        let stranded = StrandedBuild {
            scratch_branch: "zed-rewrite/mr-abcdef0".to_string(),
            origin_branch: "dev".to_string(),
            merge_request_branch: "mr/dev".to_string(),
            merge_request_sha: oid(1).to_string(),
        };
        assert!(can_leave_stranded(&stranded, Some(&oid(1).to_string())));
        assert!(!can_leave_stranded(&stranded, Some(&oid(2).to_string())));
        assert!(!can_leave_stranded(&stranded, None));
    }

    #[test]
    fn test_parse_development_range() {
        let record = |sha: Oid, parents: &str, subject: &str, body: &str| {
            format!("{sha}\x1f{parents}\x1f{subject}\x1f{body}\n\0")
        };
        let stdout = [
            record(oid(1), &oid(0).to_string(), "A", "A\n\n本文\x1fそのまま"),
            record(
                oid(2),
                &format!("{} {}", oid(1), oid(7)),
                "Merge branch 'x'",
                "Merge branch 'x'",
            ),
            record(oid(3), &oid(2).to_string(), "", ""),
            "壊れた行\0".to_string(),
        ]
        .concat();
        let range = parse_development_range(&stdout);
        assert!(!range.truncated);
        assert_eq!(
            range.commits,
            vec![
                RangeCommit {
                    sha: oid(1),
                    subject: "A".into(),
                    message: "A\n\n本文\x1fそのまま".into(),
                    is_merge: false,
                },
                RangeCommit {
                    sha: oid(2),
                    subject: "Merge branch 'x'".into(),
                    message: "Merge branch 'x'".into(),
                    is_merge: true,
                },
                RangeCommit {
                    sha: oid(3),
                    subject: "".into(),
                    message: "".into(),
                    is_merge: false,
                },
            ]
        );

        let many = (0..=MAX_RANGE_COMMITS)
            .map(|index| {
                let sha: Oid = format!("{index:040x}").parse().unwrap();
                record(sha, "", "x", "x")
            })
            .collect::<String>();
        let range = parse_development_range(&many);
        assert!(range.truncated);
        assert_eq!(range.commits.len(), MAX_RANGE_COMMITS);
        assert_eq!(
            range.commits.first().map(|commit| commit.sha),
            Some(format!("{:040x}", 1).parse().unwrap()),
            "the oldest one goes"
        );
        assert_eq!(
            development_range_args("origin/main", "dev")
                .last()
                .map(String::as_str),
            Some("origin/main..dev")
        );
        git::repository::validate_git_command(&development_range_args("origin/main", "dev"))
            .unwrap();
    }

    fn branch(name: &str, upstream: Option<(&str, u32)>) -> Branch {
        Branch {
            is_head: false,
            ref_name: format!("refs/heads/{name}").into(),
            upstream: upstream.map(|(remote, behind)| Upstream {
                ref_name: format!("refs/remotes/{remote}").into(),
                tracking: UpstreamTracking::Tracked(UpstreamTrackingStatus { ahead: 0, behind }),
            }),
            most_recent_commit: Some(CommitSummary {
                sha: "abc".into(),
                subject: "".into(),
                commit_timestamp: 0,
                author_name: "".into(),
                has_parent: true,
            }),
        }
    }

    #[test]
    fn test_preferred_base_and_stale_base_warning() {
        let branches = vec![
            branch("main", Some(("origin/main", 3))),
            branch("develop", None),
        ];
        assert_eq!(
            preferred_base(Some("main"), &branches).as_deref(),
            Some("origin/main"),
            "the merge request goes to the remote"
        );
        assert_eq!(
            preferred_base(Some("develop"), &branches).as_deref(),
            Some("develop")
        );
        assert_eq!(
            preferred_base(Some("origin/main"), &branches).as_deref(),
            Some("origin/main")
        );
        assert_eq!(preferred_base(None, &branches), None);

        let warning = stale_base_warning("main", &branches).unwrap();
        assert!(warning.contains("3 commits behind"), "{warning}");
        assert!(warning.contains("origin/main"), "{warning}");
        assert_eq!(stale_base_warning("origin/main", &branches), None);
        assert_eq!(stale_base_warning("develop", &branches), None);
        let fresh = vec![branch("main", Some(("origin/main", 0)))];
        assert_eq!(stale_base_warning("main", &fresh), None);
    }

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            language_model::init(cx);
            crate::init(cx);
        });
    }

    fn dot_git() -> &'static Path {
        Path::new(path!("/project/.git"))
    }

    /// Four commits in a line, newest first as the graph lists them: `oid(1)` is the tip of the
    /// development branch and `oid(4)` is where `origin/main` is.
    fn graph_commits() -> Vec<Arc<InitialGraphCommitData>> {
        (1..=4u8)
            .map(|byte| {
                Arc::new(InitialGraphCommitData {
                    sha: oid(byte),
                    parents: (byte < 4).then(|| oid(byte + 1)).into_iter().collect(),
                    ref_names: Vec::new(),
                })
            })
            .collect()
    }

    /// What `git log origin/main..<development_branch>` answers: `commits`, oldest first.
    fn set_development_range(fs: &FakeFs, development_branch: &str, commits: &[u8]) {
        let stdout = commits
            .iter()
            .map(|byte| {
                format!(
                    "{}\x1f{}\x1f件名 {byte}\x1f件名 {byte}\n\n本文 {byte}\n\0",
                    oid(*byte),
                    oid(byte + 1)
                )
            })
            .collect::<String>();
        let key = development_range_args("origin/main", development_branch).join(" ");
        fs.with_git_state(dot_git(), false, |state| {
            state.command_outputs.insert(
                key,
                git::repository::GitCommandOutput {
                    stdout,
                    stderr: String::new(),
                    exit_code: Some(0),
                },
            );
        })
        .unwrap();
    }

    fn set_git_result(fs: &FakeFs, args: &str, stdout: &str, exit_code: i32) {
        fs.with_git_state(dot_git(), false, |state| {
            state.command_outputs.insert(
                args.to_string(),
                git::repository::GitCommandOutput {
                    stdout: stdout.to_string(),
                    stderr: String::new(),
                    exit_code: Some(exit_code),
                },
            );
        })
        .unwrap();
    }

    /// A project whose repository is on `development_branch`, and a workspace with the commit
    /// graph open in it.
    async fn setup(
        development_branch: &str,
        cx: &mut TestAppContext,
    ) -> (
        Arc<FakeFs>,
        Entity<Workspace>,
        Entity<GitGraph>,
        VisualTestContext,
    ) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            serde_json::json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;
        fs.set_graph_commits(dot_git(), graph_commits());
        fs.set_branch_name(dot_git(), Some(development_branch));
        set_development_range(&fs, development_branch, &[3, 2, 1]);

        let project = Project::test(fs.clone(), [Path::new(path!("/project"))], cx).await;
        let window = cx.add_window(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .unwrap();
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        cx.run_until_parked();

        let graph = workspace.update_in(&mut cx, |workspace, window, cx| {
            let project = workspace.project().read(cx);
            let repo_id = project
                .active_repository(cx)
                .expect("the repository should be found")
                .read(cx)
                .id;
            let git_store = project.git_store().clone();
            let workspace_handle = workspace.weak_handle();
            let graph =
                cx.new(|cx| GitGraph::new(repo_id, git_store, workspace_handle, None, window, cx));
            workspace.add_item_to_active_pane(Box::new(graph.clone()), None, true, window, cx);
            graph
        });
        cx.run_until_parked();
        (fs, workspace, graph, cx)
    }

    fn open_box(graph: &Entity<GitGraph>, cx: &mut VisualTestContext) -> Entity<MergeRequestBox> {
        graph.update_in(cx, |graph, window, cx| {
            graph.toggle_merge_request_box(window, cx);
        });
        cx.run_until_parked();
        graph
            .read_with(cx, |graph, _| graph.visible_merge_request_box())
            .expect("the box should be open")
    }

    fn placed_in(merge_request_box: &Entity<MergeRequestBox>, cx: &VisualTestContext) -> Vec<Oid> {
        merge_request_box.read_with(cx, |merge_request_box, _| placed(&merge_request_box.state))
    }

    fn mark_of(graph: &Entity<GitGraph>, sha: Oid, cx: &VisualTestContext) -> Option<RowMark> {
        graph.read_with(cx, |graph, cx| {
            graph
                .merge_request_row_marks(cx)
                .and_then(|marks| marks.mark(&sha))
        })
    }

    /// Something to drag a commit from, in a pane next to the graph: the graph's own rows carry
    /// no selector a test can find them by.
    struct CommitDragSource {
        dragged: DraggedCommit,
        focus_handle: FocusHandle,
    }

    impl gpui::EventEmitter<()> for CommitDragSource {}

    impl Focusable for CommitDragSource {
        fn focus_handle(&self, _cx: &App) -> FocusHandle {
            self.focus_handle.clone()
        }
    }

    impl workspace::item::Item for CommitDragSource {
        type Event = ();

        fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
            "drag".into()
        }
    }

    impl Render for CommitDragSource {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .id("commit-drag-source")
                .debug_selector(|| "commit-drag-source".into())
                .size_full()
                .on_drag(self.dragged.clone(), |_, _, _, cx| cx.new(|_| gpui::Empty))
        }
    }

    /// The box opens next to the graph on the checked-out branch, measured against the remote
    /// default branch; a dragged multi-selection goes in whole, in history order, and the graph
    /// marks what is placed, excluded and still unplaced, for as long as the box is open.
    #[gpui::test]
    async fn test_box_takes_a_dragged_selection_and_marks_the_rows(cx: &mut TestAppContext) {
        let (_fs, workspace, graph, mut cx) = setup("dev-drag", cx).await;
        let merge_request_box = open_box(&graph, &mut cx);
        merge_request_box.read_with(&cx, |merge_request_box, _| {
            assert_eq!(
                merge_request_box.development_branch.as_deref(),
                Some("dev-drag")
            );
            assert_eq!(merge_request_box.base.as_deref(), Some("origin/main"));
            assert_eq!(merge_request_box.range_status, RangeStatus::Loaded);
            assert_eq!(merge_request_box.state.unplaced().len(), 3);
        });
        assert_eq!(mark_of(&graph, oid(3), &cx), Some(RowMark::Unplaced));
        assert_eq!(
            mark_of(&graph, oid(4), &cx),
            None,
            "the base is not in the range"
        );

        let source = cx.new(|cx| CommitDragSource {
            dragged: DraggedCommit {
                sha: oid(1),
                subject: "件名 1".into(),
                selection: Arc::from([oid(2), oid(1)]),
            },
            focus_handle: cx.focus_handle(),
        });
        workspace.update_in(&mut cx, |workspace, window, cx| {
            workspace.split_item(
                workspace::SplitDirection::Left,
                Box::new(source),
                window,
                cx,
            );
        });
        graph.update(&mut cx, |_, cx| cx.notify());
        cx.run_until_parked();
        let from = cx
            .debug_bounds("commit-drag-source")
            .expect("the drag source should be on screen");
        let to = cx
            .debug_bounds("merge-request-box-drop")
            .expect("the box should be on screen");
        cx.simulate_mouse_down(from.center(), MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(
            from.center() + point(px(20.), px(0.)),
            MouseButton::Left,
            Modifiers::none(),
        );
        cx.simulate_mouse_move(to.center(), MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(to.center(), MouseButton::Left, Modifiers::none());
        cx.run_until_parked();

        assert_eq!(
            placed_in(&merge_request_box, &cx),
            vec![oid(2), oid(1)],
            "the whole selection, oldest first"
        );
        merge_request_box.read_with(&cx, |merge_request_box, _| {
            assert_eq!(merge_request_box.state.message, "件名 2\n\n本文 2\n件名 1");
        });
        assert_eq!(mark_of(&graph, oid(1), &cx), Some(RowMark::Placed));
        assert_eq!(mark_of(&graph, oid(2), &cx), Some(RowMark::Placed));
        assert_eq!(mark_of(&graph, oid(3), &cx), Some(RowMark::Unplaced));

        merge_request_box.update_in(&mut cx, |merge_request_box, window, cx| {
            merge_request_box.exclude_commits(&[oid(3)], window, cx);
        });
        cx.run_until_parked();
        assert_eq!(mark_of(&graph, oid(3), &cx), Some(RowMark::Excluded));

        // Closed, the rows go back to normal; opened again, the box is as it was.
        graph.update(&mut cx, |graph, cx| graph.close_merge_request_box(cx));
        assert_eq!(mark_of(&graph, oid(1), &cx), None);
        let reopened = open_box(&graph, &mut cx);
        assert_eq!(reopened, merge_request_box);
        assert_eq!(placed_in(&reopened, &cx), vec![oid(2), oid(1)]);
    }

    /// The multi-selection menu's entries put the picked rows in the box, opening it first.
    #[gpui::test]
    async fn test_multi_selection_goes_into_the_box(cx: &mut TestAppContext) {
        let (_fs, _workspace, graph, mut cx) = setup("dev-menu", cx).await;
        graph.update_in(&mut cx, |graph, window, cx| {
            graph.select_rows_for_test(&[0, 2]);
            graph.add_selection_to_merge_request_box(window, cx);
        });
        cx.run_until_parked();
        let merge_request_box = graph
            .read_with(&cx, |graph, _| graph.visible_merge_request_box())
            .expect("the menu should have opened the box");
        assert_eq!(placed_in(&merge_request_box, &cx), vec![oid(3), oid(1)]);
    }

    /// However many commits are in the box, the dashed frame holds them all and the rest of the
    /// box stays below it: a short window scrolls the box instead of squeezing its parts.
    #[gpui::test]
    async fn test_full_box_scrolls_instead_of_overflowing(cx: &mut TestAppContext) {
        let (_fs, _workspace, graph, mut cx) = setup("dev-full", cx).await;
        let merge_request_box = open_box(&graph, &mut cx);
        let bytes: Vec<u8> = (100..140).collect();
        merge_request_box.update_in(&mut cx, |merge_request_box, window, cx| {
            merge_request_box.add_candidates(
                bytes.iter().map(|byte| candidate(*byte)).collect(),
                window,
                cx,
            );
        });
        let last_card: &'static str = format!("merge-request-box-card-{}", oid(139)).leak();

        let measure = |height: f32, cx: &mut VisualTestContext| {
            cx.simulate_resize(gpui::size(px(1600.), px(height)));
            cx.run_until_parked();
            let mut bounds = |selector: &'static str| {
                cx.debug_bounds(selector)
                    .unwrap_or_else(|| panic!("{selector} should be on screen"))
            };
            (
                bounds("merge-request-box"),
                bounds("merge-request-box-body"),
                bounds("merge-request-box-drop"),
                bounds(last_card),
                bounds("merge-request-box-range"),
                bounds("merge-request-box-message"),
            )
        };

        let (_, _, tall_drop, _, _, tall_message) = measure(4000., &mut cx);
        let (column, body, drop, card, range, message) = measure(500., &mut cx);
        assert!(
            card.bottom() <= drop.bottom(),
            "the last card ({:?}) is drawn past the frame ({:?})",
            card.bottom(),
            drop.bottom()
        );
        assert!(
            drop.bottom() <= range.top(),
            "the frame ({:?}) runs into the unplaced count ({:?})",
            drop.bottom(),
            range.top()
        );
        assert_eq!(
            drop.size.height, tall_drop.size.height,
            "a short window squeezes the frame"
        );
        assert_eq!(
            message.size.height, tall_message.size.height,
            "a short window squeezes the message editor"
        );
        assert!(
            range.top() > body.bottom(),
            "with this much in the box, the rest is below the fold and the body scrolls"
        );
        assert!(body.bottom() <= column.bottom());
    }

    /// Picks the `from_end`th of the MR entries counted from the last one, which come right
    /// before the custom commands' "詳細" in a row's menu and right before "SHA をコピー" in the
    /// multi-selection menu.
    fn confirm_merge_request_entry(
        graph: &Entity<GitGraph>,
        row: Option<usize>,
        from_end: usize,
        cx: &mut VisualTestContext,
    ) {
        let menu = graph
            .update_in(cx, |graph, window, cx| {
                graph.open_row_menu_for_test(row, window, cx)
            })
            .expect("the menu should open");
        menu.update_in(cx, |menu, window, cx| {
            menu.select_last(window, cx);
            for _ in 0..from_end {
                menu.select_previous(&menu::SelectPrevious, window, cx);
            }
            menu.confirm(&menu::Confirm, window, cx);
        });
        cx.run_until_parked();
    }

    /// With the list of unplaced commits gone from the box, the graph's own menus set rows aside
    /// and bring them back, and the box points at the first row still to decide on.
    #[gpui::test]
    async fn test_graph_menus_exclude_and_restore(cx: &mut TestAppContext) {
        let (_fs, _workspace, graph, mut cx) = setup("dev-exclude", cx).await;
        let merge_request_box = open_box(&graph, &mut cx);
        // Rows are newest first: oid(1) is row 0 and oid(3) row 2.
        confirm_merge_request_entry(&graph, Some(2), 1, &mut cx);
        assert_eq!(mark_of(&graph, oid(3), &cx), Some(RowMark::Excluded));

        cx.simulate_resize(gpui::size(px(1600.), px(1200.)));
        cx.run_until_parked();
        let first_unplaced = cx
            .debug_bounds("merge-request-box-first-unplaced")
            .expect("the box should point at what is left");
        cx.simulate_click(first_unplaced.center(), Modifiers::none());
        cx.run_until_parked();
        assert_eq!(
            graph.read_with(&cx, |graph, _| graph.selected_row_for_test()),
            Some(1),
            "oid(2) is now the oldest unplaced commit"
        );

        confirm_merge_request_entry(&graph, Some(2), 1, &mut cx);
        assert_eq!(mark_of(&graph, oid(3), &cx), Some(RowMark::Unplaced));

        merge_request_box.update_in(&mut cx, |merge_request_box, window, cx| {
            merge_request_box.add_candidates(vec![candidate(1)], window, cx);
        });
        graph.update(&mut cx, |graph, _| graph.select_rows_for_test(&[0, 1, 3]));
        confirm_merge_request_entry(&graph, None, 1, &mut cx);
        assert_eq!(
            merge_request_box.read_with(&cx, |merge_request_box, _| merge_request_box
                .state
                .excluded_shas()),
            vec![oid(2), oid(1)],
            "the placed and the unplaced row, not the base below the range"
        );
        assert_eq!(
            mark_of(&graph, oid(4), &cx),
            None,
            "the base below the range was never missing from the box"
        );
        assert!(placed_in(&merge_request_box, &cx).is_empty());

        confirm_merge_request_entry(&graph, None, 1, &mut cx);
        assert_eq!(mark_of(&graph, oid(1), &cx), Some(RowMark::Unplaced));
        assert_eq!(mark_of(&graph, oid(2), &cx), Some(RowMark::Unplaced));

        // With the unplaced list gone, a row's own menu puts it in the box, just above the entry
        // that sets it aside.
        confirm_merge_request_entry(&graph, Some(0), 2, &mut cx);
        assert_eq!(mark_of(&graph, oid(1), &cx), Some(RowMark::Placed));
        assert_eq!(placed_in(&merge_request_box, &cx), vec![oid(1)]);
        assert!(
            merge_request_box
                .read_with(&cx, |merge_request_box, _| merge_request_box
                    .menu_choices(&[oid(1)])
                    .addable)
                .is_empty(),
            "a row already in the box is not offered again"
        );
    }

    /// `git::ToggleMergeRequestBox` with no graph open opens one, with the box in it; again, it
    /// closes the box.
    #[gpui::test]
    async fn test_toggle_action_opens_the_graph_with_the_box(cx: &mut TestAppContext) {
        let (_fs, workspace, graph, mut cx) = setup("dev-action", cx).await;
        workspace.update_in(&mut cx, |workspace, window, cx| {
            let pane = workspace.active_pane().clone();
            pane.update(cx, |pane, cx| {
                pane.close_all_items(&Default::default(), window, cx)
                    .detach();
            });
        });
        cx.run_until_parked();
        drop(graph);
        assert_eq!(
            workspace.read_with(&cx, |workspace, cx| workspace
                .items_of_type::<GitGraph>(cx)
                .count()),
            0
        );

        cx.dispatch_action(git::ToggleMergeRequestBox);
        cx.run_until_parked();
        let graph = workspace
            .read_with(&cx, |workspace, cx| {
                workspace.items_of_type::<GitGraph>(cx).next()
            })
            .expect("the action should have opened the graph");
        assert!(graph.read_with(&cx, |graph, _| graph.visible_merge_request_box().is_some()));

        cx.dispatch_action(git::ToggleMergeRequestBox);
        cx.run_until_parked();
        assert!(graph.read_with(&cx, |graph, _| graph.visible_merge_request_box().is_none()));
    }

    /// `git::OpenMergeRequestBox` (the Git menu's "MR を作る") opens the box and, pressed again,
    /// leaves it open with what is in it.
    #[gpui::test]
    async fn test_open_action_never_closes_the_box(cx: &mut TestAppContext) {
        let (_fs, _workspace, graph, mut cx) = setup("dev-open", cx).await;
        assert!(graph.read_with(&cx, |graph, _| graph.visible_merge_request_box().is_none()));

        cx.dispatch_action(git::OpenMergeRequestBox);
        cx.run_until_parked();
        let merge_request_box = graph
            .read_with(&cx, |graph, _| graph.visible_merge_request_box())
            .expect("the action should have opened the box");

        cx.dispatch_action(git::OpenMergeRequestBox);
        cx.run_until_parked();
        assert_eq!(
            graph.read_with(&cx, |graph, _| graph.visible_merge_request_box()),
            Some(merge_request_box),
            "the same box stays open"
        );
    }

    /// The box is saved per development branch and read back into a new one; a commit that a
    /// rewrite of the branch took away leaves the box, and the user is told.
    #[gpui::test]
    async fn test_box_is_restored_and_pruned_after_a_rewrite(cx: &mut TestAppContext) {
        let (fs, workspace, graph, mut cx) = setup("dev-restore", cx).await;
        let merge_request_box = open_box(&graph, &mut cx);
        merge_request_box.update_in(&mut cx, |merge_request_box, window, cx| {
            let candidates = merge_request_box
                .state
                .unplaced()
                .into_iter()
                .filter(|commit| commit.sha != oid(3))
                .map(range_candidate)
                .collect();
            merge_request_box.add_candidates(candidates, window, cx);
            merge_request_box.exclude_commits(&[oid(3)], window, cx);
        });
        cx.executor().advance_clock(SAVE_DELAY * 2);
        cx.run_until_parked();

        // The branch is rewritten: 2 is gone, 5 took its place.
        set_development_range(&fs, "dev-restore", &[3, 5, 1]);
        let second = workspace.update_in(&mut cx, |workspace, window, cx| {
            let project = workspace.project().read(cx);
            let repo_id = project.active_repository(cx).unwrap().read(cx).id;
            let git_store = project.git_store().clone();
            let workspace_handle = workspace.weak_handle();
            cx.new(|cx| GitGraph::new(repo_id, git_store, workspace_handle, None, window, cx))
        });
        let restored = open_box(&second, &mut cx);
        assert_eq!(placed_in(&restored, &cx), vec![oid(1)]);
        restored.read_with(&cx, |restored, _| {
            assert_eq!(restored.state.excluded_shas(), vec![oid(3)]);
            let notice = restored
                .box_notice
                .as_ref()
                .expect("taking commits out of the box is said out loud");
            assert!(
                notice.text.contains("1 commit was taken out of the box"),
                "{}",
                notice.text
            );
            assert_eq!(
                restored
                    .state
                    .unplaced()
                    .iter()
                    .map(|commit| commit.sha)
                    .collect::<Vec<_>>(),
                vec![oid(5)]
            );
        });
    }

    /// A development range longer than the box reads is refused with the reason, rather than
    /// shown in part with nothing ever taken out of the box.
    #[gpui::test]
    async fn test_too_wide_range_is_refused(cx: &mut TestAppContext) {
        let (fs, _workspace, graph, mut cx) = setup("dev-wide", cx).await;
        let stdout = (0..=MAX_RANGE_COMMITS)
            .map(|index| {
                let sha: Oid = format!("{:040x}", index + 1000).parse().unwrap();
                format!("{sha}\x1f\x1f件名\x1f件名\n\0")
            })
            .collect::<String>();
        set_git_result(
            &fs,
            &development_range_args("origin/main", "dev-wide").join(" "),
            &stdout,
            0,
        );
        let merge_request_box = open_box(&graph, &mut cx);
        merge_request_box.read_with(&cx, |merge_request_box, _| {
            assert_eq!(
                merge_request_box.range_status,
                RangeStatus::Failed(range_too_wide_message().into())
            );
            assert!(merge_request_box.state.unplaced().is_empty());
            assert!(merge_request_box.state.range.is_none());
            assert!(
                merge_request_box.unverified_range().is_some(),
                "creating still says the check is incomplete"
            );
        });
    }

    /// A commit dropped in from outside the development range before the graph had loaded it
    /// gets its subject on the next read of the range, which every change of the refs brings.
    #[gpui::test]
    async fn test_outside_commit_subjects_are_read_again(cx: &mut TestAppContext) {
        let (fs, _workspace, graph, mut cx) = setup("dev-outside", cx).await;
        let merge_request_box = open_box(&graph, &mut cx);
        let outsider = oid(4);
        merge_request_box.update_in(&mut cx, |merge_request_box, window, cx| {
            merge_request_box.add_candidates(
                vec![MergeRequestCandidate {
                    sha: outsider,
                    subject: outsider.display_short().into(),
                    message: SharedString::default(),
                    is_merge: false,
                }],
                window,
                cx,
            );
        });
        cx.run_until_parked();

        set_git_result(
            &fs,
            &outside_commit_args(&[outsider]).join(" "),
            &format!("{outsider}\x1f\x1f基点の件名\x1f基点の件名\n\n本文\n\0"),
            0,
        );
        merge_request_box.update_in(&mut cx, |merge_request_box, window, cx| {
            merge_request_box.reload_range(window, cx);
        });
        cx.run_until_parked();
        merge_request_box.read_with(&cx, |merge_request_box, _| {
            let placed = merge_request_box
                .state
                .placed
                .iter()
                .find(|placed| placed.sha == outsider)
                .expect("the outsider stays in the box");
            assert_eq!(placed.subject.as_ref(), "基点の件名");
            assert_eq!(placed.message.as_ref(), "基点の件名\n\n本文");
        });
    }

    /// The create button refuses a dirty tree with the box left as it is; on a clean tree it asks,
    /// builds, and opens the merge-request check.
    #[gpui::test]
    async fn test_create_builds_and_opens_the_comparison(cx: &mut TestAppContext) {
        let (fs, workspace, graph, mut cx) = setup("dev-create", cx).await;
        let merge_request_box = open_box(&graph, &mut cx);
        merge_request_box.update_in(&mut cx, |merge_request_box, window, cx| {
            merge_request_box.add_candidates(
                vec![
                    range_candidate(&range_commit(1, false)),
                    range_candidate(&range_commit(2, false)),
                ],
                window,
                cx,
            );
        });
        cx.run_until_parked();

        fs.set_status_for_repo(
            dot_git(),
            &[("file.txt", git::status::StatusCode::Modified.worktree())],
        );
        cx.run_until_parked();
        merge_request_box.update_in(&mut cx, |merge_request_box, window, cx| {
            merge_request_box.create(window, cx);
        });
        cx.run_until_parked();
        assert!(
            !cx.has_pending_prompt(),
            "a dirty tree is refused before asking"
        );
        merge_request_box.read_with(&cx, |merge_request_box, _| {
            let notice = merge_request_box.create_notice.as_ref().unwrap();
            assert!(
                notice.text.contains("uncommitted changes"),
                "{}",
                notice.text
            );
            assert_eq!(placed(&merge_request_box.state), vec![oid(2), oid(1)]);
        });

        fs.set_status_for_repo(dot_git(), &[]);
        cx.run_until_parked();

        // The base is never the branch that gets (re)created.
        merge_request_box.update_in(&mut cx, |merge_request_box, window, cx| {
            merge_request_box
                .branch_name_editor
                .update(cx, |editor, cx| {
                    editor.set_text("origin/main", window, cx);
                });
            let refusal = merge_request_box
                .prepare_build(cx)
                .err()
                .expect("the base may not be the merge-request branch");
            assert!(refusal.contains("The base branch"), "{refusal}");

            // Nor the default branch, "作り直す" ticked or not.
            assert_eq!(
                merge_request_box.local_default_branch.as_deref(),
                Some("main")
            );
            merge_request_box
                .branch_name_editor
                .update(cx, |editor, cx| {
                    editor.set_text("main", window, cx);
                });
            for recreate in [false, true] {
                merge_request_box.recreate = recreate;
                let refusal = merge_request_box
                    .prepare_build(cx)
                    .err()
                    .expect("the default branch may not be the merge-request branch");
                assert!(refusal.contains("default branch"), "{refusal}");
            }
            merge_request_box.recreate = false;
            merge_request_box
                .branch_name_editor
                .update(cx, |editor, cx| {
                    editor.set_text("mr/dev-create", window, cx);
                });
        });
        cx.run_until_parked();

        let base = "b000000000000000000000000000000000000000";
        let scratch = merge_request_scratch_branch_name(&oid(1).to_string());
        set_git_result(&fs, "rev-parse --abbrev-ref HEAD", "dev-create\n", 0);
        for probe in [
            "REBASE_HEAD",
            "CHERRY_PICK_HEAD",
            "REVERT_HEAD",
            "MERGE_HEAD",
        ] {
            set_git_result(&fs, &format!("rev-parse --verify --quiet {probe}"), "", 1);
        }
        set_git_result(&fs, "diff --name-only --diff-filter=U", "", 0);
        set_git_result(&fs, "diff --quiet HEAD", "", 0);
        set_git_result(
            &fs,
            &format!("rev-parse --verify --quiet refs/heads/{scratch}"),
            "",
            1,
        );
        set_git_result(
            &fs,
            "rev-parse --verify --quiet origin/main^{commit}",
            base,
            0,
        );
        set_git_result(
            &fs,
            "rev-parse --verify --quiet refs/heads/mr/dev-create",
            "",
            1,
        );
        // The box's commits touch file.txt, which is no ignored local file.
        set_git_result(
            &fs,
            &format!(
                "log --format= --name-only --no-renames -z --no-walk {} {}",
                oid(2),
                oid(1)
            ),
            "file.txt\0",
            0,
        );
        set_git_result(
            &fs,
            "ls-files --others --ignored --exclude-standard -z -- :(top,literal)file.txt",
            "",
            0,
        );
        let commands = [
            format!("switch --no-overwrite-ignore -c {scratch} {base}"),
            format!("cherry-pick --keep-redundant-commits {} {}", oid(2), oid(1)),
            format!("reset --soft {base}"),
            "commit -m 件名 2\n\n本文 2\n件名 1".to_string(),
            "branch mr/dev-create HEAD".to_string(),
            "switch dev-create".to_string(),
            format!("branch -D {scratch}"),
        ];
        for command in &commands {
            set_git_result(&fs, command, "", 0);
        }
        set_git_result(&fs, &format!("diff --cached --quiet {base}"), "", 1);
        set_git_result(
            &fs,
            "rev-parse --verify --quiet HEAD",
            "d400000000000000000000000000000000000000",
            0,
        );

        merge_request_box.update_in(&mut cx, |merge_request_box, window, cx| {
            merge_request_box.create(window, cx);
        });
        cx.run_until_parked();
        assert!(cx.has_pending_prompt(), "a build is confirmed first");
        cx.simulate_prompt_answer("Create");
        cx.run_until_parked();

        let log = fs
            .with_git_state(dot_git(), false, |state| {
                state
                    .run_command_log
                    .iter()
                    .map(|(args, _)| args.join(" "))
                    .collect::<Vec<_>>()
            })
            .unwrap();
        for command in &commands {
            assert!(
                log.contains(command),
                "`{command}` should have run: {log:#?}"
            );
        }
        assert_eq!(
            workspace.read_with(&cx, |workspace, cx| workspace
                .items_of_type::<RefComparison>(cx)
                .count()),
            1,
            "the comparison should have opened"
        );
        merge_request_box.read_with(&cx, |merge_request_box, _| {
            assert!(!merge_request_box.building);
            assert_eq!(merge_request_box.create_notice, None);
        });
    }
}
