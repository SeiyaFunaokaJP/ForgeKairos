use anyhow::Result;
use serde_json::Value;

use crate::migrations::migrate_settings;

const AGENT_KEY: &str = "agent";
const SANDBOX_PERMISSIONS_KEY: &str = "sandbox_permissions";
const WRITE_PATHS_KEY: &str = "write_paths";
const REQUESTED_KEY: &str = "requested";
const RESOLVED_KEY: &str = "resolved";

/// ADR-F49 decision 34: rewrite sandbox write grants that name a folder in a
/// WSL distribution into the one spelling grants are now stored and compared
/// in (`\\wsl.localhost\<distro in lower case>\…`), and drop the rows that
/// become duplicates of an earlier one.
///
/// A grant approved before that decision recorded the distribution as
/// `wsl.exe` reported it (`\\wsl.localhost\Ubuntu\…`), and grants are compared
/// as plain paths, so approving the same folder again did not match: the user
/// was asked a second time and the settings file kept both rows. Folding the
/// rows makes the old grant cover the new one and leaves one row per folder.
///
/// Only the server name and the distribution's own case change; the names
/// below it are left exactly as written, because they are case-sensitive on
/// the distribution's filesystem. A path that is not a folder in a
/// distribution — a Windows drive, another server's share, a bare Linux path —
/// is left untouched.
pub fn fold_wsl_sandbox_grant_spellings(value: &mut Value) -> Result<()> {
    migrate_settings(value, &mut migrate_one)
}

fn migrate_one(object: &mut serde_json::Map<String, Value>) -> Result<()> {
    let Some(write_paths) = object
        .get_mut(AGENT_KEY)
        .and_then(|agent| agent.as_object_mut())
        .and_then(|agent| agent.get_mut(SANDBOX_PERMISSIONS_KEY))
        .and_then(|permissions| permissions.as_object_mut())
        .and_then(|permissions| permissions.get_mut(WRITE_PATHS_KEY))
        .and_then(|paths| paths.as_array_mut())
    else {
        return Ok(());
    };

    for entry in write_paths.iter_mut() {
        match entry {
            Value::String(path) => fold(path),
            Value::Object(entry) => {
                for key in [REQUESTED_KEY, RESOLVED_KEY] {
                    if let Some(Value::String(path)) = entry.get_mut(key) {
                        fold(path);
                    }
                }
            }
            _ => {}
        }
    }

    // What is enforced for a row is its resolved canonical when it has one, so
    // that is what makes two rows the same grant. The first row wins: it is
    // the one the rest of the file (and the settings UI's own list) was
    // written against.
    let mut enforced: Vec<String> = Vec::new();
    write_paths.retain(|entry| {
        let Some(path) = enforced_path(entry) else {
            return true;
        };
        let duplicate = enforced.iter().any(|seen| seen == &path);
        if !duplicate {
            enforced.push(path);
        }
        !duplicate
    });

    Ok(())
}

fn fold(path: &mut String) {
    if let Some(folded) = util::path_mapping::canonical_wsl_grant_spelling(path) {
        *path = folded;
    }
}

/// The path a row grants write access to: its resolved canonical target, or
/// the requested path when it has none (a hand-written row, resolved afresh
/// when a command runs).
fn enforced_path(entry: &Value) -> Option<String> {
    match entry {
        Value::String(path) => Some(path.clone()),
        Value::Object(entry) => match entry.get(RESOLVED_KEY) {
            Some(Value::String(resolved)) => Some(resolved.clone()),
            _ => match entry.get(REQUESTED_KEY) {
                Some(Value::String(requested)) => Some(requested.clone()),
                _ => None,
            },
        },
        _ => None,
    }
}
