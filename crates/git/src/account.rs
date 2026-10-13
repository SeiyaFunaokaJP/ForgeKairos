//! ForgeKairos (ADR-F93): the identity a project's git commands run as, picked from the user's
//! git accounts. The account list and the per-project choice live in the user's settings; this
//! module only knows how an identity reaches a git process.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::Result;

/// Who the git commands of one repository commit as, and, optionally, which HTTPS login its
/// remote commands authenticate as.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitIdentity {
    pub name: String,
    pub email: String,
    pub signing_key: Option<String>,
    pub https_login: Option<GitHttpsLogin>,
    /// The private key SSH remotes are reached with. Local repositories only: it is a path on
    /// this machine, and never sent to a remote server.
    pub ssh_key: Option<PathBuf>,
}

/// The HTTPS host an account's token belongs to, and the login it goes with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitHttpsLogin {
    pub host: String,
    pub login: String,
}

/// What an askpass prompt git showed is asking for, when it is this login's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GitHttpsPrompt {
    Username,
    Password,
}

impl GitIdentity {
    /// Checked when an account is resolved, so that nothing malformed reaches git's command line.
    pub fn new(
        name: &str,
        email: &str,
        signing_key: Option<&str>,
        https_login: Option<GitHttpsLogin>,
    ) -> Result<Self> {
        let name = name.trim();
        let email = email.trim();
        anyhow::ensure!(!name.is_empty(), i18n::t!("The account has no user name"));
        anyhow::ensure!(!email.is_empty(), i18n::t!("The account has no email"));
        // Git refuses `<`, `>` and line breaks in an identity, and would quietly strip others.
        for value in [name, email] {
            anyhow::ensure!(
                !value.contains(['<', '>']) && !value.chars().any(char::is_control),
                i18n::t!(
                    "The account's name and email cannot contain '<', '>' or control characters"
                )
            );
        }
        let signing_key = signing_key
            .map(str::trim)
            .filter(|signing_key| !signing_key.is_empty());
        if let Some(signing_key) = signing_key {
            anyhow::ensure!(
                !signing_key.chars().any(char::is_control),
                i18n::t!("The account's signing key contains a control character")
            );
        }
        Ok(Self {
            name: name.to_string(),
            email: email.to_string(),
            signing_key: signing_key.map(str::to_string),
            https_login,
            ssh_key: None,
        })
    }

    /// This identity, reaching SSH remotes with the private key at `ssh_key`, which must be an
    /// absolute path.
    pub fn with_ssh_key(mut self, ssh_key: Option<&Path>) -> Result<Self> {
        let Some(ssh_key) = ssh_key.filter(|ssh_key| !ssh_key.as_os_str().is_empty()) else {
            self.ssh_key = None;
            return Ok(self);
        };
        // The path is put in a command that a shell runs (`GIT_SSH_COMMAND`); quoting keeps
        // anything else in it literal, but a line break would end the command.
        anyhow::ensure!(
            !ssh_key.to_string_lossy().chars().any(char::is_control),
            i18n::t!("The account's SSH key path contains a control character")
        );
        anyhow::ensure!(
            ssh_key.is_absolute(),
            i18n::t!("The account's SSH key must be given as an absolute path")
        );
        self.ssh_key = Some(ssh_key.to_path_buf());
        Ok(self)
    }

    /// The `GIT_SSH_COMMAND` that makes ssh offer only this identity's key, if it has one.
    ///
    /// git runs the command through a shell (Git for Windows through its own `sh`), so the path
    /// is quoted for a POSIX shell: inside single quotes, spaces, backslashes and `$` stay as
    /// they are.
    pub fn ssh_command(&self) -> Option<String> {
        let ssh_key = self.ssh_key.as_ref()?;
        Some(format!(
            "ssh -i {} -o IdentitiesOnly=yes",
            shell_single_quote(&ssh_key.to_string_lossy())
        ))
    }

    /// The environment that makes git commit as this identity. The variables win over every
    /// `user.name` / `user.email` in git's configuration, the repository's own included.
    pub fn environment(&self) -> [(&'static str, &str); 4] {
        [
            ("GIT_AUTHOR_NAME", &self.name),
            ("GIT_AUTHOR_EMAIL", &self.email),
            ("GIT_COMMITTER_NAME", &self.name),
            ("GIT_COMMITTER_EMAIL", &self.email),
        ]
    }

    /// Options that go before the subcommand. None of them holds a secret: the token is handed
    /// over through askpass, never on the command line or in the environment, where other
    /// processes of the same user can read it.
    pub fn config_args(&self) -> Vec<OsString> {
        let mut args = Vec::new();
        if let Some(signing_key) = &self.signing_key {
            args.push("-c".into());
            args.push(format!("user.signingkey={signing_key}").into());
        }
        if let Some(login) = &self.https_login {
            let url = login.url();
            // An empty helper empties the list git has collected so far for this URL, so a
            // credential helper (such as Git Credential Manager) cannot answer with another
            // account's login; git then asks through askpass, which answers with the token.
            args.push("-c".into());
            args.push(format!("credential.{url}.helper=").into());
            args.push("-c".into());
            args.push(format!("credential.{url}.username={}", login.login).into());
        }
        args
    }
}

impl GitHttpsLogin {
    pub fn new(host: &str, login: &str) -> Result<Self> {
        let host = host.trim().trim_end_matches('/');
        let host = host.strip_prefix("https://").unwrap_or(host);
        let login = login.trim();
        anyhow::ensure!(!host.is_empty(), i18n::t!("The account has no host"));
        // The host becomes part of a `-c` option's key, where `=` ends the key and `.` separates
        // its parts, so only what a host name and port are made of is let through.
        anyhow::ensure!(
            host.chars()
                .all(|character| character.is_ascii_alphanumeric()
                    || matches!(character, '-' | '.' | ':')),
            i18n::t!(
                "The account's host must be a host name such as github.com, optionally with a port"
            )
        );
        anyhow::ensure!(!login.is_empty(), i18n::t!("The account has no login"));
        anyhow::ensure!(
            !login.chars().any(|character| character.is_whitespace()
                || character.is_control()
                || matches!(character, '/' | ':' | '@')),
            i18n::t!("The account's login cannot contain spaces, '/', ':' or '@'")
        );
        Ok(Self {
            host: host.to_ascii_lowercase(),
            login: login.to_string(),
        })
    }

    fn url(&self) -> String {
        format!("https://{}", self.host)
    }

    /// Whether `prompt`, as git words it for askpass, asks for this login's user name or
    /// password. Only HTTPS to exactly this host qualifies: the token must not go to another host
    /// (a submodule's, a redirect's) nor over plain HTTP.
    pub fn match_prompt(&self, prompt: &str) -> Option<GitHttpsPrompt> {
        let prompt = prompt.trim_end();
        let (kind, rest) = if let Some(rest) = prompt.strip_prefix("Username for '") {
            (GitHttpsPrompt::Username, rest)
        } else if let Some(rest) = prompt.strip_prefix("Password for '") {
            (GitHttpsPrompt::Password, rest)
        } else {
            return None;
        };
        let described = rest.strip_suffix("':")?;
        let after_scheme = described.strip_prefix("https://")?;
        // With `credential.useHttpPath` git describes the URL with its path.
        let authority = after_scheme
            .split_once('/')
            .map_or(after_scheme, |(authority, _)| authority);
        let (login, host) = match authority.rsplit_once('@') {
            Some((login, host)) => (Some(login), host),
            None => (None, authority),
        };
        if !host.eq_ignore_ascii_case(&self.host) {
            return None;
        }
        match (kind, login) {
            (GitHttpsPrompt::Username, None) => Some(kind),
            (GitHttpsPrompt::Password, Some(login)) if login == self.login => Some(kind),
            _ => None,
        }
    }
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> GitIdentity {
        GitIdentity::new(
            "Ada",
            "ada@example.com",
            Some("ABCD1234"),
            Some(GitHttpsLogin::new("github.com", "ada-work").unwrap()),
        )
        .unwrap()
    }

    #[test]
    fn test_environment_sets_author_and_committer() {
        let identity = identity();
        assert_eq!(
            identity.environment(),
            [
                ("GIT_AUTHOR_NAME", "Ada"),
                ("GIT_AUTHOR_EMAIL", "ada@example.com"),
                ("GIT_COMMITTER_NAME", "Ada"),
                ("GIT_COMMITTER_EMAIL", "ada@example.com"),
            ]
        );
    }

    #[test]
    fn test_config_args_reset_the_helper_for_the_host_only() {
        let identity = identity();
        assert_eq!(
            identity.config_args(),
            [
                "-c",
                "user.signingkey=ABCD1234",
                "-c",
                "credential.https://github.com.helper=",
                "-c",
                "credential.https://github.com.username=ada-work",
            ]
            .map(OsString::from)
        );

        let without_login = GitIdentity::new("Ada", "ada@example.com", None, None).unwrap();
        assert!(without_login.config_args().is_empty());
    }

    #[test]
    fn test_invalid_values_are_refused() {
        assert!(GitIdentity::new("", "ada@example.com", None, None).is_err());
        assert!(GitIdentity::new("Ada", " ", None, None).is_err());
        assert!(GitIdentity::new("Ada <x>", "ada@example.com", None, None).is_err());
        assert!(GitIdentity::new("Ada", "ada@example.com\n", None, None).is_ok());
        assert!(GitIdentity::new("Ada\nEve", "ada@example.com", None, None).is_err());
        assert!(GitIdentity::new("Ada", "ada@example.com", Some("a\nb"), None).is_err());

        assert!(GitHttpsLogin::new("github.com=x", "ada").is_err());
        assert!(GitHttpsLogin::new("github.com/owner", "ada").is_err());
        assert!(GitHttpsLogin::new("evil.com.helper", "ada").is_ok());
        assert!(GitHttpsLogin::new("github.com", "").is_err());
        assert!(GitHttpsLogin::new("github.com", "a b").is_err());
        assert!(GitHttpsLogin::new("github.com", "ada@example.com").is_err());
        assert_eq!(
            GitHttpsLogin::new(" https://GitHub.com/ ", "ada").unwrap(),
            GitHttpsLogin {
                host: "github.com".into(),
                login: "ada".into()
            }
        );
    }

    #[test]
    fn test_ssh_command_offers_only_the_accounts_key() {
        let without_key = identity();
        assert_eq!(without_key.ssh_command(), None);

        let absolute = if cfg!(windows) {
            r"C:\Users\Ada Work\.ssh\id_ed25519"
        } else {
            "/home/Ada Work/.ssh/id_ed25519"
        };
        let with_key = identity()
            .with_ssh_key(Some(Path::new(absolute)))
            .unwrap();
        assert_eq!(
            with_key.ssh_command().unwrap(),
            format!("ssh -i '{absolute}' -o IdentitiesOnly=yes")
        );
        assert_eq!(
            identity().with_ssh_key(None).unwrap().ssh_command(),
            None
        );
        assert_eq!(
            identity()
                .with_ssh_key(Some(Path::new("")))
                .unwrap()
                .ssh_command(),
            None
        );

        assert!(
            identity()
                .with_ssh_key(Some(Path::new("id_ed25519")))
                .is_err(),
            "a relative path depends on where git runs"
        );
        let with_line_break = format!("{absolute}\ntouch pwned");
        assert!(
            identity()
                .with_ssh_key(Some(Path::new(&with_line_break)))
                .is_err()
        );
    }

    #[test]
    fn test_shell_single_quote_keeps_everything_literal() {
        assert_eq!(shell_single_quote("a b"), "'a b'");
        assert_eq!(shell_single_quote(r"C:\a\b"), r"'C:\a\b'");
        assert_eq!(shell_single_quote("$(x)`y`"), "'$(x)`y`'");
        assert_eq!(shell_single_quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn test_prompts_are_matched_by_scheme_host_and_login() {
        let login = GitHttpsLogin::new("github.com", "ada-work").unwrap();
        let matches = |prompt: &str| login.match_prompt(prompt);

        assert_eq!(
            matches("Password for 'https://ada-work@github.com': "),
            Some(GitHttpsPrompt::Password)
        );
        assert_eq!(
            matches("Password for 'https://ada-work@GitHub.com/owner/repo.git': "),
            Some(GitHttpsPrompt::Password)
        );
        assert_eq!(
            matches("Username for 'https://github.com': "),
            Some(GitHttpsPrompt::Username)
        );

        assert_eq!(matches("Password for 'http://ada-work@github.com': "), None);
        assert_eq!(matches("Password for 'https://other@github.com': "), None);
        assert_eq!(
            matches("Password for 'https://ada-work@gitlab.com': "),
            None
        );
        assert_eq!(
            matches("Password for 'https://ada-work@github.com.evil.com': "),
            None
        );
        assert_eq!(
            matches("Password for 'https://github.com@evil.com': "),
            None
        );
        assert_eq!(matches("Password for 'https://github.com': "), None);
        assert_eq!(matches("Username for 'https://x@github.com': "), None);
        assert_eq!(
            matches("Enter passphrase for key '/home/ada/.ssh/id': "),
            None
        );
        assert_eq!(matches("ada-work@github.com's password: "), None);
    }
}
