use crate::{Oid, repository::RepoPath};
use anyhow::{Result, anyhow};
use collections::HashMap;
use gpui::SharedString;
use serde::{Deserialize, Serialize};
use std::{str::FromStr, sync::Arc};
use util::{ResultExt, rel_path::RelPath};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FileStatus {
    Untracked,
    Ignored,
    Unmerged(UnmergedStatus),
    Tracked(TrackedStatus),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct UnmergedStatus {
    pub first_head: UnmergedStatusCode,
    pub second_head: UnmergedStatusCode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum UnmergedStatusCode {
    Added,
    Deleted,
    Updated,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TrackedStatus {
    pub index_status: StatusCode,
    pub worktree_status: StatusCode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum StatusCode {
    Modified,
    TypeChanged,
    Added,
    Deleted,
    Renamed,
    Copied,
    Unmodified,
}

impl From<UnmergedStatus> for FileStatus {
    fn from(value: UnmergedStatus) -> Self {
        FileStatus::Unmerged(value)
    }
}

impl From<TrackedStatus> for FileStatus {
    fn from(value: TrackedStatus) -> Self {
        FileStatus::Tracked(value)
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum StageStatus {
    Staged,
    Unstaged,
    PartiallyStaged,
}

impl StageStatus {
    pub const fn is_fully_staged(&self) -> bool {
        matches!(self, StageStatus::Staged)
    }

    pub const fn is_fully_unstaged(&self) -> bool {
        matches!(self, StageStatus::Unstaged)
    }

    pub const fn has_staged(&self) -> bool {
        matches!(self, StageStatus::Staged | StageStatus::PartiallyStaged)
    }

    pub const fn has_unstaged(&self) -> bool {
        matches!(self, StageStatus::Unstaged | StageStatus::PartiallyStaged)
    }

    pub const fn as_bool(self) -> Option<bool> {
        match self {
            StageStatus::Staged => Some(true),
            StageStatus::Unstaged => Some(false),
            StageStatus::PartiallyStaged => None,
        }
    }
}

impl FileStatus {
    pub const fn worktree(worktree_status: StatusCode) -> Self {
        FileStatus::Tracked(TrackedStatus {
            index_status: StatusCode::Unmodified,
            worktree_status,
        })
    }

    pub const fn index(index_status: StatusCode) -> Self {
        FileStatus::Tracked(TrackedStatus {
            worktree_status: StatusCode::Unmodified,
            index_status,
        })
    }

    /// Generate a FileStatus Code from a byte pair, as described in
    /// https://git-scm.com/docs/git-status#_output
    ///
    /// NOTE: That instead of '', we use ' ' to denote no change
    fn from_bytes(bytes: [u8; 2]) -> anyhow::Result<Self> {
        let status = match bytes {
            [b'?', b'?'] => FileStatus::Untracked,
            [b'!', b'!'] => FileStatus::Ignored,
            [b'A', b'A'] => UnmergedStatus {
                first_head: UnmergedStatusCode::Added,
                second_head: UnmergedStatusCode::Added,
            }
            .into(),
            [b'D', b'D'] => UnmergedStatus {
                first_head: UnmergedStatusCode::Added,
                second_head: UnmergedStatusCode::Added,
            }
            .into(),
            [x, b'U'] => UnmergedStatus {
                first_head: UnmergedStatusCode::from_byte(x)?,
                second_head: UnmergedStatusCode::Updated,
            }
            .into(),
            [b'U', y] => UnmergedStatus {
                first_head: UnmergedStatusCode::Updated,
                second_head: UnmergedStatusCode::from_byte(y)?,
            }
            .into(),
            [x, y] => TrackedStatus {
                index_status: StatusCode::from_byte(x)?,
                worktree_status: StatusCode::from_byte(y)?,
            }
            .into(),
        };
        Ok(status)
    }

    pub fn staging(self) -> StageStatus {
        match self {
            FileStatus::Untracked | FileStatus::Ignored | FileStatus::Unmerged { .. } => {
                StageStatus::Unstaged
            }
            FileStatus::Tracked(tracked) => match (tracked.index_status, tracked.worktree_status) {
                (StatusCode::Unmodified, _) => StageStatus::Unstaged,
                (_, StatusCode::Unmodified) => StageStatus::Staged,
                _ => StageStatus::PartiallyStaged,
            },
        }
    }

    pub fn is_conflicted(self) -> bool {
        matches!(self, FileStatus::Unmerged { .. })
    }

    pub fn is_ignored(self) -> bool {
        matches!(self, FileStatus::Ignored)
    }

    pub fn has_changes(&self) -> bool {
        self.is_modified()
            || self.is_created()
            || self.is_deleted()
            || self.is_untracked()
            || self.is_conflicted()
    }

    pub fn is_modified(self) -> bool {
        match self {
            FileStatus::Tracked(tracked) => matches!(
                (tracked.index_status, tracked.worktree_status),
                (StatusCode::Modified, _) | (_, StatusCode::Modified)
            ),
            _ => false,
        }
    }

    pub fn is_created(self) -> bool {
        match self {
            FileStatus::Tracked(tracked) => matches!(
                (tracked.index_status, tracked.worktree_status),
                (StatusCode::Added, _) | (_, StatusCode::Added)
            ),
            FileStatus::Untracked => true,
            _ => false,
        }
    }

    pub fn is_deleted(self) -> bool {
        let FileStatus::Tracked(tracked) = self else {
            return false;
        };
        tracked.index_status == StatusCode::Deleted && tracked.worktree_status != StatusCode::Added
            || tracked.worktree_status == StatusCode::Deleted
    }

    pub fn is_untracked(self) -> bool {
        matches!(self, FileStatus::Untracked)
    }

    pub fn summary(self) -> GitSummary {
        match self {
            FileStatus::Ignored => GitSummary::UNCHANGED,
            FileStatus::Untracked => GitSummary::UNTRACKED,
            FileStatus::Unmerged(_) => GitSummary::CONFLICT,
            FileStatus::Tracked(TrackedStatus {
                index_status,
                worktree_status,
            }) => GitSummary {
                index: index_status.to_summary(),
                worktree: worktree_status.to_summary(),
                conflict: 0,
                untracked: 0,
                count: 1,
            },
        }
    }
}

impl StatusCode {
    fn from_byte(byte: u8) -> anyhow::Result<Self> {
        match byte {
            b'M' => Ok(StatusCode::Modified),
            b'T' => Ok(StatusCode::TypeChanged),
            b'A' => Ok(StatusCode::Added),
            b'D' => Ok(StatusCode::Deleted),
            b'R' => Ok(StatusCode::Renamed),
            b'C' => Ok(StatusCode::Copied),
            b' ' => Ok(StatusCode::Unmodified),
            _ => anyhow::bail!("Invalid status code: {byte}"),
        }
    }

    fn to_summary(self) -> TrackedSummary {
        match self {
            StatusCode::Modified | StatusCode::TypeChanged => TrackedSummary {
                modified: 1,
                ..TrackedSummary::UNCHANGED
            },
            StatusCode::Added => TrackedSummary {
                added: 1,
                ..TrackedSummary::UNCHANGED
            },
            StatusCode::Deleted => TrackedSummary {
                deleted: 1,
                ..TrackedSummary::UNCHANGED
            },
            StatusCode::Renamed | StatusCode::Copied | StatusCode::Unmodified => {
                TrackedSummary::UNCHANGED
            }
        }
    }

    pub fn index(self) -> FileStatus {
        FileStatus::Tracked(TrackedStatus {
            index_status: self,
            worktree_status: StatusCode::Unmodified,
        })
    }

    pub fn worktree(self) -> FileStatus {
        FileStatus::Tracked(TrackedStatus {
            index_status: StatusCode::Unmodified,
            worktree_status: self,
        })
    }
}

impl UnmergedStatusCode {
    fn from_byte(byte: u8) -> anyhow::Result<Self> {
        match byte {
            b'A' => Ok(UnmergedStatusCode::Added),
            b'D' => Ok(UnmergedStatusCode::Deleted),
            b'U' => Ok(UnmergedStatusCode::Updated),
            _ => anyhow::bail!("Invalid unmerged status code: {byte}"),
        }
    }
}

#[derive(Clone, Debug, Default, Copy, PartialEq, Eq)]
pub struct TrackedSummary {
    pub added: usize,
    pub modified: usize,
    pub deleted: usize,
}

impl TrackedSummary {
    pub const UNCHANGED: Self = Self {
        added: 0,
        modified: 0,
        deleted: 0,
    };

    pub const ADDED: Self = Self {
        added: 1,
        modified: 0,
        deleted: 0,
    };

    pub const MODIFIED: Self = Self {
        added: 0,
        modified: 1,
        deleted: 0,
    };

    pub const DELETED: Self = Self {
        added: 0,
        modified: 0,
        deleted: 1,
    };
}

impl std::ops::AddAssign for TrackedSummary {
    fn add_assign(&mut self, rhs: Self) {
        self.added += rhs.added;
        self.modified += rhs.modified;
        self.deleted += rhs.deleted;
    }
}

impl std::ops::Add for TrackedSummary {
    type Output = Self;

    fn add(self, rhs: Self) -> Self::Output {
        TrackedSummary {
            added: self.added + rhs.added,
            modified: self.modified + rhs.modified,
            deleted: self.deleted + rhs.deleted,
        }
    }
}

impl std::ops::Sub for TrackedSummary {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self::Output {
        TrackedSummary {
            added: self.added - rhs.added,
            modified: self.modified - rhs.modified,
            deleted: self.deleted - rhs.deleted,
        }
    }
}

#[derive(Clone, Debug, Default, Copy, PartialEq, Eq)]
pub struct GitSummary {
    pub index: TrackedSummary,
    pub worktree: TrackedSummary,
    pub conflict: usize,
    pub untracked: usize,
    pub count: usize,
}

impl GitSummary {
    pub const CONFLICT: Self = Self {
        conflict: 1,
        count: 1,
        ..Self::UNCHANGED
    };

    pub const UNTRACKED: Self = Self {
        untracked: 1,
        count: 1,
        ..Self::UNCHANGED
    };

    pub const UNCHANGED: Self = Self {
        index: TrackedSummary::UNCHANGED,
        worktree: TrackedSummary::UNCHANGED,
        conflict: 0,
        untracked: 0,
        count: 0,
    };
}

impl From<FileStatus> for GitSummary {
    fn from(status: FileStatus) -> Self {
        status.summary()
    }
}

impl sum_tree::ContextLessSummary for GitSummary {
    fn zero() -> Self {
        Default::default()
    }

    fn add_summary(&mut self, rhs: &Self) {
        *self += *rhs;
    }
}

impl std::ops::Add<Self> for GitSummary {
    type Output = Self;

    fn add(mut self, rhs: Self) -> Self {
        self += rhs;
        self
    }
}

impl std::ops::AddAssign for GitSummary {
    fn add_assign(&mut self, rhs: Self) {
        self.index += rhs.index;
        self.worktree += rhs.worktree;
        self.conflict += rhs.conflict;
        self.untracked += rhs.untracked;
        self.count += rhs.count;
    }
}

impl std::ops::Sub for GitSummary {
    type Output = GitSummary;

    fn sub(self, rhs: Self) -> Self::Output {
        GitSummary {
            index: self.index - rhs.index,
            worktree: self.worktree - rhs.worktree,
            conflict: self.conflict - rhs.conflict,
            untracked: self.untracked - rhs.untracked,
            count: self.count - rhs.count,
        }
    }
}

#[derive(Clone, Debug)]
pub struct GitStatus {
    pub entries: Arc<[(RepoPath, FileStatus)]>,
}

impl FromStr for GitStatus {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let mut entries = Vec::new();
        let mut fields = s.split('\0');
        while let Some(field) = fields.next() {
            let Some((status, path)) = field.split_at_checked(3) else {
                continue;
            };
            let &[index_code, worktree_code, b' '] = status.as_bytes() else {
                continue;
            };
            let codes = [index_code, worktree_code];
            let is_rename_or_copy = |code: u8| matches!(code, b'R' | b'C');
            if codes.into_iter().any(is_rename_or_copy) {
                // In the `-z` format a rename or copy spans two fields, the new path and then the
                // original one, so the original must be consumed here rather than read as a record.
                let original_path = fields.next();
                // `git status` only accepts `--no-renames` since git 2.18, so older versions report
                // pairs. Record them exactly as `--no-renames` would: the new path as added, and a
                // renamed-away original as deleted, so no `Renamed`/`Copied` status reaches callers.
                let new_path_codes =
                    codes.map(|code| if is_rename_or_copy(code) { b'A' } else { code });
                entries.extend(status_entry(new_path_codes, path));
                let original_path_codes = codes.map(|code| if code == b'R' { b'D' } else { b' ' });
                if original_path_codes != [b' ', b' ']
                    && let Some(original_path) = original_path
                {
                    entries.extend(status_entry(original_path_codes, original_path));
                }
                continue;
            }
            // The git status output includes untracked directories as well as untracked files.
            // We do our own processing to compute the "summary" status of each directory,
            // so just skip any directories in the output, since they'll otherwise interfere
            // with our handling of nested repositories.
            if path.ends_with('/') {
                continue;
            }
            entries.extend(status_entry(codes, path));
        }
        entries.sort_unstable_by(|(a, _), (b, _)| a.cmp(b));
        // When a file exists in HEAD, is deleted in the index, and exists again in the working copy,
        // git produces two lines for it, one reading `D ` (deleted in index, unmodified in working copy)
        // and the other reading `??` (untracked). Merge these two into the equivalent of `DA`.
        entries.dedup_by(|(a, a_status), (b, b_status)| {
            const INDEX_DELETED: FileStatus = FileStatus::index(StatusCode::Deleted);
            if a.ne(&b) {
                return false;
            }
            match (*a_status, *b_status) {
                (INDEX_DELETED, FileStatus::Untracked) | (FileStatus::Untracked, INDEX_DELETED) => {
                    *b_status = TrackedStatus {
                        index_status: StatusCode::Deleted,
                        worktree_status: StatusCode::Added,
                    }
                    .into();
                }
                (x, y) if x == y => {}
                _ => {
                    log::warn!(
                        "Unexpected duplicated status entries: {a_status:?} and {b_status:?}"
                    );
                }
            }
            true
        });
        Ok(Self {
            entries: entries.into(),
        })
    }
}

fn status_entry(status: [u8; 2], path: &str) -> Option<(RepoPath, FileStatus)> {
    let status = FileStatus::from_bytes(status).log_err()?;
    // git-status outputs `/`-delimited repo paths, even on Windows.
    let path = RepoPath::from_rel_path(RelPath::from_unix_str(path).log_err()?);
    Some((path, status))
}

impl Default for GitStatus {
    fn default() -> Self {
        Self {
            entries: Arc::new([]),
        }
    }
}

pub enum DiffTreeType {
    MergeBase {
        base: SharedString,
        head: SharedString,
    },
    MergeBaseWithWorktree {
        base: SharedString,
    },
    Since {
        base: SharedString,
        head: SharedString,
    },
}

#[derive(Debug, PartialEq)]
pub struct TreeDiff {
    pub entries: HashMap<RepoPath, TreeDiffStatus>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TreeDiffStatus {
    Added,
    Modified { old: Oid },
    Deleted { old: Oid },
}

impl FromStr for TreeDiff {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let mut fields = s.split('\0');
        let mut parsed = HashMap::default();
        while let Some((status, path)) = fields.next().zip(fields.next()) {
            let path = RepoPath::from_rel_path(RelPath::from_unix_str(path)?);

            let mut fields = status.split(" ").skip(2);
            let old_sha = fields
                .next()
                .ok_or_else(|| anyhow!("expected to find old_sha"))?
                .to_owned()
                .parse()?;
            let _new_sha = fields
                .next()
                .ok_or_else(|| anyhow!("expected to find new_sha"))?;
            let status = fields
                .next()
                .and_then(|s| {
                    if s.len() == 1 {
                        s.as_bytes().first()
                    } else {
                        None
                    }
                })
                .ok_or_else(|| anyhow!("expected to find status"))?;

            let result = match StatusCode::from_byte(*status)? {
                StatusCode::Modified => TreeDiffStatus::Modified { old: old_sha },
                StatusCode::Added => TreeDiffStatus::Added,
                StatusCode::Deleted => TreeDiffStatus::Deleted { old: old_sha },
                _status => continue,
            };

            parsed.insert(path, result);
        }

        Ok(Self { entries: parsed })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DiffStat {
    pub added: u32,
    pub deleted: u32,
}

#[derive(Clone, Debug, Default)]
pub struct GitDiffStat {
    pub entries: Arc<[(RepoPath, DiffStat)]>,
}

/// Parses the output of `git diff --numstat` where output looks like:
///
/// ```text
/// 24   12   dir/file.txt
/// ```
pub fn parse_numstat(output: &str) -> GitDiffStat {
    let mut entries = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(3, '\t');
        let (Some(added_str), Some(deleted_str), Some(path_str)) =
            (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let Ok(added) = added_str.parse::<u32>() else {
            continue;
        };
        let Ok(deleted) = deleted_str.parse::<u32>() else {
            continue;
        };
        let Ok(path) = RepoPath::new(path_str) else {
            continue;
        };
        entries.push((path, DiffStat { added, deleted }));
    }
    entries.sort_unstable_by(|(a, _), (b, _)| a.cmp(b));
    entries.dedup_by(|(a, _), (b, _)| a == b);

    GitDiffStat {
        entries: entries.into(),
    }
}

#[cfg(test)]
mod tests {

    use crate::{
        repository::RepoPath,
        status::{FileStatus, GitStatus, StatusCode, TrackedStatus, TreeDiff, TreeDiffStatus},
    };

    use super::{DiffStat, parse_numstat};

    fn lookup<'a>(entries: &'a [(RepoPath, DiffStat)], path: &str) -> Option<&'a DiffStat> {
        let path = RepoPath::new(path).unwrap();
        entries.iter().find(|(p, _)| p == &path).map(|(_, s)| s)
    }

    fn parsed_status(output: &str) -> Vec<(String, FileStatus)> {
        let status: GitStatus = output.parse().unwrap();
        status
            .entries
            .iter()
            .map(|(path, status)| (path.as_unix_str().to_string(), *status))
            .collect()
    }

    #[test]
    fn test_rename_pairs_from_git_2_17() {
        // Verbatim output of git 2.17.1, which rejects `--no-renames`, for a repository where
        // `a` is modified, `n` is untracked, and `git mv k k2` is staged.
        assert_eq!(
            parsed_status(" M a\0R  k2\0k\0?? n\0"),
            [
                ("a".to_string(), FileStatus::worktree(StatusCode::Modified)),
                ("k".to_string(), FileStatus::index(StatusCode::Deleted)),
                ("k2".to_string(), FileStatus::index(StatusCode::Added)),
                ("n".to_string(), FileStatus::Untracked),
            ]
        );
    }

    #[test]
    fn test_rename_with_worktree_modification() {
        assert_eq!(
            parsed_status("RM new\0old\0"),
            [
                (
                    "new".to_string(),
                    FileStatus::Tracked(TrackedStatus {
                        index_status: StatusCode::Added,
                        worktree_status: StatusCode::Modified,
                    })
                ),
                ("old".to_string(), FileStatus::index(StatusCode::Deleted)),
            ]
        );
    }

    #[test]
    fn test_worktree_rename() {
        assert_eq!(
            parsed_status(" R new\0old\0"),
            [
                ("new".to_string(), FileStatus::worktree(StatusCode::Added)),
                ("old".to_string(), FileStatus::worktree(StatusCode::Deleted)),
            ]
        );
    }

    #[test]
    fn test_copy_records() {
        assert_eq!(
            parsed_status("C  copy\0orig\0M  orig\0"),
            [
                ("copy".to_string(), FileStatus::index(StatusCode::Added)),
                ("orig".to_string(), FileStatus::index(StatusCode::Modified)),
            ]
        );
        assert_eq!(
            parsed_status("C  copy\0orig\0"),
            [("copy".to_string(), FileStatus::index(StatusCode::Added))]
        );
    }

    #[test]
    fn test_rename_original_path_is_not_read_as_a_record() {
        // Original paths whose third character is a space look like status records on their own.
        assert_eq!(
            parsed_status("R  moved\0ab c\0R  new\0AM x\0"),
            [
                ("AM x".to_string(), FileStatus::index(StatusCode::Deleted)),
                ("ab c".to_string(), FileStatus::index(StatusCode::Deleted)),
                ("moved".to_string(), FileStatus::index(StatusCode::Added)),
                ("new".to_string(), FileStatus::index(StatusCode::Added)),
            ]
        );
    }

    #[test]
    fn test_rename_of_path_recreated_as_untracked() {
        assert_eq!(
            parsed_status("R  k2\0k\0?? k\0"),
            [
                (
                    "k".to_string(),
                    FileStatus::Tracked(TrackedStatus {
                        index_status: StatusCode::Deleted,
                        worktree_status: StatusCode::Added,
                    })
                ),
                ("k2".to_string(), FileStatus::index(StatusCode::Added)),
            ]
        );
    }

    #[test]
    fn test_parse_numstat_normal() {
        let input = "10\t5\tsrc/main.rs\n3\t1\tREADME.md\n";
        let result = parse_numstat(input);
        assert_eq!(result.entries.len(), 2);
        assert_eq!(
            lookup(&result.entries, "src/main.rs"),
            Some(&DiffStat {
                added: 10,
                deleted: 5
            })
        );
        assert_eq!(
            lookup(&result.entries, "README.md"),
            Some(&DiffStat {
                added: 3,
                deleted: 1
            })
        );
    }

    #[test]
    fn test_parse_numstat_binary_files_skipped() {
        // git diff --numstat outputs "-\t-\tpath" for binary files
        let input = "-\t-\timage.png\n5\t2\tsrc/lib.rs\n";
        let result = parse_numstat(input);
        assert_eq!(result.entries.len(), 1);
        assert!(lookup(&result.entries, "image.png").is_none());
        assert_eq!(
            lookup(&result.entries, "src/lib.rs"),
            Some(&DiffStat {
                added: 5,
                deleted: 2
            })
        );
    }

    #[test]
    fn test_parse_numstat_empty_input() {
        assert!(parse_numstat("").entries.is_empty());
        assert!(parse_numstat("\n\n").entries.is_empty());
        assert!(parse_numstat("   \n  \n").entries.is_empty());
    }

    #[test]
    fn test_parse_numstat_malformed_lines_skipped() {
        let input = "not_a_number\t5\tfile.rs\n10\t5\tvalid.rs\n";
        let result = parse_numstat(input);
        assert_eq!(result.entries.len(), 1);
        assert_eq!(
            lookup(&result.entries, "valid.rs"),
            Some(&DiffStat {
                added: 10,
                deleted: 5
            })
        );
    }

    #[test]
    fn test_parse_numstat_incomplete_lines_skipped() {
        // Lines with fewer than 3 tab-separated fields are skipped
        let input = "10\t5\n7\t3\tok.rs\n";
        let result = parse_numstat(input);
        assert_eq!(result.entries.len(), 1);
        assert_eq!(
            lookup(&result.entries, "ok.rs"),
            Some(&DiffStat {
                added: 7,
                deleted: 3
            })
        );
    }

    #[test]
    fn test_parse_numstat_zero_stats() {
        let input = "0\t0\tunchanged_but_present.rs\n";
        let result = parse_numstat(input);
        assert_eq!(
            lookup(&result.entries, "unchanged_but_present.rs"),
            Some(&DiffStat {
                added: 0,
                deleted: 0
            })
        );
    }

    #[test]
    fn test_duplicate_untracked_entries() {
        // Regression test for ZED-2XA: git can produce duplicate untracked entries
        // for the same path. This should deduplicate them instead of panicking.
        let input = "?? file.txt\0?? file.txt";
        let status: GitStatus = input.parse().unwrap();
        assert_eq!(status.entries.len(), 1);
        assert_eq!(status.entries[0].1, FileStatus::Untracked);
    }

    #[test]
    fn test_tree_diff_parsing() {
        let input = ":000000 100644 0000000000000000000000000000000000000000 0062c311b8727c3a2e3cd7a41bc9904feacf8f98 A\x00.zed/settings.json\x00".to_owned() +
            ":100644 000000 bb3e9ed2e97a8c02545bae243264d342c069afb3 0000000000000000000000000000000000000000 D\x00README.md\x00" +
            ":100644 100644 42f097005a1f21eb2260fad02ec8c991282beee8 a437d85f63bb8c62bd78f83f40c506631fabf005 M\x00parallel.go\x00";

        let output: TreeDiff = input.parse().unwrap();
        assert_eq!(
            output,
            TreeDiff {
                entries: [
                    (
                        RepoPath::new(".zed/settings.json").unwrap(),
                        TreeDiffStatus::Added,
                    ),
                    (
                        RepoPath::new("README.md").unwrap(),
                        TreeDiffStatus::Deleted {
                            old: "bb3e9ed2e97a8c02545bae243264d342c069afb3".parse().unwrap()
                        }
                    ),
                    (
                        RepoPath::new("parallel.go").unwrap(),
                        TreeDiffStatus::Modified {
                            old: "42f097005a1f21eb2260fad02ec8c991282beee8".parse().unwrap(),
                        }
                    ),
                ]
                .into_iter()
                .collect()
            }
        )
    }
}
