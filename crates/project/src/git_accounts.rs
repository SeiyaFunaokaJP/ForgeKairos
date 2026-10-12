//! ForgeKairos (ADR-F93): git accounts, and which one each repository commits and pushes as.
//!
//! The accounts and the per-repository choice are in the user's settings (`git.accounts`,
//! `git.project_accounts`), never in a project's own settings, which other people can write. An
//! account's token is in the operating system's credential store, under its host and login, so
//! settings that point an account at another host cannot get that account's token sent there.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use askpass::{AskPassDelegate, EncryptedPassword};
use collections::HashMap;
use fs::Fs;
use futures::channel::oneshot;
pub use git::account::GitHttpsLogin;
use git::account::{GitHttpsPrompt, GitIdentity};
use gpui::{App, AppContext as _, AsyncApp, Task};
use settings::GitAccountContent;

use crate::git_store::{LocalRepositoryState, RepositoryState};
use crate::project_settings::GitSettings;

/// The key a repository's choice is kept under in `git.project_accounts`.
pub fn project_account_key(work_directory_abs_path: &Path) -> String {
    work_directory_abs_path.to_string_lossy().into_owned()
}

/// What a repository has picked, as the git panel shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitAccountChoice {
    /// Nothing picked: git's own configuration decides.
    GitConfig,
    Account(GitAccountContent),
    /// The picked account is no longer in the settings, or is not valid; the repository's
    /// commands fall back to git's configuration until another one is picked.
    Unavailable {
        id: String,
        reason: String,
    },
}

pub fn account_choice(settings: &GitSettings, work_directory_abs_path: &Path) -> GitAccountChoice {
    account_choice_in(
        &settings.accounts,
        &settings.project_accounts,
        work_directory_abs_path,
    )
}

fn account_choice_in(
    accounts: &[GitAccountContent],
    project_accounts: &BTreeMap<String, String>,
    work_directory_abs_path: &Path,
) -> GitAccountChoice {
    let Some(id) = project_accounts.get(&project_account_key(work_directory_abs_path)) else {
        return GitAccountChoice::GitConfig;
    };
    let Some(account) = accounts.iter().find(|account| &account.id == id) else {
        return GitAccountChoice::Unavailable {
            id: id.clone(),
            reason: i18n::t!("There is no git account named \"{id}\" any more"),
        };
    };
    match identity_for_account(account) {
        Ok(_) => GitAccountChoice::Account(account.clone()),
        Err(error) => GitAccountChoice::Unavailable {
            id: id.clone(),
            reason: format!("{error:#}"),
        },
    }
}

/// The identity a repository's git commands run as, if one is picked and valid.
pub fn resolve_identity(
    settings: &GitSettings,
    work_directory_abs_path: &Path,
) -> Option<Arc<GitIdentity>> {
    identity_from_choice(
        account_choice(settings, work_directory_abs_path),
        work_directory_abs_path,
    )
}

fn identity_from_choice(
    choice: GitAccountChoice,
    work_directory_abs_path: &Path,
) -> Option<Arc<GitIdentity>> {
    match choice {
        GitAccountChoice::Account(account) => identity_for_account(&account).ok().map(Arc::new),
        GitAccountChoice::GitConfig => None,
        GitAccountChoice::Unavailable { id, reason } => {
            log::warn!(
                "the git account {id:?} picked for {work_directory_abs_path:?} is not used: {reason}"
            );
            None
        }
    }
}

pub fn https_login_for_account(account: &GitAccountContent) -> Result<Option<GitHttpsLogin>> {
    let host = account.host.as_deref().map(str::trim).unwrap_or_default();
    let login = account.login.as_deref().map(str::trim).unwrap_or_default();
    if host.is_empty() && login.is_empty() {
        return Ok(None);
    }
    GitHttpsLogin::new(host, login).map(Some)
}

pub fn identity_for_account(account: &GitAccountContent) -> Result<GitIdentity> {
    GitIdentity::new(
        &account.name,
        &account.email,
        account.signing_key.as_deref(),
        https_login_for_account(account)?,
    )
}

/// Whether `id` can name an account: it is written into settings and shown in pickers.
pub fn validate_account_id(id: &str) -> Result<()> {
    anyhow::ensure!(
        !id.is_empty()
            && id.chars().all(
                |character| character.is_alphanumeric() || matches!(character, '-' | '_' | '.')
            ),
        i18n::t!("An account name may use only letters, digits, '-', '_' and '.'")
    );
    Ok(())
}

/// Puts `account` in the user's git settings, in place of the account named `original_id` when
/// it is edited, and checks it first. A renamed account stays picked where it was.
pub fn upsert_account(
    git: &mut settings::GitSettings,
    original_id: Option<&str>,
    account: GitAccountContent,
) -> Result<()> {
    validate_account_id(&account.id)?;
    identity_for_account(&account)?;
    let accounts = git.accounts.get_or_insert_default();
    anyhow::ensure!(
        !accounts
            .iter()
            .any(|existing| existing.id == account.id && Some(existing.id.as_str()) != original_id),
        i18n::t!(
            "A git account named \"{id}\" already exists",
            id = account.id
        )
    );
    let position = original_id.and_then(|original_id| {
        accounts
            .iter()
            .position(|existing| existing.id == original_id)
    });
    if let Some(original_id) = original_id
        && original_id != account.id
        && let Some(project_accounts) = git.project_accounts.as_mut()
    {
        for picked in project_accounts.values_mut() {
            if picked == original_id {
                *picked = account.id.clone();
            }
        }
    }
    match position.and_then(|position| accounts.get_mut(position)) {
        Some(slot) => *slot = account,
        None => accounts.push(account),
    }
    Ok(())
}

/// Removes the account named `id`, and the picks of it: those repositories go back to git's
/// configuration rather than pointing at an account that is gone.
pub fn remove_account(git: &mut settings::GitSettings, id: &str) {
    if let Some(accounts) = git.accounts.as_mut() {
        accounts.retain(|account| account.id != id);
    }
    if let Some(project_accounts) = git.project_accounts.as_mut() {
        project_accounts.retain(|_, picked| picked != id);
    }
}

/// Whether any of `accounts` signs in as `login`, so that its token is still needed.
pub fn login_is_used(accounts: &[GitAccountContent], login: &GitHttpsLogin) -> bool {
    accounts.iter().any(|account| {
        https_login_for_account(account)
            .ok()
            .flatten()
            .is_some_and(|account_login| &account_login == login)
    })
}

/// The credential store entry an HTTPS login's token is kept under.
pub fn token_credentials_url(login: &GitHttpsLogin) -> String {
    format!(
        "{}://git-accounts/{}/{}",
        branding::CURRENT.url_scheme,
        login.host,
        login.login
    )
}

/// Picks `account_id` (or git's configuration, with `None`) for the repository at
/// `work_directory_abs_path`, in the user's settings file.
pub fn set_project_account(
    work_directory_abs_path: &Path,
    account_id: Option<String>,
    fs: Arc<dyn Fs>,
    cx: &App,
) {
    let key = project_account_key(work_directory_abs_path);
    settings::update_settings_file(fs, cx, move |settings, _| {
        let git = settings.git.get_or_insert_default();
        let project_accounts = git.project_accounts.get_or_insert_default();
        match account_id {
            Some(account_id) => {
                project_accounts.insert(key, account_id);
            }
            None => {
                project_accounts.remove(&key);
            }
        }
    });
}

pub fn save_token(login: &GitHttpsLogin, token: &str, cx: &App) -> Result<Task<Result<()>>> {
    let token = token.trim();
    anyhow::ensure!(!token.is_empty(), i18n::t!("The token is empty"));
    anyhow::ensure!(
        !token.chars().any(char::is_control),
        i18n::t!("The token contains a line break or a control character")
    );
    let url = token_credentials_url(login);
    let username = login.login.clone();
    let token = token.to_string();
    let credentials_provider = zed_credentials_provider::global(cx);
    Ok(cx.spawn(async move |cx| {
        credentials_provider
            .write_credentials(&url, &username, token.as_bytes(), cx)
            .await
            .context(i18n::t!("Couldn't write the token to the credential store"))
    }))
}

pub fn remove_token(login: &GitHttpsLogin, cx: &App) -> Task<Result<()>> {
    let url = token_credentials_url(login);
    let credentials_provider = zed_credentials_provider::global(cx);
    cx.spawn(async move |cx| {
        // Deleting an entry that isn't there is an error on Windows and macOS.
        if credentials_provider
            .read_credentials(&url, cx)
            .await?
            .is_some()
        {
            credentials_provider
                .delete_credentials(&url, cx)
                .await
                .context(i18n::t!(
                    "Couldn't remove the token from the credential store"
                ))?;
        }
        Ok(())
    })
}

/// Whether a token is stored for `login`. Only that is answered, never the token.
pub fn token_is_stored(login: &GitHttpsLogin, cx: &App) -> Task<Result<bool>> {
    let url = token_credentials_url(login);
    let credentials_provider = zed_credentials_provider::global(cx);
    cx.spawn(async move |cx| {
        let stored = credentials_provider.read_credentials(&url, cx).await?;
        Ok(stored.is_some_and(|(_, token)| !token.is_empty()))
    })
}

/// `askpass`, except that git's prompts for the identity's HTTPS login are answered from the
/// credential store, without asking the user. Every other prompt, and this one when no token is
/// stored, goes to `askpass`.
///
/// An untrusted repository always gets `askpass`: its own configuration (`http.sslVerify`,
/// `http.proxy`, `http.curloptResolve`, ...) can route the right host through someone else, and
/// a `-c` option cannot outrank a more specific `http.<url>.*` of the repository.
pub fn askpass_with_account_token(
    identity: Option<Arc<GitIdentity>>,
    repository_is_trusted: bool,
    askpass: AskPassDelegate,
    cx: &mut AsyncApp,
) -> AskPassDelegate {
    if !repository_is_trusted {
        return askpass;
    }
    let Some(login) = identity.and_then(|identity| identity.https_login.clone()) else {
        return askpass;
    };
    let fallback = Arc::new(askpass);
    let credentials_provider = cx.update(|cx| zed_credentials_provider::global(cx));
    AskPassDelegate::new(cx, move |prompt, response_sender, cx| {
        match login.match_prompt(&prompt) {
            Some(GitHttpsPrompt::Username) => match EncryptedPassword::try_from(&*login.login) {
                Ok(answer) => {
                    response_sender.send(answer).ok();
                }
                Err(error) => {
                    log::error!("answering git's user name prompt: {error:#}");
                    forward_prompt(&fallback, prompt, response_sender, cx);
                }
            },
            Some(GitHttpsPrompt::Password) => {
                let url = token_credentials_url(&login);
                let credentials_provider = credentials_provider.clone();
                let fallback = fallback.clone();
                cx.spawn(async move |cx| {
                    let answer = match credentials_provider.read_credentials(&url, cx).await {
                        Ok(stored) => stored.and_then(|(_, token)| token_answer(token)),
                        Err(error) => {
                            log::error!(
                                "reading a git account's token from the credential store: {error:#}"
                            );
                            None
                        }
                    };
                    match answer {
                        Some(answer) => {
                            response_sender.send(answer).ok();
                        }
                        None => forward_prompt(&fallback, prompt, response_sender, cx),
                    }
                })
                .detach();
            }
            None => forward_prompt(&fallback, prompt, response_sender, cx),
        }
    })
}

/// The environment a repository's git job runs with: the project's, with the picked account's
/// author and committer over any the project's shell exports (`GitBinary` sets them too, but the
/// commands apply the project's environment after it).
pub fn environment_with_identity(
    environment: &Arc<HashMap<String, String>>,
    identity: Option<&GitIdentity>,
) -> Arc<HashMap<String, String>> {
    let Some(identity) = identity else {
        return environment.clone();
    };
    let mut environment = (**environment).clone();
    for (key, value) in identity.environment() {
        environment.insert(key.to_string(), value.to_string());
    }
    Arc::new(environment)
}

pub(crate) fn with_account_environment(state: &RepositoryState) -> RepositoryState {
    match state {
        RepositoryState::Local(local) => RepositoryState::Local(LocalRepositoryState {
            fs: local.fs.clone(),
            backend: local.backend.clone(),
            environment: environment_with_identity(
                &local.environment,
                local.backend.identity().as_deref(),
            ),
        }),
        RepositoryState::Remote(remote) => RepositoryState::Remote(remote.clone()),
    }
}

/// Never logs what it was given: the errors of these conversions can carry the bytes.
fn token_answer(token: Vec<u8>) -> Option<EncryptedPassword> {
    let token = match String::from_utf8(token) {
        Ok(token) => token,
        Err(_) => {
            log::error!("the git account's stored token is not UTF-8");
            return None;
        }
    };
    if token.is_empty() {
        return None;
    }
    match EncryptedPassword::try_from(token.as_str()) {
        Ok(answer) => Some(answer),
        Err(_) => {
            log::error!("the git account's token could not be held in memory");
            None
        }
    }
}

fn forward_prompt(
    fallback: &Arc<AskPassDelegate>,
    prompt: String,
    response_sender: oneshot::Sender<EncryptedPassword>,
    cx: &mut AsyncApp,
) {
    let answer = fallback.ask_password(prompt);
    cx.background_spawn(async move {
        if let Some(answer) = answer.await {
            response_sender.send(answer).ok();
        }
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;
    use askpass::IKnowWhatIAmDoingAndIHaveReadTheDocs;
    use gpui::TestAppContext;
    use std::path::PathBuf;
    use zed_credentials_provider::test_support::InMemoryCredentialsProvider;

    fn account(id: &str) -> GitAccountContent {
        GitAccountContent {
            id: id.to_string(),
            name: "Ada Work".to_string(),
            email: "ada@work.example".to_string(),
            host: Some("github.com".to_string()),
            login: Some("ada-work".to_string()),
            signing_key: None,
        }
    }

    struct Settings {
        accounts: Vec<GitAccountContent>,
        project_accounts: BTreeMap<String, String>,
    }

    fn settings(accounts: Vec<GitAccountContent>, picks: &[(&Path, &str)]) -> Settings {
        let mut project_accounts = BTreeMap::default();
        for (path, id) in picks {
            project_accounts.insert(project_account_key(path), id.to_string());
        }
        Settings {
            accounts,
            project_accounts,
        }
    }

    fn account_choice(settings: &Settings, path: &Path) -> GitAccountChoice {
        account_choice_in(&settings.accounts, &settings.project_accounts, path)
    }

    fn resolve_identity(settings: &Settings, path: &Path) -> Option<Arc<GitIdentity>> {
        identity_from_choice(account_choice(settings, path), path)
    }

    #[test]
    fn test_choice_is_per_repository_and_falls_back_to_git_config() {
        let picked = PathBuf::from("/work/repo");
        let other = PathBuf::from("/home/repo");
        let settings = settings(vec![account("work")], &[(&picked, "work")]);

        assert_eq!(
            account_choice(&settings, &picked),
            GitAccountChoice::Account(account("work"))
        );
        assert_eq!(
            account_choice(&settings, &other),
            GitAccountChoice::GitConfig
        );
        let identity = resolve_identity(&settings, &picked).unwrap();
        assert_eq!(identity.name, "Ada Work");
        assert_eq!(
            identity.https_login,
            Some(GitHttpsLogin::new("github.com", "ada-work").unwrap())
        );
        assert_eq!(resolve_identity(&settings, &other), None);
    }

    #[test]
    fn test_missing_or_invalid_account_is_unavailable() {
        let repo = PathBuf::from("/work/repo");
        let missing = settings(vec![account("work")], &[(&repo, "personal")]);
        assert!(matches!(
            account_choice(&missing, &repo),
            GitAccountChoice::Unavailable { id, .. } if id == "personal"
        ));
        assert_eq!(resolve_identity(&missing, &repo), None);

        let mut broken = account("work");
        broken.host = Some("github.com/evil".to_string());
        let invalid = settings(vec![broken], &[(&repo, "work")]);
        assert!(matches!(
            account_choice(&invalid, &repo),
            GitAccountChoice::Unavailable { .. }
        ));
        assert_eq!(resolve_identity(&invalid, &repo), None);

        let mut without_login = account("work");
        without_login.host = None;
        without_login.login = None;
        let author_only = settings(vec![without_login], &[(&repo, "work")]);
        assert_eq!(
            resolve_identity(&author_only, &repo).unwrap().https_login,
            None
        );
    }

    #[test]
    fn test_token_is_keyed_by_host_and_login() {
        let login = GitHttpsLogin::new("github.com", "ada-work").unwrap();
        let other_host = GitHttpsLogin::new("evil.example", "ada-work").unwrap();
        assert_ne!(
            token_credentials_url(&login),
            token_credentials_url(&other_host)
        );
        assert!(token_credentials_url(&login).ends_with("://git-accounts/github.com/ada-work"));
    }

    #[test]
    fn test_account_ids() {
        assert!(validate_account_id("work").is_ok());
        assert!(validate_account_id("仕事-2").is_ok());
        assert!(validate_account_id("").is_err());
        assert!(validate_account_id("a b").is_err());
        assert!(validate_account_id("a/b").is_err());
    }

    #[test]
    fn test_upsert_and_remove_keep_the_picks_consistent() {
        let mut git = settings::GitSettings::default();
        upsert_account(&mut git, None, account("work")).unwrap();
        upsert_account(&mut git, None, account("personal")).unwrap();
        assert!(
            upsert_account(&mut git, None, account("work")).is_err(),
            "a second account cannot take a name in use"
        );
        assert!(upsert_account(&mut git, Some("personal"), account("work")).is_err());
        let mut invalid = account("broken");
        invalid.email = String::new();
        assert!(upsert_account(&mut git, None, invalid).is_err());
        let mut bad_name = account("a b");
        bad_name.id = "a b".to_string();
        assert!(upsert_account(&mut git, None, bad_name).is_err());
        assert_eq!(git.accounts.as_ref().unwrap().len(), 2);

        git.project_accounts = Some(BTreeMap::from_iter([
            ("/a".to_string(), "work".to_string()),
            ("/b".to_string(), "personal".to_string()),
        ]));
        let mut renamed = account("office");
        renamed.name = "Ada Office".to_string();
        upsert_account(&mut git, Some("work"), renamed).unwrap();
        let ids: Vec<_> = git
            .accounts
            .iter()
            .flatten()
            .map(|account| account.id.as_str())
            .collect();
        assert_eq!(ids, ["office", "personal"]);
        assert_eq!(git.project_accounts.as_ref().unwrap()["/a"], "office");
        assert_eq!(git.project_accounts.as_ref().unwrap()["/b"], "personal");

        remove_account(&mut git, "office");
        assert_eq!(git.accounts.as_ref().unwrap().len(), 1);
        assert_eq!(
            git.project_accounts.as_ref().unwrap(),
            &BTreeMap::from_iter([("/b".to_string(), "personal".to_string())])
        );
    }

    #[test]
    fn test_account_environment_wins_over_the_projects() {
        let environment = Arc::new(HashMap::from_iter([
            ("PATH".to_string(), "/bin".to_string()),
            ("GIT_AUTHOR_NAME".to_string(), "Shell User".to_string()),
            (
                "GIT_COMMITTER_EMAIL".to_string(),
                "shell@example.com".to_string(),
            ),
        ]));
        assert!(Arc::ptr_eq(
            &environment_with_identity(&environment, None),
            &environment
        ));

        let identity = identity_for_account(&account("work")).unwrap();
        let overlaid = environment_with_identity(&environment, Some(&identity));
        assert_eq!(overlaid["PATH"], "/bin");
        assert_eq!(overlaid["GIT_AUTHOR_NAME"], "Ada Work");
        assert_eq!(overlaid["GIT_AUTHOR_EMAIL"], "ada@work.example");
        assert_eq!(overlaid["GIT_COMMITTER_NAME"], "Ada Work");
        assert_eq!(overlaid["GIT_COMMITTER_EMAIL"], "ada@work.example");
    }

    #[test]
    fn test_login_is_used() {
        let accounts = vec![account("work")];
        assert!(login_is_used(
            &accounts,
            &GitHttpsLogin::new("github.com", "ada-work").unwrap()
        ));
        assert!(!login_is_used(
            &accounts,
            &GitHttpsLogin::new("gitlab.com", "ada-work").unwrap()
        ));
    }

    async fn ask(delegate: &AskPassDelegate, prompt: &str) -> Option<String> {
        delegate
            .ask_password(prompt.to_string())
            .await
            .map(|answer| {
                answer
                    .decrypt(IKnowWhatIAmDoingAndIHaveReadTheDocs)
                    .unwrap()
            })
    }

    #[gpui::test]
    async fn test_askpass_answers_only_the_accounts_host_with_its_token(cx: &mut TestAppContext) {
        let credentials = cx.update(InMemoryCredentialsProvider::install);
        let login = GitHttpsLogin::new("github.com", "ada-work").unwrap();
        credentials.insert(&token_credentials_url(&login), "ada-work", b"secret-token");
        let identity = Arc::new(
            GitIdentity::new("Ada", "ada@work.example", None, Some(login.clone())).unwrap(),
        );

        let forwarded = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let fallback = AskPassDelegate::new(&mut cx.to_async(), {
            let forwarded = forwarded.clone();
            move |prompt, response_sender, _| {
                forwarded.lock().push(prompt);
                response_sender
                    .send(EncryptedPassword::try_from("typed").unwrap())
                    .ok();
            }
        });
        let delegate =
            askpass_with_account_token(Some(identity.clone()), true, fallback, &mut cx.to_async());

        assert_eq!(
            ask(&delegate, "Password for 'https://ada-work@github.com': ").await,
            Some("secret-token".to_string())
        );
        assert!(forwarded.lock().is_empty());

        for prompt in [
            "Password for 'https://ada-work@evil.example': ",
            "Password for 'http://ada-work@github.com': ",
            "Enter passphrase for key '/home/ada/.ssh/id_ed25519': ",
        ] {
            assert_eq!(
                ask(&delegate, prompt).await,
                Some("typed".to_string()),
                "{prompt}"
            );
        }
        assert_eq!(forwarded.lock().len(), 3);

        // Without a readable token the user is asked, as before.
        credentials.set_fail_reads(true);
        assert_eq!(
            ask(&delegate, "Password for 'https://ada-work@github.com': ").await,
            Some("typed".to_string())
        );
        assert_eq!(forwarded.lock().len(), 4);
        credentials.set_fail_reads(false);

        // An untrusted repository's own configuration could send the request elsewhere.
        let untrusted_fallback = AskPassDelegate::new(&mut cx.to_async(), {
            let forwarded = forwarded.clone();
            move |prompt, response_sender, _| {
                forwarded.lock().push(prompt);
                response_sender
                    .send(EncryptedPassword::try_from("typed").unwrap())
                    .ok();
            }
        });
        let untrusted = askpass_with_account_token(
            Some(identity),
            false,
            untrusted_fallback,
            &mut cx.to_async(),
        );
        assert_eq!(
            ask(&untrusted, "Password for 'https://ada-work@github.com': ").await,
            Some("typed".to_string())
        );
        assert_eq!(forwarded.lock().len(), 5);
    }
}
