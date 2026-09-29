//! Passwords remembered for quick-connect targets (ADR-F28).
//!
//! The password itself never touches `terminal_sessions.json`: it lives in the
//! OS credential store behind [`CredentialsProvider`] (Windows Credential
//! Manager in release builds, a file under the Zed support directory in the dev
//! channel), and only the "remember it" flag is written to the sessions file.
//!
//! Two very different mechanisms consume the stored password:
//!
//! * SSH — [`stored_password_proxy`] stands up an askpass socket and points
//!   `SSH_ASKPASS` at it, the same trick the fork's remote connections use
//!   (`remote::transport::ssh`). The password is handed to `ssh` out of band,
//!   so it never travels through the PTY and cannot be echoed.
//! * Telnet — there is no askpass, so [`answer_telnet_login`] watches the grid
//!   for a `login:` / `Password:` prompt and types the answer once. This does
//!   go through the PTY; see [`Terminal::write_secret`] for what that costs.

use std::{
    ops::ControlFlow,
    sync::{
        Arc, LazyLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context as _, Result, anyhow};
use askpass::{EncryptedPassword, IKnowWhatIAmDoingAndIHaveReadTheDocs, PasswordProxy};
use credentials_provider::CredentialsProvider;
use futures::{StreamExt as _, channel::mpsc};
use gpui::{AsyncApp, BackgroundExecutor, Entity, Task, WeakEntity};
use regex::Regex;
use util::ResultExt as _;
use zeroize::Zeroize as _;

use crate::{
    Event, Terminal,
    terminal_macro::{ConnectionProtocol, SavedConnection},
};

pub use askpass::EncryptedPassword as StoredPassword;

/// Scheme prefix of the credential keys this module owns. Distinct from the
/// `ssh://` keys Zed's remote projects use, so a quick-connect password can
/// never be mistaken for (or overwrite) a remote development credential.
const CREDENTIAL_SCHEME_PREFIX: &str = "zed-quick-connect+";

/// How many trailing lines the telnet watcher looks at. Only the last one is
/// matched; the rest are read so a wrapped prompt still ends up last.
const TELNET_SCAN_LINES: usize = 20;

/// How long the telnet watcher waits for a login prompt before giving up. Long
/// enough for a switch that takes its time printing a banner, short enough that
/// a forgotten watcher does not sit on a live terminal.
pub const TELNET_LOGIN_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the session log stays paused after the password is written, to
/// cover the round trip in which a device that echoes its input would send the
/// password back (ADR-F29).
pub const PASSWORD_ECHO_PAUSE: Duration = Duration::from_millis(1_500);

/// `login:` / `Username:`, anchored at the end of the line so only a prompt
/// waiting for input matches — not the word appearing inside a banner
/// (`Last login: …`). Japanese-firmware devices spell it out, hence the second
/// half of the alternation.
static LOGIN_PROMPT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(login|user\s?name|ログイン|ユーザー?名)\s*[:：]\s*$")
        .expect("login prompt regex is valid")
});

/// `Password:` / `Passcode:` / `Pass:`, anchored the same way.
static PASSWORD_PROMPT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(pass(word|code)?|パスワード)\s*[:：]\s*$")
        .expect("password prompt regex is valid")
});

/// The credential key for a saved connection. Deliberately independent of the
/// connection's *name* and folder, so renaming an entry keeps its password and
/// two entries for the same target share one.
pub fn credential_url(connection: &SavedConnection) -> String {
    credential_url_for_target(
        connection.protocol,
        &connection.host,
        connection.port_or_default(),
    )
}

/// See [`credential_url`]. Exposed for callers that only know the target, such
/// as the file transfer screen.
pub fn credential_url_for_target(protocol: ConnectionProtocol, host: &str, port: u16) -> String {
    format!(
        "{CREDENTIAL_SCHEME_PREFIX}{}://{}:{}",
        protocol.scheme(),
        host,
        port
    )
}

/// The user name a credential is filed under. `None` and `""` are the same
/// account (a telnet device that only asks for a password), so both normalize
/// to the empty string.
fn credential_username(user: Option<&str>) -> &str {
    user.map(str::trim).unwrap_or_default()
}

/// Reads and writes remembered quick-connect passwords.
#[derive(Clone)]
pub struct QuickConnectCredentials {
    provider: Arc<dyn CredentialsProvider>,
}

impl QuickConnectCredentials {
    pub fn new(provider: Arc<dyn CredentialsProvider>) -> Self {
        Self { provider }
    }

    /// The stored password for `connection`, if there is one for the same user.
    pub async fn lookup(
        &self,
        connection: &SavedConnection,
        cx: &AsyncApp,
    ) -> Option<EncryptedPassword> {
        lookup_for_target(
            self.provider.as_ref(),
            connection.protocol,
            &connection.host,
            connection.port_or_default(),
            connection.user.as_deref(),
            cx,
        )
        .await
    }

    /// The stored password for a bare target. This is the entry point for
    /// callers that hold no [`SavedConnection`] — the file transfer screen
    /// reaches the quick-connect passwords through it (ADR-F28).
    pub async fn lookup_target(
        &self,
        protocol: ConnectionProtocol,
        host: &str,
        port: u16,
        user: Option<&str>,
        cx: &AsyncApp,
    ) -> Option<EncryptedPassword> {
        lookup_for_target(self.provider.as_ref(), protocol, host, port, user, cx).await
    }

    /// Whether a password is stored, without decrypting it. Used for the
    /// 「保存済み」 badge.
    pub async fn has_password(&self, connection: &SavedConnection, cx: &AsyncApp) -> bool {
        read_raw(
            self.provider.as_ref(),
            &credential_url(connection),
            connection.user.as_deref(),
            cx,
        )
        .await
        .is_some()
    }

    pub async fn store(
        &self,
        connection: &SavedConnection,
        password: &str,
        cx: &AsyncApp,
    ) -> Result<()> {
        self.store_target(
            connection.protocol,
            &connection.host,
            connection.port_or_default(),
            connection.user.as_deref(),
            password,
            cx,
        )
        .await
    }

    /// Remembers a password for a bare target, the counterpart of
    /// [`Self::lookup_target`] for callers that hold no [`SavedConnection`].
    pub async fn store_target(
        &self,
        protocol: ConnectionProtocol,
        host: &str,
        port: u16,
        user: Option<&str>,
        password: &str,
        cx: &AsyncApp,
    ) -> Result<()> {
        let url = credential_url_for_target(protocol, host, port);
        self.provider
            .write_credentials(&url, credential_username(user), password.as_bytes(), cx)
            .await
            .with_context(|| i18n::t!("Couldn't save the password ({url})"))
    }

    /// Removes the stored password. A target that has none is not an error, so
    /// this checks before deleting: `CredDeleteW` fails on a missing entry.
    pub async fn forget(&self, connection: &SavedConnection, cx: &AsyncApp) -> Result<()> {
        self.forget_target(
            connection.protocol,
            &connection.host,
            connection.port_or_default(),
            cx,
        )
        .await
    }

    /// See [`Self::forget`]. The user name is not part of the key, so this
    /// drops whichever account is filed under the target.
    pub async fn forget_target(
        &self,
        protocol: ConnectionProtocol,
        host: &str,
        port: u16,
        cx: &AsyncApp,
    ) -> Result<()> {
        let url = credential_url_for_target(protocol, host, port);
        let stored = self
            .provider
            .read_credentials(&url, cx)
            .await
            .with_context(|| i18n::t!("Couldn't read the password ({url})"))?;
        if stored.is_none() {
            return Ok(());
        }
        self.provider
            .delete_credentials(&url, cx)
            .await
            .with_context(|| i18n::t!("Couldn't delete the password ({url})"))
    }
}

/// The stored password for a target. Most callers want
/// [`QuickConnectCredentials::lookup_target`], which holds the provider for
/// them.
pub async fn lookup_for_target(
    provider: &dyn CredentialsProvider,
    protocol: ConnectionProtocol,
    host: &str,
    port: u16,
    user: Option<&str>,
    cx: &AsyncApp,
) -> Option<EncryptedPassword> {
    let mut password = read_raw(
        provider,
        &credential_url_for_target(protocol, host, port),
        user,
        cx,
    )
    .await?;
    let encrypted = std::str::from_utf8(&password)
        .ok()
        .and_then(|password| EncryptedPassword::try_from(password).log_err());
    password.zeroize();
    encrypted
}

/// The raw blob, filtered by user name. A password stored for `ubuntu` is not
/// offered when connecting as `root`, even though both live on one host.
async fn read_raw(
    provider: &dyn CredentialsProvider,
    url: &str,
    user: Option<&str>,
    cx: &AsyncApp,
) -> Option<Vec<u8>> {
    let (stored_user, password) = provider
        .read_credentials(url, cx)
        .await
        .with_context(|| i18n::t!("Couldn't read the password ({url})"))
        .log_err()??;
    (stored_user == credential_username(user)).then_some(password)
}

// -------------------------------------------------------------------------
// SSH: answering `ssh`'s askpass prompts from the store
// -------------------------------------------------------------------------

/// What `ssh` asked for. Only [`AskPassPrompt::Password`] can be answered from
/// the store; the rest need a human, and a quick-connect terminal has no way to
/// show one because `SSH_ASKPASS_REQUIRE=force` takes the prompt away from the
/// tty.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AskPassPrompt {
    Password,
    /// A private key passphrase. Answering it with the account password would
    /// burn the one shot the account password gets.
    Passphrase,
    /// First-contact host key confirmation (`yes/no/[fingerprint]`).
    HostKey,
    Other,
}

/// Classifies an askpass prompt. Order matters: a host key prompt mentions
/// neither word, a passphrase prompt contains "passphrase" which itself
/// contains "pass".
pub fn classify_prompt(prompt: &str) -> AskPassPrompt {
    let prompt = prompt.to_ascii_lowercase();
    if prompt.contains("yes/no") || prompt.contains("fingerprint") {
        AskPassPrompt::HostKey
    } else if prompt.contains("passphrase") {
        AskPassPrompt::Passphrase
    } else if prompt.contains("password") {
        AskPassPrompt::Password
    } else {
        AskPassPrompt::Other
    }
}

/// What [`StoredPasswordPolicy`] did with one prompt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AskPassOutcome {
    /// The stored password was handed to `ssh`.
    Answered,
    /// `ssh` asked for the password again, which means the stored one was
    /// rejected. Answering again would loop, so the request is failed.
    Rejected,
    /// `ssh` asked something the store cannot answer.
    Unanswerable(AskPassPrompt),
}

impl AskPassOutcome {
    /// Japanese explanation to show the user, for the outcomes that actually
    /// stop the connection.
    ///
    /// A refused passphrase or an unrecognized prompt is *not* one of them:
    /// `ssh` reads the empty answer as a failed attempt at that one method and
    /// moves on, so a key with a passphrase followed by a working password
    /// would otherwise raise an alarm about a connection that succeeded.
    pub fn message(&self) -> Option<&'static str> {
        match self {
            AskPassOutcome::Rejected => Some(i18n::t!(
                "The saved password was rejected. Enter the password again in Quick Connect."
            )),
            // Refusing this one aborts the connection: OpenSSH reads the empty
            // answer as "no" and gives up on the unknown host key.
            AskPassOutcome::Unanswerable(AskPassPrompt::HostKey) => Some(i18n::t!(
                "A saved password can't answer the host key confirmation of a first connection. \
                 Connect once with “Remember password” turned off and confirm the host key."
            )),
            AskPassOutcome::Answered | AskPassOutcome::Unanswerable(_) => None,
        }
    }
}

/// Hands the stored password to the *first* password prompt and refuses every
/// prompt after it.
///
/// Refusing rather than repeating matters: `ssh` retries a rejected password up
/// to `NumberOfPasswordPrompts` times, and a proxy that keeps answering would
/// spin through all of them with the same wrong secret before failing.
#[derive(Default)]
pub struct StoredPasswordPolicy {
    answered: AtomicBool,
}

impl StoredPasswordPolicy {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn decide(&self, prompt: &str) -> AskPassOutcome {
        match classify_prompt(prompt) {
            AskPassPrompt::Password => {
                if self.answered.swap(true, Ordering::SeqCst) {
                    AskPassOutcome::Rejected
                } else {
                    AskPassOutcome::Answered
                }
            }
            other => AskPassOutcome::Unanswerable(other),
        }
    }
}

/// Environment that points `ssh` at a [`PasswordProxy`].
pub struct StoredPasswordAskPass {
    /// Held for as long as the terminal lives: dropping it takes the socket
    /// down and `ssh` would then hang on its next prompt.
    pub proxy: Arc<PasswordProxy>,
    pub env: collections::HashMap<String, String>,
    /// Reports what the proxy did, so the caller can explain a failure. Ends
    /// when `proxy` is dropped.
    pub outcomes: mpsc::UnboundedReceiver<AskPassOutcome>,
}

/// Extra `ssh` options that go with a stored password.
///
/// `NumberOfPasswordPrompts` bounds only the password and keyboard-interactive
/// methods, so public key authentication is untouched by it. `2` is the real
/// attempt plus the one retry that tells us the stored password was refused —
/// [`StoredPasswordPolicy`] fails that retry immediately, and the failure is
/// what produces 「保存されたパスワードが拒否されました」. `1` would give the
/// user nothing but OpenSSH's own English "Permission denied", and the default
/// `3` would send two empty attempts at the server instead of one.
pub const SSH_STORED_PASSWORD_ARGS: [&str; 2] = ["-o", "NumberOfPasswordPrompts=2"];

/// Builds the askpass socket that feeds `password` to `ssh`.
pub async fn stored_password_proxy(
    password: EncryptedPassword,
    executor: BackgroundExecutor,
) -> Result<StoredPasswordAskPass> {
    let (report, outcomes) = mpsc::unbounded();
    let policy = StoredPasswordPolicy::new();

    let get_password = move |prompt: String| {
        let outcome = policy.decide(&prompt);
        // The prompt itself is not logged: OpenSSH puts the account and host in
        // it, and there is no reason to spread that around.
        log::debug!("quick connect askpass: {outcome:?}");
        // The receiver is gone once the terminal is closed; the proxy still has
        // to answer, it just has nobody left to tell.
        report.unbounded_send(outcome).ok();
        match outcome {
            AskPassOutcome::Answered => Task::ready(ControlFlow::Continue(Ok(password.clone()))),
            // An `Err` makes the proxy close the connection without writing,
            // which `ssh` reads as an empty answer and reports as a failed
            // attempt. Returning `Break` instead would hold the socket open and
            // leave `ssh` waiting forever.
            AskPassOutcome::Rejected => Task::ready(ControlFlow::Continue(Err(anyhow!(
                "the stored password was already offered once"
            )))),
            AskPassOutcome::Unanswerable(kind) => Task::ready(ControlFlow::Continue(Err(anyhow!(
                "a stored password cannot answer {kind:?}"
            )))),
        }
    };

    let proxy = PasswordProxy::new(Box::new(get_password), executor).await?;

    let mut env = collections::HashMap::default();
    // `force` is what makes OpenSSH ≥ 8.4 use askpass even though the terminal
    // has a tty and no DISPLAY is set.
    env.insert("SSH_ASKPASS_REQUIRE".to_string(), "force".to_string());
    env.insert(
        "SSH_ASKPASS".to_string(),
        proxy.script_path().as_ref().to_string_lossy().into_owned(),
    );
    #[cfg(target_os = "windows")]
    env.insert(
        "ZED_ASKPASS_SOCKET".to_string(),
        proxy.socket_path().as_ref().to_string_lossy().into_owned(),
    );

    Ok(StoredPasswordAskPass {
        proxy: Arc::new(proxy),
        env,
        outcomes,
    })
}

// -------------------------------------------------------------------------
// Telnet: typing the answer to a login prompt
// -------------------------------------------------------------------------

/// Answers a telnet device's login prompts once, then disarms.
///
/// Gives up — without sending anything — when the user types first, when 30
/// seconds pass without a prompt, or when the terminal goes away. It answers at
/// most one password prompt, so a device that rejects the stored password is
/// left to the user rather than locked out by repeated attempts.
pub fn answer_telnet_login(
    terminal: &Entity<Terminal>,
    user: Option<String>,
    password: EncryptedPassword,
    cx: &mut AsyncApp,
) -> Task<()> {
    let terminal = terminal.downgrade();
    cx.spawn(async move |cx| {
        if let Err(error) = watch_telnet_login(terminal, user, password, cx).await {
            log::warn!("stopped the telnet auto-login: {error:#}");
        }
    })
}

async fn watch_telnet_login(
    terminal: WeakEntity<Terminal>,
    user: Option<String>,
    password: EncryptedPassword,
    cx: &mut AsyncApp,
) -> Result<()> {
    let entity = terminal.upgrade().context("the terminal is gone")?;
    let (wake_tx, wake_rx) = async_channel::unbounded::<()>();
    // Registered before the first look at the grid, so a prompt printed while
    // we scan still wakes us.
    let _subscription = cx.update(|cx| {
        cx.subscribe(&entity, move |_, event: &Event, _| {
            if matches!(event, Event::Wakeup) {
                // Unbounded, so the only failure is a closed channel, which
                // just means this watcher has already finished.
                wake_tx.try_send(()).ok();
            }
        })
    });
    drop(entity);

    let mut wake_rx = Box::pin(wake_rx);
    let mut timeout = cx.background_executor().timer(TELNET_LOGIN_TIMEOUT);
    // Taken when spent: the password may only be typed once.
    let mut password = Some(password);
    let mut user_to_send = user.filter(|user| !user.trim().is_empty());

    loop {
        let Some(entity) = terminal.upgrade() else {
            return Ok(());
        };
        let (typed, last_line) = entity.read_with(cx, |terminal, _| {
            (
                terminal.keyboard_input_sent(),
                terminal
                    .last_n_non_empty_lines(TELNET_SCAN_LINES)
                    .last()
                    .cloned(),
            )
        });
        // The user took over; typing into the middle of what they are doing
        // would be worse than not helping at all.
        if typed {
            return Ok(());
        }

        if let Some(line) = last_line.as_deref().map(str::trim_end) {
            if PASSWORD_PROMPT.is_match(line) {
                let password = password.take().context("the password was already sent")?;
                let mut secret = password
                    .decrypt(IKnowWhatIAmDoingAndIHaveReadTheDocs)
                    .context("couldn't decrypt the saved password")?;
                // The log taps the PTY's read side, so it is the device's echo —
                // not the write — that would leak the password (ADR-F29).
                entity.update(cx, |terminal, _| {
                    terminal.pause_session_log();
                    terminal.write_secret(&secret);
                });
                secret.zeroize();
                // Fixed rather than clever: the echo of a line cannot be
                // observed directly, and a wait keyed on the next wakeup would
                // resume in the middle of a device that echoes character by
                // character over a slow link. What this costs is up to
                // `PASSWORD_ECHO_PAUSE` of banner, and the marker says so.
                let settle = cx.background_executor().timer(PASSWORD_ECHO_PAUSE);
                settle.await;
                if let Some(entity) = terminal.upgrade() {
                    entity.update(cx, |terminal, _| terminal.resume_session_log());
                }
                return Ok(());
            } else if let Some(user) = user_to_send.as_deref()
                && LOGIN_PROMPT.is_match(line)
            {
                let line = format!("{user}\r").into_bytes();
                entity.update(cx, |terminal, _| terminal.write_init_command(line));
                user_to_send = None;
            }
        }
        drop(entity);

        let mut next_wake = wake_rx.next();
        match futures::future::select(&mut next_wake, &mut timeout).await {
            futures::future::Either::Left((Some(()), _)) => continue,
            futures::future::Either::Left((None, _)) => return Ok(()),
            futures::future::Either::Right(((), _)) => return Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connection(protocol: ConnectionProtocol, host: &str, port: Option<u16>) -> SavedConnection {
        SavedConnection {
            name: "テスト".to_string(),
            protocol,
            host: host.to_string(),
            port,
            ..SavedConnection::default()
        }
    }

    #[test]
    fn credential_url_keys_on_protocol_host_and_port() {
        assert_eq!(
            credential_url(&connection(
                ConnectionProtocol::Ssh,
                "192.168.100.150",
                None
            )),
            "zed-quick-connect+ssh://192.168.100.150:22"
        );
        assert_eq!(
            credential_url(&connection(ConnectionProtocol::Ssh, "host", Some(2222))),
            "zed-quick-connect+ssh://host:2222"
        );
        assert_eq!(
            credential_url(&connection(ConnectionProtocol::Telnet, "sw1", None)),
            "zed-quick-connect+telnet://sw1:23"
        );
    }

    #[test]
    fn credential_url_ignores_the_entry_name_and_user() {
        let mut renamed = connection(ConnectionProtocol::Ssh, "host", Some(22));
        renamed.name = "別の名前".to_string();
        renamed.user = Some("ubuntu".to_string());
        renamed.folder = Some("拠点/本社".to_string());
        assert_eq!(
            credential_url(&renamed),
            credential_url(&connection(ConnectionProtocol::Ssh, "host", Some(22)))
        );
    }

    #[test]
    fn credential_url_never_collides_with_remote_project_keys() {
        let url = credential_url(&connection(ConnectionProtocol::Ssh, "host", Some(22)));
        assert!(url.starts_with(CREDENTIAL_SCHEME_PREFIX));
        assert!(!url.starts_with("ssh://"));
    }

    #[test]
    fn credential_username_normalizes_missing_and_empty() {
        assert_eq!(credential_username(None), "");
        assert_eq!(credential_username(Some("  ")), "");
        assert_eq!(credential_username(Some(" ubuntu ")), "ubuntu");
    }

    #[test]
    fn prompts_are_classified_by_what_they_ask_for() {
        assert_eq!(
            classify_prompt("ubuntu@192.168.100.150's password: "),
            AskPassPrompt::Password
        );
        assert_eq!(
            classify_prompt("Password for ubuntu@host:"),
            AskPassPrompt::Password
        );
        assert_eq!(
            classify_prompt("Enter passphrase for key '/home/u/.ssh/id_ed25519': "),
            AskPassPrompt::Passphrase
        );
        assert_eq!(
            classify_prompt(
                "The authenticity of host 'h (1.2.3.4)' can't be established.\n\
                 Are you sure you want to continue connecting (yes/no/[fingerprint])? "
            ),
            AskPassPrompt::HostKey
        );
        assert_eq!(classify_prompt("Verification code: "), AskPassPrompt::Other);
    }

    #[test]
    fn the_stored_password_answers_only_the_first_prompt() {
        let policy = StoredPasswordPolicy::new();
        assert_eq!(
            policy.decide("user@host's password: "),
            AskPassOutcome::Answered
        );
        assert_eq!(
            policy.decide("user@host's password: "),
            AskPassOutcome::Rejected
        );
        assert_eq!(
            policy.decide("user@host's password: "),
            AskPassOutcome::Rejected
        );
    }

    #[test]
    fn a_passphrase_prompt_does_not_consume_the_one_password_answer() {
        let policy = StoredPasswordPolicy::new();
        assert_eq!(
            policy.decide("Enter passphrase for key 'id_rsa': "),
            AskPassOutcome::Unanswerable(AskPassPrompt::Passphrase)
        );
        assert_eq!(
            policy.decide("user@host's password: "),
            AskPassOutcome::Answered
        );
    }

    #[test]
    fn only_the_outcomes_that_stop_the_connection_are_reported() {
        assert!(AskPassOutcome::Rejected.message().is_some());
        assert!(
            AskPassOutcome::Unanswerable(AskPassPrompt::HostKey)
                .message()
                .is_some()
        );
        // ssh moves on to the next method after these, so a connection that
        // then succeeds must not raise an alarm.
        assert!(AskPassOutcome::Answered.message().is_none());
        for prompt in [AskPassPrompt::Passphrase, AskPassPrompt::Other] {
            assert!(AskPassOutcome::Unanswerable(prompt).message().is_none());
        }
    }

    #[test]
    fn login_prompts_match_only_at_the_end_of_a_line() {
        for prompt in [
            "login: ",
            "Login:",
            "Username: ",
            "user name:",
            "sw1 login:",
            "ユーザー名を入力 login：",
        ] {
            assert!(LOGIN_PROMPT.is_match(prompt), "{prompt:?} should match");
        }
        for other in [
            "Last login: Mon Sep  1 10:00:00 2025 from 192.168.1.2",
            "login incorrect",
            "ubuntu@host:~$ ",
        ] {
            assert!(!LOGIN_PROMPT.is_match(other), "{other:?} should not match");
        }
    }

    #[test]
    fn password_prompts_match_only_at_the_end_of_a_line() {
        for prompt in [
            "Password: ",
            "password:",
            "Enter password: ",
            "Passcode: ",
            "Pass:",
            "パスワード：",
        ] {
            assert!(PASSWORD_PROMPT.is_match(prompt), "{prompt:?} should match");
        }
        for other in [
            "Password: incorrect",
            "Last password change: never",
            "ubuntu@host:~$ ",
            "Permission denied, please try again.",
        ] {
            assert!(
                !PASSWORD_PROMPT.is_match(other),
                "{other:?} should not match"
            );
        }
    }

    #[test]
    fn a_login_prompt_is_not_mistaken_for_a_password_prompt() {
        assert!(!PASSWORD_PROMPT.is_match("login: "));
        assert!(!LOGIN_PROMPT.is_match("Password: "));
    }
}
