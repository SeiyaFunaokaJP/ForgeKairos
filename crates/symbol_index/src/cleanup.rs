//! Removing the indexes of folders that have not been opened for a long time, so that the
//! directory holding one index per folder ever opened does not grow without end.
//!
//! Only what is certainly an unused index is removed: a directory named like one, holding only
//! the files an index writes, whose lock can be taken. Links are not followed.

use std::{
    fs::{self, File},
    io,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use crate::{LAST_USED_FILE_NAME, LOCK_FILE_NAME, storage::MANIFEST_FILE_NAME};

/// The length of an index directory's name: hexadecimal digits of a hash.
pub const INDEX_DIRECTORY_NAME_LENGTH: usize = 32;

/// What [`remove_stale_indexes`] did with one directory under the root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StaleIndexOutcome {
    Removed,
    /// Used more recently than the age limit, or with no time to go by.
    Recent,
    /// Its lock is held: by another process, another window, or this one.
    InUse,
    /// Named in `keep`.
    Kept,
    /// It holds something an index does not write.
    UnknownFiles,
    /// Not a directory named like an index (or a link to one).
    NotAnIndex,
    Failed(String),
}

/// Whether `name` is a file the index writes into its directory.
pub fn is_index_file_name(name: &str) -> bool {
    let is_segment = |name: &str| {
        name.strip_prefix("seg-")
            .and_then(|rest| rest.strip_suffix(".bin"))
            .is_some_and(|hex| !hex.is_empty() && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
    };
    name == MANIFEST_FILE_NAME
        || name == LOCK_FILE_NAME
        || name == LAST_USED_FILE_NAME
        || is_segment(name)
        || name
            .strip_suffix(".tmp")
            .is_some_and(|name| name == MANIFEST_FILE_NAME || is_segment(name))
}

fn is_index_directory_name(name: &str) -> bool {
    name.len() == INDEX_DIRECTORY_NAME_LENGTH
        && name
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// When the index in `directory` was last opened: the time its marker was written. `None`
/// without a marker, which keeps the index: one written by a Zed from before the marker (still
/// installed, say) is opened without ever leaving one, and its manifest changes only with
/// the files.
pub fn index_last_used(directory: &Path) -> Option<SystemTime> {
    fs::symlink_metadata(directory.join(LAST_USED_FILE_NAME))
        .ok()
        .filter(|metadata| metadata.is_file())
        .and_then(|metadata| metadata.modified().ok())
}

/// Removes each index directory directly under `root` that was last used more than `max_age`
/// before `now`, unless it is in `keep`, its lock is held, or it holds anything an index does
/// not write. The lock is held while the files are removed, so that no Zed opens the index
/// meanwhile.
pub fn remove_stale_indexes(
    root: &Path,
    now: SystemTime,
    max_age: Duration,
    keep: &[PathBuf],
) -> Vec<(PathBuf, StaleIndexOutcome)> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) => {
            if error.kind() != io::ErrorKind::NotFound {
                log::warn!(
                    "symbol_index: cannot list {} to remove old indexes: {error}",
                    root.display()
                );
            }
            return Vec::new();
        }
    };
    let mut outcomes = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                log::warn!("symbol_index: listing {}: {error}", root.display());
                continue;
            }
        };
        let path = entry.path();
        let outcome = stale_index_outcome(&entry, now, max_age, keep);
        match &outcome {
            StaleIndexOutcome::Removed => {
                log::info!("symbol_index: removed the unused index {}", path.display())
            }
            StaleIndexOutcome::Failed(error) => log::warn!(
                "symbol_index: cannot remove the unused index {}: {error}",
                path.display()
            ),
            _ => {}
        }
        outcomes.push((path, outcome));
    }
    outcomes
}

fn stale_index_outcome(
    entry: &fs::DirEntry,
    now: SystemTime,
    max_age: Duration,
    keep: &[PathBuf],
) -> StaleIndexOutcome {
    let path = entry.path();
    let is_directory = fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_dir());
    let named_like_one = entry
        .file_name()
        .to_str()
        .is_some_and(is_index_directory_name);
    if !is_directory || !named_like_one {
        return StaleIndexOutcome::NotAnIndex;
    }
    if keep.iter().any(|kept| kept == &path) {
        return StaleIndexOutcome::Kept;
    }
    let old_enough = index_last_used(&path).is_some_and(|last_used| {
        now.duration_since(last_used)
            .is_ok_and(|age| age >= max_age)
    });
    if !old_enough {
        return StaleIndexOutcome::Recent;
    }
    match index_file_names(&path) {
        Ok(Some(_)) => {}
        Ok(None) => return StaleIndexOutcome::UnknownFiles,
        Err(error) => return StaleIndexOutcome::Failed(error.to_string()),
    }
    match remove_locked(&path, &|| {}) {
        Ok(outcome) => outcome,
        Err(error) => StaleIndexOutcome::Failed(error.to_string()),
    }
}

/// The names in `directory`, when they are all regular files with the names an index writes.
fn index_file_names(directory: &Path) -> io::Result<Option<Vec<String>>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let is_file = fs::symlink_metadata(entry.path())?.is_file();
        let name = entry
            .file_name()
            .into_string()
            .ok()
            .filter(|name| is_index_file_name(name));
        match name {
            Some(name) if is_file => names.push(name),
            _ => return Ok(None),
        }
    }
    Ok(Some(names))
}

/// `after_check` runs between the last look at the files and their removal, for tests.
fn remove_locked(directory: &Path, after_check: &dyn Fn()) -> io::Result<StaleIndexOutcome> {
    let lock_path = directory.join(LOCK_FILE_NAME);
    // Created when missing, as opening the index does, so that an open starting meanwhile
    // finds the lock taken instead of making its own.
    let lock_file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)?;
    match lock_file.try_lock() {
        Ok(()) => {}
        Err(fs::TryLockError::WouldBlock) => return Ok(StaleIndexOutcome::InUse),
        Err(fs::TryLockError::Error(error)) => return Err(error),
    }
    // Checked again under the lock: what was an index a moment ago may have been opened and
    // written to since. Only the files seen here are removed; anything that appears later
    // stays, and keeps the directory.
    let Some(mut names) = index_file_names(directory)? else {
        return Ok(StaleIndexOutcome::UnknownFiles);
    };
    after_check();
    names.retain(|name| name != LOCK_FILE_NAME);
    // The manifest first, so that a directory left half removed opens as an empty index.
    names.sort_by_key(|name| name != MANIFEST_FILE_NAME);
    for name in names {
        remove_if_present(&directory.join(name))?;
    }
    // Windows does not remove a file that is open; elsewhere the lock is removed while held.
    if cfg!(windows) {
        drop(lock_file);
        remove_if_present(&lock_path)?;
    } else {
        remove_if_present(&lock_path)?;
        drop(lock_file);
    }
    // Fails, leaving the directory, when something was added after the check.
    fs::remove_dir(directory)?;
    Ok(StaleIndexOutcome::Removed)
}

fn remove_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{IndexConfig, SymbolIndex};

    const DAY: Duration = Duration::from_secs(24 * 60 * 60);
    const MAX_AGE: Duration = Duration::from_secs(90 * 24 * 60 * 60);

    fn index_name(digit: char) -> String {
        std::iter::repeat_n(digit, INDEX_DIRECTORY_NAME_LENGTH).collect()
    }

    /// An index directory under `root`, opened and closed once.
    fn closed_index(root: &Path, name: &str) -> PathBuf {
        let directory = root.join(name);
        drop(SymbolIndex::open(&directory, IndexConfig::default()).expect("opening an index"));
        directory
    }

    fn outcome_of(
        outcomes: &[(PathBuf, StaleIndexOutcome)],
        directory: &Path,
    ) -> Option<StaleIndexOutcome> {
        outcomes
            .iter()
            .find(|(path, _)| path == directory)
            .map(|(_, outcome)| outcome.clone())
    }

    #[test]
    fn only_indexes_unused_for_long_enough_are_removed() {
        let root = tempfile::tempdir().expect("a temporary directory");
        let index = closed_index(root.path(), &index_name('a'));
        let now = SystemTime::now();

        let outcomes = remove_stale_indexes(root.path(), now + DAY * 89, MAX_AGE, &[]);
        assert_eq!(
            outcome_of(&outcomes, &index),
            Some(StaleIndexOutcome::Recent)
        );
        assert!(index.join(LAST_USED_FILE_NAME).exists());

        let outcomes = remove_stale_indexes(root.path(), now + DAY * 91, MAX_AGE, &[]);
        assert_eq!(
            outcome_of(&outcomes, &index),
            Some(StaleIndexOutcome::Removed)
        );
        assert!(!index.exists());

        // Opening the folder again builds a new, empty index there.
        let reopened = SymbolIndex::open(&index, IndexConfig::default()).expect("reopening");
        assert_eq!(reopened.stats().files, 0);
    }

    #[test]
    fn opening_an_index_marks_it_used() {
        let root = tempfile::tempdir().expect("a temporary directory");
        let index = closed_index(root.path(), &index_name('b'));
        let long_ago = SystemTime::now() - DAY * 200;
        for name in [LAST_USED_FILE_NAME, MANIFEST_FILE_NAME] {
            if !index.join(name).exists() {
                continue;
            }
            File::options()
                .write(true)
                .open(index.join(name))
                .and_then(|file| file.set_modified(long_ago))
                .expect("backdating a file");
        }
        assert!(index_last_used(&index).is_some_and(|used| used <= long_ago));
        drop(SymbolIndex::open(&index, IndexConfig::default()).expect("opening"));
        let used = index_last_used(&index).expect("a time");
        assert!(used > long_ago + DAY * 100, "the open is recorded");
        let outcomes = remove_stale_indexes(root.path(), SystemTime::now(), MAX_AGE, &[]);
        assert_eq!(
            outcome_of(&outcomes, &index),
            Some(StaleIndexOutcome::Recent)
        );
    }

    #[test]
    fn an_index_in_use_or_kept_is_left_alone() {
        let root = tempfile::tempdir().expect("a temporary directory");
        let open_directory = root.path().join(index_name('c'));
        let open = SymbolIndex::open(&open_directory, IndexConfig::default()).expect("opening");
        let kept = closed_index(root.path(), &index_name('d'));
        let later = SystemTime::now() + DAY * 365;

        let outcomes =
            remove_stale_indexes(root.path(), later, MAX_AGE, std::slice::from_ref(&kept));
        assert_eq!(
            outcome_of(&outcomes, &open_directory),
            Some(StaleIndexOutcome::InUse)
        );
        assert_eq!(outcome_of(&outcomes, &kept), Some(StaleIndexOutcome::Kept));
        assert!(open_directory.join(LAST_USED_FILE_NAME).exists());
        assert!(kept.join(LAST_USED_FILE_NAME).exists());
        assert_eq!(open.stats().files, 0, "the open index still works");

        drop(open);
        let outcomes = remove_stale_indexes(root.path(), later, MAX_AGE, &[]);
        assert_eq!(
            outcome_of(&outcomes, &open_directory),
            Some(StaleIndexOutcome::Removed)
        );
        assert_eq!(
            outcome_of(&outcomes, &kept),
            Some(StaleIndexOutcome::Removed)
        );
    }

    #[test]
    fn a_directory_with_other_files_or_another_name_is_left_alone() {
        let root = tempfile::tempdir().expect("a temporary directory");
        let later = SystemTime::now() + DAY * 365;
        let with_notes = closed_index(root.path(), &index_name('e'));
        fs::write(with_notes.join("notes.txt"), "mine").expect("writing a file");
        let with_folder = closed_index(root.path(), &index_name('f'));
        fs::create_dir(with_folder.join("seg-0000000000000001.bin")).expect("a folder");
        let upper_case = closed_index(root.path(), &index_name('A'));
        let short = closed_index(root.path(), &"a".repeat(INDEX_DIRECTORY_NAME_LENGTH - 1));
        let not_hex = closed_index(root.path(), &index_name('g'));
        let plain_file = root.path().join(index_name('1'));
        fs::write(&plain_file, "not a directory").expect("writing a file");

        let outcomes = remove_stale_indexes(root.path(), later, MAX_AGE, &[]);
        assert_eq!(
            outcome_of(&outcomes, &with_notes),
            Some(StaleIndexOutcome::UnknownFiles)
        );
        assert_eq!(
            outcome_of(&outcomes, &with_folder),
            Some(StaleIndexOutcome::UnknownFiles)
        );
        for path in [&upper_case, &short, &not_hex, &plain_file] {
            assert_eq!(
                outcome_of(&outcomes, path),
                Some(StaleIndexOutcome::NotAnIndex),
                "{}",
                path.display()
            );
        }
        assert_eq!(
            fs::read_to_string(with_notes.join("notes.txt"))
                .ok()
                .as_deref(),
            Some("mine")
        );
        for path in [&with_notes, &with_folder, &upper_case, &short, &not_hex] {
            assert!(
                path.join(LAST_USED_FILE_NAME).exists(),
                "{}",
                path.display()
            );
        }
        assert!(plain_file.exists());
    }

    #[test]
    fn links_are_not_followed() {
        let root = tempfile::tempdir().expect("a temporary directory");
        let elsewhere = tempfile::tempdir().expect("a temporary directory");
        let target = closed_index(elsewhere.path(), &index_name('7'));
        let link = root.path().join(index_name('8'));
        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(&target, &link);
        #[cfg(windows)]
        let linked = std::os::windows::fs::symlink_dir(&target, &link).or_else(|_| {
            // A junction needs no privilege, and is followed like a link to a directory.
            #[allow(
                clippy::disallowed_methods,
                reason = "a test waiting for one short command"
            )]
            let status = std::process::Command::new("cmd")
                .arg("/C")
                .arg("mklink")
                .arg("/J")
                .arg(&link)
                .arg(&target)
                .output()?
                .status;
            if status.success() {
                Ok(())
            } else {
                Err(io::Error::other(format!("mklink /J: {status}")))
            }
        });
        if let Err(error) = linked {
            // Windows lets only some users make links.
            eprintln!("skipping the linked directory: {error}");
        } else {
            let outcomes =
                remove_stale_indexes(root.path(), SystemTime::now() + DAY * 365, MAX_AGE, &[]);
            assert_eq!(
                outcome_of(&outcomes, &link),
                Some(StaleIndexOutcome::NotAnIndex)
            );
            assert!(target.join(LAST_USED_FILE_NAME).exists());
        }

        // A link inside an index directory, named like an index file.
        let index = closed_index(root.path(), &index_name('9'));
        let outside_file = elsewhere.path().join("keep.bin");
        fs::write(&outside_file, "keep").expect("writing a file");
        let link_in_index = index.join("seg-00000000000000ff.bin");
        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(&outside_file, &link_in_index);
        #[cfg(windows)]
        let linked = std::os::windows::fs::symlink_file(&outside_file, &link_in_index);
        if let Err(error) = linked {
            eprintln!("skipping the linked file: {error}");
        } else {
            let outcomes =
                remove_stale_indexes(root.path(), SystemTime::now() + DAY * 365, MAX_AGE, &[]);
            assert_eq!(
                outcome_of(&outcomes, &index),
                Some(StaleIndexOutcome::UnknownFiles)
            );
        }
        assert_eq!(
            fs::read_to_string(&outside_file).ok().as_deref(),
            Some("keep")
        );
    }

    #[test]
    fn an_index_without_a_marker_is_kept() {
        let root = tempfile::tempdir().expect("a temporary directory");
        let unmarked = closed_index(root.path(), &index_name('3'));
        let marked = closed_index(root.path(), &index_name('4'));
        fs::remove_file(unmarked.join(LAST_USED_FILE_NAME)).expect("removing the marker");
        fs::write(unmarked.join(MANIFEST_FILE_NAME), "old").expect("writing a manifest");
        let long_ago = SystemTime::now() - DAY * 400;
        for path in [
            unmarked.join(MANIFEST_FILE_NAME),
            marked.join(LAST_USED_FILE_NAME),
        ] {
            File::options()
                .write(true)
                .open(&path)
                .and_then(|file| file.set_modified(long_ago))
                .expect("backdating a file");
        }
        assert_eq!(index_last_used(&unmarked), None);

        let outcomes = remove_stale_indexes(root.path(), SystemTime::now(), MAX_AGE, &[]);
        assert_eq!(
            outcome_of(&outcomes, &unmarked),
            Some(StaleIndexOutcome::Recent)
        );
        assert!(unmarked.join(MANIFEST_FILE_NAME).exists());
        assert_eq!(
            outcome_of(&outcomes, &marked),
            Some(StaleIndexOutcome::Removed)
        );
        assert!(!marked.exists());
    }

    #[test]
    fn only_the_files_checked_under_the_lock_are_removed() {
        let root = tempfile::tempdir().expect("a temporary directory");
        let index = closed_index(root.path(), &index_name('5'));
        let segment = index.join("seg-0000000000000001.bin");
        fs::write(&segment, "segment").expect("writing a segment");
        let added = index.join("notes.txt");
        let late_segment = index.join("seg-0000000000000002.bin");
        let outcome = remove_locked(&index, &|| {
            fs::write(&added, "mine").expect("writing a file");
            fs::write(&late_segment, "late").expect("writing a segment");
        });
        assert!(outcome.is_err(), "{outcome:?}");
        assert!(!segment.exists(), "a file seen in the check is removed");
        assert_eq!(
            fs::read_to_string(&added).ok().as_deref(),
            Some("mine"),
            "a file added after the check stays"
        );
        assert!(late_segment.exists(), "even one named like an index file");
    }

    #[test]
    fn an_index_without_a_lock_file_is_locked_while_it_is_removed() {
        let root = tempfile::tempdir().expect("a temporary directory");
        let index = closed_index(root.path(), &index_name('6'));
        fs::remove_file(index.join(LOCK_FILE_NAME)).expect("removing the lock file");
        let observed = std::cell::Cell::new(None);
        let outcome = remove_locked(&index, &|| {
            observed.set(Some(
                SymbolIndex::open(&index, IndexConfig::default()).is_err(),
            ));
        })
        .expect("removing the index");
        assert_eq!(outcome, StaleIndexOutcome::Removed);
        assert_eq!(
            observed.get(),
            Some(true),
            "an open meanwhile finds the index locked"
        );
    }

    #[test]
    fn index_file_names_are_recognized() {
        for name in [
            "manifest.bin",
            "manifest.bin.tmp",
            "lock",
            "last-used",
            "seg-00000000000000a1.bin",
            "seg-00000000000000a1.bin.tmp",
        ] {
            assert!(is_index_file_name(name), "{name}");
        }
        for name in [
            "seg-.bin",
            "seg-xyz.bin",
            "notes.txt",
            "lock.tmp",
            "manifest",
            ".DS_Store",
        ] {
            assert!(!is_index_file_name(name), "{name}");
        }
    }
}
