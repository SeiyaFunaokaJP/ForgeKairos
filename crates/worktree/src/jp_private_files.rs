use collections::HashMap;
use rpc::proto;
use util::rel_path::RelPath;

use crate::WorktreeSettings;

/// zed-jp-enhance: marks as private the entries of a remote worktree update
/// that this side's own `private_files` covers, on top of those the host
/// marked.
///
/// The host's verdict alone leaves an entry open to agents and edit
/// prediction whenever the flag does not arrive: a host whose wire revision
/// numbers the field differently, a collab server that rebuilds entries
/// without it, or a host whose settings have not caught up with this side's
/// yet. Either side calling an entry private is enough. Patterns are matched
/// against the worktree-relative path, which reads the same on every host.
pub(crate) fn mark_private_entries(
    update: &mut proto::UpdateWorktree,
    settings: &WorktreeSettings,
) {
    // The first update of a large worktree carries hundreds of thousands of
    // entries that share a few thousand directories: matching every ancestor
    // of every entry would more than double what applying it costs.
    let mut directory_verdicts = HashMap::default();
    for entry in &mut update.updated_entries {
        if entry.is_private {
            continue;
        }
        // The root of a worktree opened on a single file has the empty path,
        // which no pattern matches; the host judges that file by its name.
        let path = if entry.path.is_empty() && !entry.is_dir {
            update.root_name.as_str()
        } else {
            entry.path.as_str()
        };
        // An entry whose path does not parse is dropped when the update is
        // applied, so there is nothing to mark.
        if let Ok(path) = RelPath::from_unix_str(path)
            && is_path_private(path, settings, &mut directory_verdicts)
        {
            entry.is_private = true;
        }
    }
}

/// `WorktreeSettings::is_path_private`, remembering the verdict on each
/// directory it passes through.
fn is_path_private(
    path: &RelPath,
    settings: &WorktreeSettings,
    directory_verdicts: &mut HashMap<String, bool>,
) -> bool {
    if settings.private_files.is_match(path) {
        return true;
    }
    let Some(parent) = path.parent() else {
        return false;
    };
    if let Some(verdict) = directory_verdicts.get(parent.as_unix_str()) {
        return *verdict;
    }
    let verdict = is_path_private(parent, settings, directory_verdicts);
    directory_verdicts.insert(parent.as_unix_str().to_string(), verdict);
    verdict
}
