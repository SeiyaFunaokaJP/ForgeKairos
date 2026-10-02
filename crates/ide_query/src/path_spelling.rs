use std::sync::Arc;

use gpui::App;
use project::{Project, ProjectPath, Worktree, WorktreeSettings};
use settings::Settings as _;
use unicode_normalization::UnicodeNormalization as _;
use util::{
    paths::{PathMatcher, PathStyle},
    rel_path::{RelPath, RelPathBuf},
};

/// How a path an agent gave relates to the files it can reach. A file system that ignores
/// case opens `secret.rs` for `SECRET.rs`, while the worktree's entries and the settings'
/// patterns tell the two apart, so a judgement made on the spelling given would let the
/// file through.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathSpelling {
    /// The path is spelled as the worktree spells an entry: it names exactly the file it opens.
    AsGiven,
    /// The file system ignores case. This is the path as the worktree spells it, as far as
    /// the worktree has entries for it; the components below the last entry stay as given
    /// (on Windows, as Windows reads them: see [`win32_name`]).
    OnDisk(ProjectPath),
    /// Whether the file system ignores case is not known here (the worktree lives on a remote
    /// host, or the local check doesn't speak for the worktree's volume). The path is opened
    /// as given, but judged as every file it could reach.
    Uncertain(ProjectPath),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CaseRule {
    Insensitive,
    Unknown,
}

pub(crate) fn case_rule(worktree: &Worktree) -> CaseRule {
    match worktree.as_local() {
        // The local check creates its test files in the system's temporary directory, not on
        // the worktree's volume, so "sensitive" says nothing of a Windows drive mounted under
        // Linux or of a FAT disk. A Linux host can serve such a volume too.
        Some(local) if !local.fs_is_case_sensitive() => CaseRule::Insensitive,
        _ => CaseRule::Unknown,
    }
}

pub fn path_spelling(project: &Project, path: &ProjectPath, cx: &App) -> PathSpelling {
    let Some(worktree) = project.worktree_for_id(path.worktree_id, cx) else {
        return PathSpelling::AsGiven;
    };
    let worktree = worktree.read(cx);
    path_spelling_under(worktree, case_rule(worktree), path)
}

pub(crate) fn path_spelling_under(
    worktree: &Worktree,
    case_rule: CaseRule,
    path: &ProjectPath,
) -> PathSpelling {
    if worktree.entry_for_path(&path.path).is_some() {
        return PathSpelling::AsGiven;
    }
    let respelled = ProjectPath {
        worktree_id: path.worktree_id,
        path: respell(worktree, &path.path),
    };
    match case_rule {
        CaseRule::Insensitive => PathSpelling::OnDisk(respelled),
        CaseRule::Unknown => PathSpelling::Uncertain(respelled),
    }
}

/// One spelling for names a case-insensitive file system takes as the same. It folds more
/// than any one file system does (`ı` and `i`, `ß` and `ss`), which only makes more names
/// compare equal; NFC because macOS takes the composed and decomposed forms as one name.
fn fold(text: &str) -> String {
    text.nfc()
        .flat_map(char::to_uppercase)
        .flat_map(char::to_lowercase)
        .collect()
}

/// The name Windows opens for `component`: it drops trailing dots and spaces, and a stream
/// named after a `:` belongs to the file named before it (`secret.pem::$DATA` is the file).
fn win32_name(component: &str) -> &str {
    let name = component
        .split_once(':')
        .map_or(component, |(name, _)| name);
    let trimmed = name.trim_end_matches(['.', ' ']);
    if trimmed.is_empty() {
        component
    } else {
        trimmed
    }
}

/// Whether Windows may take `component` as the short (8.3) name of a file with a longer one,
/// as `SECRET~1.YML` for `secrets.yml`. Nothing in the worktree maps one to the other.
fn may_be_short_name(component: &str) -> bool {
    component
        .split('~')
        .skip(1)
        .any(|after| after.starts_with(|character: char| character.is_ascii_digit()))
}

fn reads_win32_names(worktree: &Worktree) -> bool {
    worktree.path_style() == PathStyle::Windows
}

/// Whether `spelling` may reach a file by its short name: a component the worktree has no
/// entry for looks like one, and so may open any file of its directory.
pub(crate) fn may_name_a_file_by_its_short_name(
    worktree: Option<&Worktree>,
    spelling: &PathSpelling,
) -> bool {
    let (Some(worktree), PathSpelling::OnDisk(respelled) | PathSpelling::Uncertain(respelled)) =
        (worktree, spelling)
    else {
        return false;
    };
    reads_win32_names(worktree)
        && respelled.path.ancestors().any(|ancestor| {
            ancestor.file_name().is_some_and(may_be_short_name)
                && worktree.entry_for_path(ancestor).is_none()
        })
}

fn respell(worktree: &Worktree, path: &RelPath) -> Arc<RelPath> {
    let win32 = reads_win32_names(worktree);
    let name_read = |component: &'_ str| -> String {
        if win32 {
            win32_name(component).to_string()
        } else {
            component.to_string()
        }
    };
    let mut respelled = RelPathBuf::new();
    let mut components = path.components();
    while let Some(component) = components.next() {
        let mut exact = respelled.clone();
        if exact.push_component(component).is_err() {
            return path.into_arc();
        }
        if worktree.entry_for_path(&exact).is_some() {
            respelled = exact;
            continue;
        }
        let name = name_read(component);
        let folded_name = fold(&name);
        let found = worktree.child_entries(&respelled).find_map(|entry| {
            let entry_name = entry.path.file_name()?;
            (fold(entry_name) == folded_name).then(|| entry.path.clone())
        });
        match found {
            Some(entry_path) => respelled = entry_path.to_rel_path_buf(),
            None => {
                for unmatched in std::iter::once(name).chain(components.map(name_read)) {
                    if respelled.push_component(&unmatched).is_err() {
                        return path.into_arc();
                    }
                }
                return respelled.as_rel_path().into_arc();
            }
        }
    }
    respelled.as_rel_path().into_arc()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PathSetting {
    PrivateFiles,
    FileScanExclusions,
}

impl PathSetting {
    fn matcher(self, settings: &WorktreeSettings) -> &PathMatcher {
        match self {
            PathSetting::PrivateFiles => &settings.private_files,
            PathSetting::FileScanExclusions => &settings.file_scan_exclusions,
        }
    }

    fn matches_as_spelled(self, worktree: Option<&Worktree>, path: &ProjectPath, cx: &App) -> bool {
        let entry_is_private = self == PathSetting::PrivateFiles
            && worktree
                .and_then(|worktree| worktree.entry_for_path(&path.path))
                .is_some_and(|entry| entry.is_private);
        entry_is_private
            || [
                WorktreeSettings::get_global(cx),
                WorktreeSettings::get(Some(path.into()), cx),
            ]
            .into_iter()
            .any(|settings| {
                let matcher = self.matcher(settings);
                path.path
                    .ancestors()
                    .any(|ancestor| matcher.is_match(ancestor))
            })
    }

    /// Fails closed: a pattern that no longer parses once folded counts as a match.
    fn matches_folded(self, path: &ProjectPath, cx: &App) -> bool {
        let folded_text = fold(path.path.as_unix_str());
        let Ok(folded_path) = RelPath::from_unix_str(&folded_text) else {
            return true;
        };
        [
            WorktreeSettings::get_global(cx),
            WorktreeSettings::get(Some(path.into()), cx),
        ]
        .into_iter()
        .any(|settings| {
            let matcher = self.matcher(settings);
            if matcher.sources().next().is_none() {
                return false;
            }
            match PathMatcher::new(matcher.sources().map(fold), PathStyle::local()) {
                Ok(folded_matcher) => folded_path
                    .ancestors()
                    .any(|ancestor| folded_matcher.is_match(ancestor)),
                Err(error) => {
                    log::error!("the folded {self:?} patterns don't parse: {error}");
                    true
                }
            }
        })
    }

    /// Whether the setting covers the file `path` opens. Only the given spelling is matched
    /// where case is told apart; elsewhere the worktree's spelling too, and the patterns are
    /// folded to one case for whatever part of the path the worktree has no entry for.
    pub(crate) fn matches(
        self,
        worktree: Option<&Worktree>,
        path: &ProjectPath,
        spelling: &PathSpelling,
        cx: &App,
    ) -> bool {
        if self.matches_as_spelled(worktree, path, cx)
            || may_name_a_file_by_its_short_name(worktree, spelling)
        {
            return true;
        }
        match spelling {
            PathSpelling::AsGiven => false,
            PathSpelling::OnDisk(respelled) => {
                self.matches_as_spelled(worktree, respelled, cx)
                    || (worktree
                        .and_then(|worktree| worktree.entry_for_path(&respelled.path))
                        .is_none()
                        && self.matches_folded(respelled, cx))
            }
            PathSpelling::Uncertain(respelled) => {
                self.matches_as_spelled(worktree, respelled, cx)
                    || self.matches_folded(respelled, cx)
            }
        }
    }

    fn matches_in_worktree(self, worktree: &Worktree, path: &ProjectPath, cx: &App) -> bool {
        let spelling = path_spelling_under(worktree, case_rule(worktree), path);
        self.matches(Some(worktree), path, &spelling, cx)
    }

    fn matches_in_project(self, project: &Project, path: &ProjectPath, cx: &App) -> bool {
        match project.worktree_for_id(path.worktree_id, cx) {
            Some(worktree) => self.matches_in_worktree(worktree.read(cx), path, cx),
            None => self.matches(None, path, &PathSpelling::AsGiven, cx),
        }
    }
}

/// Whether `path` is covered by `private_files`, in whatever spelling the file system takes
/// it to the file.
pub fn matches_private_files(project: &Project, path: &ProjectPath, cx: &App) -> bool {
    PathSetting::PrivateFiles.matches_in_project(project, path, cx)
}

/// [`matches_private_files`] for a path in `worktree`.
pub fn worktree_matches_private_files(worktree: &Worktree, path: &ProjectPath, cx: &App) -> bool {
    PathSetting::PrivateFiles.matches_in_worktree(worktree, path, cx)
}

/// Whether `path` is covered by `file_scan_exclusions`, in whatever spelling the file system
/// takes it to the file.
pub fn matches_file_scan_exclusions(project: &Project, path: &ProjectPath, cx: &App) -> bool {
    PathSetting::FileScanExclusions.matches_in_project(project, path, cx)
}
