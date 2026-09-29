use anyhow::Context as _;

use git::repository::{Remote, RemoteCommandOutput};
use ui::SharedString;
use util::ResultExt as _;

/// 第 1 要素は git の stderr と突き合わせる照合文字列なので**必ず英語のまま**にする。
/// 第 2 要素はトースト上のボタンラベル (表示専用) なので翻訳する。
const PULL_REQUEST_HINTS: &[(&str, &str)] = &[
    // GitHub: "Create a pull request for 'branch' on GitHub by visiting:"
    ("Create a pull request", i18n::msg!("Create Pull Request")),
    // Bitbucket: "Create pull request for branch:"
    ("Create pull request", i18n::msg!("Create Pull Request")),
    // GitLab: "To create a merge request for branch, visit:"
    ("create a merge request", i18n::msg!("Create Merge Request")),
    // GitLab: "View merge request for branch:"
    ("View merge request", i18n::msg!("View Merge Request")),
];

#[derive(Clone)]
pub enum RemoteAction {
    Fetch(Option<Remote>),
    Pull(Remote),
    Push(SharedString, Remote),
}

impl RemoteAction {
    pub fn name(&self) -> &'static str {
        match self {
            RemoteAction::Fetch(_) => "fetch",
            RemoteAction::Pull(_) => "pull",
            RemoteAction::Push(_, _) => "push",
        }
    }
}

pub enum SuccessStyle {
    Toast,
    ToastWithLog { output: RemoteCommandOutput },
    PushPrLink { label: &'static str, url: String },
}

pub struct SuccessMessage {
    pub message: String,
    pub style: SuccessStyle,
}

fn extract_pull_request_link(output: &RemoteCommandOutput) -> Option<(&'static str, String)> {
    let mut pending_label: Option<&'static str> = None;

    for line in output.stderr.lines() {
        let Some(remote_line) = line.trim_start().strip_prefix("remote:") else {
            pending_label = None;
            continue;
        };

        if let Some((_, label)) = PULL_REQUEST_HINTS
            .iter()
            .find(|(hint, _)| remote_line.contains(hint))
        {
            pending_label = Some(label);
        }

        if let Some(url) = extract_url(remote_line)
            && let Some(label) = pending_label
        {
            return Some((label, url));
        }
    }

    None
}

fn extract_url(line: &str) -> Option<String> {
    let http_index = line.find("https://").or_else(|| line.find("http://"))?;
    let url = line[http_index..]
        .split_whitespace()
        .next()?
        .trim_end_matches(|character| matches!(character, ',' | '.' | ')' | ']' | '>'));

    Some(url.to_string())
}

pub fn format_output(action: &RemoteAction, output: RemoteCommandOutput) -> SuccessMessage {
    match action {
        RemoteAction::Fetch(remote) => {
            if output.stderr.is_empty() {
                SuccessMessage {
                    message: i18n::t!("Fetch: Already up to date").into(),
                    style: SuccessStyle::Toast,
                }
            } else {
                let message = match remote {
                    Some(remote) => i18n::t!("Synchronized with {}", remote.name),
                    None => i18n::t!("Synchronized with remotes").into(),
                };
                SuccessMessage {
                    message,
                    style: SuccessStyle::ToastWithLog { output },
                }
            }
        }
        RemoteAction::Pull(remote_ref) => {
            let get_changes = |output: &RemoteCommandOutput| -> anyhow::Result<u32> {
                let last_line = output
                    .stdout
                    .lines()
                    .last()
                    .context("Failed to get last line of output")?
                    .trim();

                let files_changed = last_line
                    .split_whitespace()
                    .next()
                    .context("Failed to get first word of last line")?
                    .parse()?;

                Ok(files_changed)
            };
            // 以下の `ends_with` / `starts_with` / `contains` は git の stdout と
            // 突き合わせる照合文字列なので英語のまま。
            if output.stdout.ends_with("Already up to date.\n") {
                SuccessMessage {
                    message: i18n::t!("Pull: Already up to date").into(),
                    style: SuccessStyle::Toast,
                }
            } else if output.stdout.starts_with("Updating") {
                let files_changed = get_changes(&output).log_err();
                let message = if let Some(files_changed) = files_changed {
                    i18n::tn!(
                        files_changed,
                        "Received {count} file change from {remote}",
                        "Received {count} file changes from {remote}",
                        remote = remote_ref.name
                    )
                } else {
                    i18n::t!("Fast forwarded from {}", remote_ref.name)
                };
                SuccessMessage {
                    message,
                    style: SuccessStyle::ToastWithLog { output },
                }
            } else if output.stdout.starts_with("Merge") {
                let files_changed = get_changes(&output).log_err();
                let message = if let Some(files_changed) = files_changed {
                    i18n::tn!(
                        files_changed,
                        "Merged {count} file change from {remote}",
                        "Merged {count} file changes from {remote}",
                        remote = remote_ref.name
                    )
                } else {
                    i18n::t!("Merged from {}", remote_ref.name)
                };
                SuccessMessage {
                    message,
                    style: SuccessStyle::ToastWithLog { output },
                }
            } else if output.stdout.contains("Successfully rebased") {
                SuccessMessage {
                    message: i18n::t!("Successfully rebased from {}", remote_ref.name),
                    style: SuccessStyle::ToastWithLog { output },
                }
            } else {
                SuccessMessage {
                    message: i18n::t!("Successfully pulled from {}", remote_ref.name),
                    style: SuccessStyle::ToastWithLog { output },
                }
            }
        }
        RemoteAction::Push(branch_name, remote_ref) => {
            // `Everything up-to-date` は git の stderr と突き合わせる照合文字列。
            if output.stderr.ends_with("Everything up-to-date\n") {
                SuccessMessage {
                    message: i18n::t!("Push: Everything is up-to-date").to_string(),
                    style: SuccessStyle::Toast,
                }
            } else if let Some((label, url)) = extract_pull_request_link(&output) {
                SuccessMessage {
                    message: i18n::t!("Pushed {} to {}", branch_name, remote_ref.name),
                    style: SuccessStyle::PushPrLink { label, url },
                }
            } else {
                SuccessMessage {
                    message: i18n::t!("Pushed {} to {}", branch_name, remote_ref.name),
                    style: SuccessStyle::ToastWithLog { output },
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;

    #[test]
    fn test_push_new_branch_pull_request() {
        let action = RemoteAction::Push(
            SharedString::new_static("test_branch"),
            Remote {
                name: SharedString::new_static("test_remote"),
            },
        );

        let output = RemoteCommandOutput {
            stdout: String::new(),
            stderr: indoc! { "
                Total 0 (delta 0), reused 0 (delta 0), pack-reused 0 (from 0)
                remote:
                remote: Create a pull request for 'test' on GitHub by visiting:
                remote:      https://example.com/test/test/pull/new/test
                remote:
                To example.com:test/test.git
                 * [new branch]      test -> test
                "}
            .to_string(),
        };

        let msg = format_output(&action, output);
        if let SuccessStyle::PushPrLink { label, url } = msg.style {
            assert_eq!(msg.message, "Pushed test_branch to test_remote");
            assert_eq!(label, "Create Pull Request");
            assert_eq!(url, "https://example.com/test/test/pull/new/test");
        } else {
            panic!("Expected PushPrLink variant");
        }
    }

    #[test]
    fn test_push_new_branch_merge_request() {
        let action = RemoteAction::Push(
            SharedString::new_static("test_branch"),
            Remote {
                name: SharedString::new_static("test_remote"),
            },
        );

        let output = RemoteCommandOutput {
            stdout: String::new(),
            stderr: indoc! {"
                Total 0 (delta 0), reused 0 (delta 0), pack-reused 0 (from 0)
                remote:
                remote: To create a merge request for test, visit:
                remote:   https://example.com/test/test/-/merge_requests/new?merge_request%5Bsource_branch%5D=test
                remote:
                To example.com:test/test.git
                 * [new branch]      test -> test
                "}
            .to_string()
            };

        let msg = format_output(&action, output);

        if let SuccessStyle::PushPrLink { label, url } = msg.style {
            assert_eq!(msg.message, "Pushed test_branch to test_remote");
            assert_eq!(label, "Create Merge Request");
            assert_eq!(
                url,
                "https://example.com/test/test/-/merge_requests/new?merge_request%5Bsource_branch%5D=test"
            )
        } else {
            panic!("Expected PushPrLink variant")
        }
    }

    #[test]
    fn test_push_new_branch_bitbucket_pull_request() {
        let output = RemoteCommandOutput {
            stdout: String::new(),
            stderr: indoc! {"
                remote:
                remote: Create pull request for test:
                remote:   https://bitbucket.example.com/projects/TEST/repos/test/pull-requests?create&sourceBranch=refs/heads/test
                "}
            .to_string(),
        };

        assert_eq!(
            extract_pull_request_link(&output),
            Some((
                "Create Pull Request",
                "https://bitbucket.example.com/projects/TEST/repos/test/pull-requests?create&sourceBranch=refs/heads/test".to_string()
            ))
        );
    }

    #[test]
    fn test_push_branch_existing_merge_request() {
        let action = RemoteAction::Push(
            SharedString::new_static("test_branch"),
            Remote {
                name: SharedString::new_static("test_remote"),
            },
        );

        let output = RemoteCommandOutput {
            stdout: String::new(),
            // Include an unrelated URL outside of the `remote:` lines, in this
            // case, an OpenSSH warning, to ensure that it is not mistaken for
            // the merge request link.
            stderr: indoc! {"
                ** WARNING: connection is not using a post-quantum key exchange algorithm.
                ** This session may be vulnerable to \"store now, decrypt later\" attacks.
                ** The server may need to be upgraded. See https://openssh.com/pq.html
                Total 0 (delta 0), reused 0 (delta 0), pack-reused 0 (from 0)
                remote:
                remote: View merge request for test:
                remote:    https://example.com/test/test/-/merge_requests/99999
                remote:
                To example.com:test/test.git
                    + 80bd3c83be...e03d499d2e test -> test
                "}
            .to_string(),
        };

        let msg = format_output(&action, output);

        if let SuccessStyle::PushPrLink { label, url } = msg.style {
            assert_eq!(msg.message, "Pushed test_branch to test_remote");
            assert_eq!(label, "View Merge Request");
            assert_eq!(url, "https://example.com/test/test/-/merge_requests/99999");
        } else {
            panic!("Expected PushPrLink variant")
        }
    }

    #[test]
    fn test_push_new_branch_no_link() {
        let action = RemoteAction::Push(
            SharedString::new_static("test_branch"),
            Remote {
                name: SharedString::new_static("test_remote"),
            },
        );

        let output = RemoteCommandOutput {
            stdout: String::new(),
            stderr: indoc! { "
                To http://example.com/test/test.git
                 * [new branch]      test -> test
                ",
            }
            .to_string(),
        };

        let msg = format_output(&action, output);

        if let SuccessStyle::ToastWithLog { output } = &msg.style {
            assert_eq!(
                output.stderr,
                "To http://example.com/test/test.git\n * [new branch]      test -> test\n"
            );
            assert_eq!(extract_pull_request_link(output), None);
        } else {
            panic!("Expected ToastWithLog variant");
        }
    }
}
