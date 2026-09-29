//! zed-jp-enhance: the wire revision check every connection starts with.
//!
//! A remote server runs under a name that carries this fork's wire revision
//! (`transport::server_binary_version`), but a name only says what a binary
//! was meant to be. Several of the fork's fields fail open when the other end
//! numbers or reads them differently — a host's `Entry.is_private` read as
//! absent opens `.env` to agents — so the two ends compare
//! `JP_ENHANCE_WIRE_REVISION` before anything else crosses:
//!
//! - The client asks right after the first ping and before the egress
//!   allowlist, and `RemoteClient::new` hands out no client (so no project can
//!   add a worktree through it) until the answer matches. On a mismatch, or
//!   no answer, the server binary is reinstalled once and the connection is
//!   made again.
//! - Either end that requires it handles nothing but pings and the check
//!   itself from a peer that has not shown the same revision. The remote
//!   server requires it of every client; the client requires it of the server
//!   too, which keeps the HTTP relay handler shut until then.

use super::{
    ChannelClient, CommandTemplate, ConnectionIdentifier, ConnectionPool, Interactive,
    RemoteClient, RemoteClientDelegate, RemoteConnection, connect,
};
use anyhow::{Context as _, Result, anyhow};
use collections::HashMap;
use futures::{
    FutureExt as _,
    channel::{mpsc, oneshot},
    future::{BoxFuture, Shared},
};
use gpui::{App, AppContext as _, AsyncApp, BorrowAppContext as _, Entity, FutureExt as _, Task};
use rpc::{
    AnyProtoClient, ErrorExt as _,
    proto::{
        self, Envelope, EnvelopedMessage as _, envelope::Payload,
        jp_enhance::JP_ENHANCE_WIRE_REVISION,
    },
};
use std::{
    fmt,
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering::SeqCst},
    },
    time::Duration,
};

/// How long the client waits for the server to answer the check. The server
/// has just answered a ping, so a server that knows the check answers at
/// once; one that does not (upstream's, or this fork's from before the check)
/// never answers at all.
pub(super) const WIRE_REVISION_TIMEOUT: Duration = Duration::from_secs(10);

const REMOVE_SERVER_BINARY_TIMEOUT: Duration = Duration::from_secs(30);

/// What a `ChannelClient` knows of its peer's wire revision.
#[derive(Default)]
pub(super) struct PeerWireRevision {
    /// Whether messages from the peer wait for a matching revision.
    required: AtomicBool,
    confirmed: AtomicBool,
}

/// Makes a remote server's session answer the check with another revision
/// (`Some`), or not at all (`None`), as a server of another build would.
/// Set on the server's app.
#[cfg(any(test, feature = "test-support"))]
pub struct WireRevisionAnswerForTests(pub Option<u32>);

#[cfg(any(test, feature = "test-support"))]
impl gpui::Global for WireRevisionAnswerForTests {}

/// Set on a remote server's app to apply the display language a client sends
/// with the check: the tag it sent, or `None` when it sent none (a client
/// from before the field), which means English. The catalogs live in a crate
/// this one does not depend on, hence the function.
///
/// Called before the check is answered, so everything the server sends the
/// client after that is in the client's language. The language is the
/// process's, and a server shared by several connections writes in the
/// language of the one that connected last.
pub struct DisplayLanguageHandler(pub fn(Option<&str>));

impl gpui::Global for DisplayLanguageHandler {}

/// The environment variable that tells the processes `terminal-session`
/// starts on the remote host the client's display language. A server from
/// before it ignores it.
pub const DISPLAY_LANGUAGE_ENV_VAR: &str = "ZED_UI_LOCALE";

/// Makes the client send this display language with the check (`None`:
/// send none, as a client from before the field), instead of the one its UI
/// is shown in. Set on the client's app.
#[cfg(any(test, feature = "test-support"))]
pub struct DisplayLanguageForTests(pub Option<String>);

#[cfg(any(test, feature = "test-support"))]
impl gpui::Global for DisplayLanguageForTests {}

/// The display language the client sends with the check.
pub(super) fn display_language(cx: &AsyncApp) -> Option<String> {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(language) =
        cx.try_read_global::<DisplayLanguageForTests, _>(|language, _| language.0.clone())
    {
        return language;
    }
    #[cfg(not(any(test, feature = "test-support")))]
    let _ = cx;
    Some(i18n::current_locale().to_string())
}

impl ChannelClient {
    /// Handles nothing from the peer but pings and the check until the peer
    /// has shown this build's wire revision.
    pub(crate) fn require_confirmed_wire_revision(&self) {
        self.wire_revision.required.store(true, SeqCst);
    }

    #[cfg(test)]
    pub(crate) fn wire_revision_confirmed(&self) -> bool {
        self.wire_revision.confirmed.load(SeqCst)
    }
}

impl RemoteClient {
    /// The session end of a connection on a remote server, which serves a
    /// client only once it has shown this build's wire revision.
    pub fn server_proto_client_from_channels(
        incoming_rx: mpsc::UnboundedReceiver<Envelope>,
        outgoing_tx: mpsc::UnboundedSender<Envelope>,
        cx: &App,
        name: &'static str,
        has_wsl_interop: bool,
    ) -> AnyProtoClient {
        let client = ChannelClient::new(incoming_rx, outgoing_tx, cx, name, has_wsl_interop);
        client.require_confirmed_wire_revision();
        client.into()
    }

    /// Connects over `remote_connection` and hands out the client once the
    /// server has shown this build's wire revision. A server that shows
    /// another, or none, is reinstalled and connected to again, once.
    ///
    /// Resolves to `None` if `cancellation` fires (or its sender is dropped)
    /// first.
    pub fn new(
        unique_identifier: ConnectionIdentifier,
        remote_connection: Arc<dyn RemoteConnection>,
        cancellation: oneshot::Receiver<()>,
        delegate: Arc<dyn RemoteClientDelegate>,
        cx: &mut App,
    ) -> Task<Result<Option<Entity<Self>>>> {
        let cancelled: Cancelled = cancellation.map(|_| ()).boxed().shared();
        cx.spawn(async move |cx| {
            let steps = manual_reinstall_steps(remote_connection.as_ref());
            let first = connect_until_cancelled(
                unique_identifier.clone(),
                remote_connection.clone(),
                cancelled.clone(),
                delegate.clone(),
                cx,
            )
            .await;
            let mut reinstall_cx = cx.clone();
            let mut retry_cx = cx.clone();
            let reinstall_delegate = delegate.clone();
            let reinstall_cancelled = cancelled.clone();
            with_one_reinstall(
                async { first },
                move || async move {
                    smol::future::or(
                        async {
                            reinstall_cancelled.await;
                            Err(ReinstallFailure::Cancelled)
                        },
                        reinstall_server(
                            &remote_connection,
                            &reinstall_delegate,
                            &mut reinstall_cx,
                        ),
                    )
                    .await
                },
                move |connection| async move {
                    connect_until_cancelled(
                        unique_identifier,
                        connection,
                        cancelled,
                        delegate,
                        &mut retry_cx,
                    )
                    .await
                },
                steps,
                steps_after_reinstall(),
            )
            .await
        })
    }
}

/// Resolves once the caller of `RemoteClient::new` cancels, or drops the
/// sender, which the attempts it makes take the same way.
type Cancelled = Shared<BoxFuture<'static, ()>>;

async fn connect_until_cancelled(
    unique_identifier: ConnectionIdentifier,
    remote_connection: Arc<dyn RemoteConnection>,
    cancelled: Cancelled,
    delegate: Arc<dyn RemoteClientDelegate>,
    cx: &mut AsyncApp,
) -> Result<Option<Entity<RemoteClient>>> {
    let (cancel_tx, cancel_rx) = oneshot::channel();
    // Held until the attempt is over: dropping it earlier drops `cancel_tx`,
    // which the attempt would take for a cancellation.
    let _forward_cancellation = cx.background_spawn(async move {
        cancelled.await;
        cancel_tx.send(()).ok();
    });
    cx.update(|cx| {
        RemoteClient::new_without_reinstall(
            unique_identifier,
            remote_connection,
            cancel_rx,
            delegate,
            cx,
        )
    })
    .await
}

/// Handles `incoming` if it is the check, or if it has to wait for one.
/// Returns whether it was handled here, in which case it must not reach the
/// message handlers.
pub(super) fn intercept(this: &Arc<ChannelClient>, incoming: &Envelope, cx: &AsyncApp) -> bool {
    if incoming.responding_to.is_some() {
        return false;
    }
    match &incoming.payload {
        Some(Payload::CheckWireRevision(check)) => {
            answer(
                this,
                incoming.id,
                check.revision,
                check.display_language.as_deref(),
                cx,
            );
            true
        }
        // What the connection is known to be alive by, which carries nothing.
        Some(Payload::Ping(_)) => false,
        _ if this.wire_revision.required.load(SeqCst)
            && !this.wire_revision.confirmed.load(SeqCst) =>
        {
            let kind = incoming
                .payload
                .as_ref()
                .map(|payload| {
                    let debug = format!("{payload:?}");
                    debug
                        .split(|character: char| !character.is_alphanumeric())
                        .next()
                        .unwrap_or_default()
                        .to_string()
                })
                .unwrap_or_else(|| "an unknown message".to_string());
            log::warn!(
                "{}: refusing {kind} ({}) from a peer that has not shown wire revision {}",
                this.name,
                incoming.id,
                JP_ENHANCE_WIRE_REVISION
            );
            if let Err(error) = AnyProtoClient::from(this.clone()).send_response(
                incoming.id,
                anyhow!(
                    "the peer has not shown wire revision {JP_ENHANCE_WIRE_REVISION}; \
                     nothing but pings is handled"
                )
                .to_proto(),
            ) {
                log::error!("{}: could not refuse message: {error:#}", this.name);
            }
            true
        }
        _ => false,
    }
}

fn answer(
    this: &Arc<ChannelClient>,
    request_id: u32,
    revision: u32,
    display_language: Option<&str>,
    cx: &AsyncApp,
) {
    #[cfg(any(test, feature = "test-support"))]
    let ours = match cx.try_read_global::<WireRevisionAnswerForTests, _>(|answer, _| answer.0) {
        Some(Some(revision)) => revision,
        Some(None) => return,
        None => JP_ENHANCE_WIRE_REVISION,
    };
    #[cfg(not(any(test, feature = "test-support")))]
    let ours = {
        let _ = cx;
        JP_ENHANCE_WIRE_REVISION
    };

    let matches = revision == ours;
    this.wire_revision.confirmed.store(matches, SeqCst);
    if matches {
        if let Some(apply) = cx.try_read_global::<DisplayLanguageHandler, _>(|handler, _| handler.0)
        {
            apply(display_language);
        }
    } else {
        log::warn!(
            "{}: the peer speaks wire revision {revision} and this build {ours}; \
             nothing but pings will be handled",
            this.name
        );
    }
    let mut envelope = proto::CheckWireRevisionResponse { revision: ours }.into_envelope(
        0,
        Some(request_id),
        None,
    );
    envelope.id = this.next_message_id.fetch_add(1, SeqCst);
    if let Err(error) = this.outgoing_tx.lock().unbounded_send(envelope) {
        log::debug!(
            "{}: the connection went before the check was answered: {error}",
            this.name
        );
    }
}

/// Asks the server behind `client` for its wire revision; the future fails
/// unless it is this build's.
///
/// The question goes out before this returns, past the replay buffer, so that
/// on a reconnect it reaches the server ahead of everything replayed. A
/// reconnect must not wait for the answer before it asks for the replay,
/// though: the server's messages in the meantime acknowledge the client's
/// acknowledgement of the new connection, which is newer than the messages
/// still waiting to be replayed, and would drop them from the replay buffer.
pub(super) fn confirm(
    client: &Arc<ChannelClient>,
    display_language: Option<String>,
) -> impl Future<Output = Result<()>> {
    let response = client.request_internal(
        proto::CheckWireRevision {
            revision: JP_ENHANCE_WIRE_REVISION,
            display_language,
        },
        false,
    );
    let timeout = client.executor.timer(WIRE_REVISION_TIMEOUT);
    let client = client.clone();
    async move {
        confirmed(&client, response, timeout, async {
            client.ping(super::HEARTBEAT_TIMEOUT).await
        })
        .await
    }
}

enum Answer {
    Revision(u32),
    Refused,
    Unanswered,
}

/// `still_there` is asked only when no answer came in time. A mismatch is
/// what removes the server binary on the host, so a connection that went
/// away while the question was out has to fail as itself, not as one.
async fn confirmed(
    client: &ChannelClient,
    response: impl Future<Output = Result<proto::CheckWireRevisionResponse>>,
    timeout: impl Future,
    still_there: impl Future<Output = Result<()>>,
) -> Result<()> {
    let answer = smol::future::or(
        async {
            match response.await {
                Ok(response) => Ok(Answer::Revision(response.revision)),
                Err(error) if error.downcast_ref::<oneshot::Canceled>().is_some() => Err(error),
                Err(error) => {
                    log::warn!("the remote server refused the wire revision check: {error:#}");
                    Ok(Answer::Refused)
                }
            }
        },
        async {
            timeout.await;
            Ok(Answer::Unanswered)
        },
    )
    .await;
    let theirs = match answer {
        Ok(Answer::Revision(theirs)) => Some(theirs),
        Ok(Answer::Refused) => None,
        Ok(Answer::Unanswered) => {
            if let Err(error) = still_there.await {
                client.wire_revision.confirmed.store(false, SeqCst);
                return Err(error.context(i18n::t!(
                    "The connection to the remote server was lost while checking its protocol revision"
                )));
            }
            log::warn!("the remote server did not answer the wire revision check");
            None
        }
        Err(error) => {
            client.wire_revision.confirmed.store(false, SeqCst);
            return Err(error.context(i18n::t!(
                "The connection to the remote server was lost while checking its protocol revision"
            )));
        }
    };
    let matches = theirs == Some(JP_ENHANCE_WIRE_REVISION);
    // A reconnection finds the flag set by the connection before it; a server
    // that now answers otherwise gets nothing more handled.
    client.wire_revision.confirmed.store(matches, SeqCst);
    if matches {
        return Ok(());
    }
    Err(WireRevisionMismatch {
        ours: JP_ENHANCE_WIRE_REVISION,
        theirs,
    }
    .into())
}

/// The remote server showed another wire revision (`theirs`), or none.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WireRevisionMismatch {
    pub ours: u32,
    pub theirs: Option<u32>,
}

impl fmt::Display for WireRevisionMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.theirs {
            Some(theirs) => f.write_str(&i18n::t!(
                "The remote server does not match this Zed (protocol revision: {ours} in this Zed, {theirs} on the server).",
                ours = self.ours
            )),
            None => f.write_str(i18n::t!(
                "The remote server does not match this Zed (it did not answer the protocol revision check; \
                 it may be a server from the official Zed or from an older version of this Zed)."
            )),
        }
    }
}

impl std::error::Error for WireRevisionMismatch {}

pub(super) enum ReinstallFailure {
    /// There is no server binary this can remove on that host.
    NotPossible,
    Failed(anyhow::Error),
    Cancelled,
}

/// Runs `first`; if the server's wire revision did not match, reinstalls the
/// server with `reinstall` and runs `retry` over the connection it made, once.
/// `steps` tells the user how to do by hand what did not work, and
/// `steps_after_reinstall` what is left once a fresh install still does not
/// match, when removing the host's binary again would change nothing.
pub(super) async fn with_one_reinstall<T, C, First, Reinstalled, Retried>(
    first: First,
    reinstall: impl FnOnce() -> Reinstalled,
    retry: impl FnOnce(C) -> Retried,
    steps: String,
    steps_after_reinstall: String,
) -> Result<T>
where
    T: Default,
    First: Future<Output = Result<T>>,
    Reinstalled: Future<Output = Result<C, ReinstallFailure>>,
    Retried: Future<Output = Result<T>>,
{
    let error = match first.await {
        Err(error) => error,
        done => return done,
    };
    let Some(mismatch) = error.downcast_ref::<WireRevisionMismatch>().copied() else {
        return Err(error);
    };
    log::warn!("{mismatch} Reinstalling the remote server once.");
    let connection = match reinstall().await {
        Ok(connection) => connection,
        Err(ReinstallFailure::Cancelled) => return Ok(T::default()),
        Err(ReinstallFailure::NotPossible) => {
            return Err(anyhow!(i18n::t!(
                "{mismatch} The server cannot be reinstalled automatically on this host. {steps}"
            )));
        }
        Err(ReinstallFailure::Failed(error)) => {
            return Err(error.context(i18n::t!(
                "{mismatch} The server could not be reinstalled. {steps}"
            )));
        }
    };
    match retry(connection).await {
        Err(error) => match error.downcast_ref::<WireRevisionMismatch>() {
            Some(again) => Err(anyhow!(i18n::t!(
                "{again} The server still does not match after it was reinstalled. {steps_after_reinstall}"
            ))),
            None => Err(error),
        },
        done => done,
    }
}

fn manual_reinstall_steps(connection: &dyn RemoteConnection) -> String {
    let folder = if connection.remote_platform().os.is_windows() {
        "%USERPROFILE%\\.zed_server"
    } else {
        "~/.zed_server"
    };
    i18n::t!(
        "On the remote host, delete the files in {folder} whose names start with zed-remote-server-, \
         then reconnect."
    )
}

/// What is left when the server this Zed installs does not match it either:
/// the copy it uploads comes from this machine's cache (or, for a build from
/// source, from the build itself).
fn steps_after_reinstall() -> String {
    i18n::t!(
        "The server that this Zed installs on the remote host may itself not match. \
         Delete {folder} on this computer and reconnect, or update Zed.",
        folder = paths::remote_servers_dir().display()
    )
}

/// Removes the server binary `remote_connection` runs and makes a new
/// connection, which installs the server afresh.
async fn reinstall_server(
    remote_connection: &Arc<dyn RemoteConnection>,
    delegate: &Arc<dyn RemoteClientDelegate>,
    cx: &mut AsyncApp,
) -> Result<Arc<dyn RemoteConnection>, ReinstallFailure> {
    let Some(template) = server_binary_removal_command(remote_connection.as_ref()) else {
        return Err(ReinstallFailure::NotPossible);
    };
    let template = template.map_err(ReinstallFailure::Failed)?;
    delegate.set_status(
        Some(i18n::t!(
            "Reinstalling the remote server because it does not match this Zed"
        )),
        cx,
    );
    let output = cx
        .background_spawn(async move {
            let mut command = util::command::new_command(&template.program);
            command
                .args(&template.args)
                .envs(template.env)
                .stdin(util::command::Stdio::null())
                .stdout(util::command::Stdio::piped())
                .stderr(util::command::Stdio::piped())
                .kill_on_drop(true);
            command.output().await
        })
        .with_timeout(REMOVE_SERVER_BINARY_TIMEOUT, cx.background_executor())
        .await
        .map_err(|_| {
            ReinstallFailure::Failed(anyhow!(i18n::t!(
                "The remote host did not finish deleting the server file within {seconds} seconds",
                seconds = REMOVE_SERVER_BINARY_TIMEOUT.as_secs()
            )))
        })?
        .context(i18n::t!(
            "Could not start the command that deletes the server file"
        ))
        .map_err(ReinstallFailure::Failed)?;
    if !output.status.success() {
        return Err(ReinstallFailure::Failed(anyhow!(i18n::t!(
            "Could not delete the server file on the remote host ({status}): {error}",
            status = output.status,
            error = String::from_utf8_lossy(&output.stderr).trim()
        ))));
    }
    // The server binary is only installed when a connection is made, and the
    // pool would hand this one back. It is forgotten rather than killed: WSL's
    // cannot be, and killing an SSH master would cut off every other window on
    // that host along with it.
    let connection_options = remote_connection.connection_options();
    cx.update(|cx| {
        cx.update_default_global(|pool: &mut ConnectionPool, _| {
            pool.connections.remove(&connection_options);
        })
    });
    connect(connection_options, delegate.clone(), cx)
        .await
        .map_err(ReinstallFailure::Failed)
}

/// The command that removes the server binary `connection` runs, when that is
/// a file this can safely remove: only ever a `zed-remote-server-*` directly
/// in the server directory, and only on a POSIX host, where a running binary
/// can be removed and `$HOME` names the directory the path is relative to.
fn server_binary_removal_command(
    connection: &dyn RemoteConnection,
) -> Option<Result<CommandTemplate>> {
    if connection.remote_platform().os.is_windows() {
        return None;
    }
    let path = connection.remote_server_binary_path()?;
    let is_server_binary = path.parent() == Some(paths::remote_server_dir_relative())
        && path.file_name().is_some_and(|name| {
            name.starts_with("zed-remote-server-")
                && name.chars().all(|character| {
                    character.is_ascii_alphanumeric() || "._+-".contains(character)
                })
        });
    if !is_server_binary {
        log::warn!(
            "not removing {}, which is not a server binary in the server directory",
            path.as_unix_str()
        );
        return None;
    }
    // The path goes into the script itself, not in as `$1`: WSL hands the
    // command line to the distro's shell once more before `sh` sees it, which
    // expands `$1` there, to nothing (`rm -f -- "$HOME/"`), and strips a layer
    // of quotes. A name of only the characters above means the same unquoted,
    // and `$HOME` means the same at whichever layer expands it.
    Some(connection.build_command(
        Some("sh".to_string()),
        &[
            "-c".to_string(),
            format!("rm -f -- \"$HOME\"/{}", path.as_unix_str()),
        ],
        &HashMap::default(),
        None,
        None,
        Interactive::No,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        RemoteConnectionOptions, RemotePlatform,
        remote_client::{RemoteArch, RemoteOs},
        transport::mock::{MockConnectionOptions, MockDelegate},
    };
    use async_trait::async_trait;
    use futures::{StreamExt as _, channel::mpsc::Sender};
    use gpui::TestAppContext;
    use parking_lot::Mutex;
    use std::{cell::Cell, path::PathBuf};
    use util::{
        paths::{PathStyle, RemotePathBuf},
        rel_path::RelPath,
    };

    fn mismatch(theirs: Option<u32>) -> anyhow::Error {
        WireRevisionMismatch {
            ours: JP_ENHANCE_WIRE_REVISION,
            theirs,
        }
        .into()
    }

    #[test]
    fn a_server_of_another_revision_is_reinstalled_once() {
        let reinstalls = Cell::new(0);
        let retries = Cell::new(0);
        let result: Result<Option<u32>> = smol::block_on(with_one_reinstall(
            async { Err(mismatch(Some(1))) },
            || async {
                reinstalls.set(reinstalls.get() + 1);
                Ok("new connection")
            },
            |connection| {
                let retries = &retries;
                async move {
                    assert_eq!(connection, "new connection");
                    retries.set(retries.get() + 1);
                    Ok(Some(7))
                }
            },
            "the steps".to_string(),
            "the steps after reinstalling".to_string(),
        ));
        assert_eq!(result.expect("the retry connected"), Some(7));
        assert_eq!((reinstalls.get(), retries.get()), (1, 1));
    }

    #[test]
    fn a_second_mismatch_ends_with_the_steps_and_no_third_attempt() {
        let reinstalls = Cell::new(0);
        let result: Result<Option<u32>> = smol::block_on(with_one_reinstall(
            async { Err(mismatch(None)) },
            || async {
                reinstalls.set(reinstalls.get() + 1);
                Ok(())
            },
            |()| async { Err(mismatch(Some(1))) },
            "the steps".to_string(),
            "the steps after reinstalling".to_string(),
        ));
        let error = result.expect_err("the second mismatch is final");
        assert_eq!(reinstalls.get(), 1);
        let message = format!("{error:#}");
        assert!(
            message.contains("still does not match after it was reinstalled"),
            "{message}"
        );
        assert!(
            message.contains("the steps after reinstalling"),
            "{message}"
        );
        assert!(message.contains("1 on the server"), "{message}");
    }

    #[test]
    fn other_failures_and_successes_reinstall_nothing() {
        let reinstalled = Cell::new(false);
        let failed: Result<Option<u32>> = smol::block_on(with_one_reinstall(
            async { Err(anyhow!("ssh: connection refused")) },
            || async {
                reinstalled.set(true);
                Ok(())
            },
            |()| async { Ok(Some(1)) },
            String::new(),
            String::new(),
        ));
        assert_eq!(
            format!("{:#}", failed.expect_err("kept")),
            "ssh: connection refused"
        );
        let connected: Result<Option<u32>> = smol::block_on(with_one_reinstall(
            async { Ok(Some(3)) },
            || async {
                reinstalled.set(true);
                Ok(())
            },
            |()| async { Ok(Some(1)) },
            String::new(),
            String::new(),
        ));
        assert_eq!(connected.expect("connected"), Some(3));
        assert!(!reinstalled.get());
    }

    #[test]
    fn a_host_that_cannot_be_reinstalled_gets_the_steps() {
        let retried = Cell::new(false);
        let result: Result<Option<u32>> = smol::block_on(with_one_reinstall(
            async { Err(mismatch(Some(9))) },
            || async { Err::<(), _>(ReinstallFailure::NotPossible) },
            |()| async {
                retried.set(true);
                Ok(Some(1))
            },
            "the steps".to_string(),
            "the steps after reinstalling".to_string(),
        ));
        let message = format!("{:#}", result.expect_err("nothing to retry with"));
        assert!(
            message.contains("cannot be reinstalled automatically"),
            "{message}"
        );
        assert!(message.contains("the steps"), "{message}");
        assert!(!retried.get());

        let cancelled: Result<Option<u32>> = smol::block_on(with_one_reinstall(
            async { Err(mismatch(Some(9))) },
            || async { Err::<(), _>(ReinstallFailure::Cancelled) },
            |()| async { Ok(Some(1)) },
            String::new(),
            String::new(),
        ));
        assert_eq!(cancelled.expect("a cancellation is not an error"), None);
    }

    #[derive(Clone, Copy)]
    enum ScriptedServer {
        Answers(u32),
        Silent,
        /// Answers the first ping, then nothing: the connection went.
        GoesAway,
        /// Answers as this build does, but asks the client to relay HTTP
        /// before the client has checked it.
        AsksBeforeTheCheck,
    }

    /// A remote server that answers the connection's first steps as a server
    /// of some other build would, and notes everything the client sends.
    struct ScriptedConnection {
        server: ScriptedServer,
        received: Arc<Mutex<Vec<String>>>,
        os: RemoteOs,
        server_binary: Option<Arc<RelPath>>,
    }

    fn payload_name(envelope: &Envelope) -> String {
        match &envelope.payload {
            Some(payload) => {
                let debug = format!("{payload:?}");
                debug
                    .split(|character: char| !character.is_alphanumeric())
                    .next()
                    .unwrap_or_default()
                    .to_string()
            }
            None => "none".to_string(),
        }
    }

    #[async_trait(?Send)]
    impl RemoteConnection for ScriptedConnection {
        fn start_proxy(
            &self,
            _unique_identifier: String,
            _reconnect: bool,
            incoming_tx: mpsc::UnboundedSender<Envelope>,
            mut outgoing_rx: mpsc::UnboundedReceiver<Envelope>,
            _connection_activity_tx: Sender<()>,
            _delegate: Arc<dyn RemoteClientDelegate>,
            cx: &mut AsyncApp,
        ) -> Task<Result<i32>> {
            let server = self.server;
            let received = self.received.clone();
            cx.background_spawn(async move {
                let mut next_id = 0;
                let mut reply = |mut envelope: Envelope| {
                    next_id += 1;
                    envelope.id = next_id;
                    incoming_tx.unbounded_send(envelope).ok();
                };
                reply(proto::RemoteStarted {}.into_envelope(0, None, None));
                if let ScriptedServer::AsksBeforeTheCheck = server {
                    reply(
                        proto::ProxyHttpRequest {
                            request_id: 1,
                            method: "GET".to_string(),
                            url: "https://example.com/".to_string(),
                            ..Default::default()
                        }
                        .into_envelope(0, None, None),
                    );
                }
                let mut pings = 0;
                while let Some(envelope) = outgoing_rx.next().await {
                    received.lock().push(payload_name(&envelope));
                    if let Some(Payload::Error(error)) = &envelope.payload {
                        received.lock().push(format!("Error: {}", error.message));
                    }
                    match (&envelope.payload, server) {
                        (Some(Payload::Ping(_)), ScriptedServer::GoesAway) if pings > 0 => {}
                        (Some(Payload::Ping(_)), _) => {
                            pings += 1;
                            reply(proto::Ack {}.into_envelope(0, Some(envelope.id), None))
                        }
                        (
                            Some(Payload::CheckWireRevision(_)),
                            ScriptedServer::Answers(revision),
                        ) => reply(proto::CheckWireRevisionResponse { revision }.into_envelope(
                            0,
                            Some(envelope.id),
                            None,
                        )),
                        (
                            Some(Payload::CheckWireRevision(_)),
                            ScriptedServer::AsksBeforeTheCheck,
                        ) => reply(
                            proto::CheckWireRevisionResponse {
                                revision: JP_ENHANCE_WIRE_REVISION,
                            }
                            .into_envelope(0, Some(envelope.id), None),
                        ),
                        _ => {}
                    }
                }
                Ok(0)
            })
        }

        fn upload_directory(
            &self,
            _src_path: PathBuf,
            _dest_path: RemotePathBuf,
            _cx: &App,
        ) -> Task<Result<()>> {
            Task::ready(Ok(()))
        }

        async fn kill(&self) -> Result<()> {
            Ok(())
        }

        fn has_been_killed(&self) -> bool {
            false
        }

        fn build_command(
            &self,
            program: Option<String>,
            args: &[String],
            env: &HashMap<String, String>,
            _working_dir: Option<String>,
            _port_forward: Option<(u16, String, u16)>,
            _interactive: Interactive,
        ) -> Result<CommandTemplate> {
            Ok(CommandTemplate {
                program: program.context("a scripted connection runs no shell")?,
                args: args.to_vec(),
                env: env.clone(),
            })
        }

        fn build_forward_ports_command(
            &self,
            _forwards: Vec<(u16, String, u16)>,
        ) -> Result<CommandTemplate> {
            Err(anyhow!("a scripted connection forwards nothing"))
        }

        fn connection_options(&self) -> RemoteConnectionOptions {
            RemoteConnectionOptions::Mock(MockConnectionOptions { id: u64::MAX })
        }

        fn path_style(&self) -> PathStyle {
            PathStyle::Unix
        }

        fn remote_platform(&self) -> RemotePlatform {
            RemotePlatform {
                os: self.os,
                arch: RemoteArch::X86_64,
            }
        }

        fn remote_os_version(&self) -> Option<String> {
            None
        }

        fn remote_server_binary_path(&self) -> Option<Arc<RelPath>> {
            self.server_binary.clone()
        }

        fn shell(&self) -> String {
            "sh".to_string()
        }

        fn default_system_shell(&self) -> String {
            "sh".to_string()
        }

        fn has_wsl_interop(&self) -> bool {
            false
        }
    }

    async fn connect_to(
        server: ScriptedServer,
        cx: &mut TestAppContext,
    ) -> (Result<Option<Entity<RemoteClient>>>, Vec<String>) {
        cx.update(|cx| release_channel::init(semver::Version::new(0, 0, 0), cx));
        let received = Arc::new(Mutex::new(Vec::new()));
        let connection = Arc::new(ScriptedConnection {
            server,
            received: received.clone(),
            os: RemoteOs::Linux,
            server_binary: None,
        });
        let (_cancel_tx, cancel_rx) = oneshot::channel();
        let connecting = cx.update(|cx| {
            RemoteClient::new(
                ConnectionIdentifier::setup(),
                connection,
                cancel_rx,
                Arc::new(MockDelegate),
                cx,
            )
        });
        cx.run_until_parked();
        cx.executor().advance_clock(WIRE_REVISION_TIMEOUT);
        cx.run_until_parked();
        // What a server that did not answer the check is pinged within.
        cx.executor().advance_clock(super::super::HEARTBEAT_TIMEOUT);
        cx.run_until_parked();
        let result = connecting.await;
        let received = received.lock().clone();
        (result, received)
    }

    /// The one file a mismatch may remove: a server binary directly in the
    /// server directory of a POSIX host, named only in characters that no
    /// layer of shell parsing changes.
    #[test]
    fn only_a_server_binary_in_the_server_directory_is_removed() {
        let removal = |os: RemoteOs, path: Option<&str>| {
            let connection = ScriptedConnection {
                server: ScriptedServer::Silent,
                received: Arc::default(),
                os,
                server_binary: path.map(|path| {
                    RelPath::from_unix_str(path)
                        .expect("a relative path")
                        .into_arc()
                }),
            };
            server_binary_removal_command(&connection)
                .map(|command| command.expect("the command builds"))
        };
        let binary = ".zed_server/zed-remote-server-stable-1.17.2-jp2";
        let command = removal(RemoteOs::Linux, Some(binary)).expect("removed");
        assert_eq!(command.program, "sh");
        assert_eq!(
            command.args,
            [
                "-c",
                "rm -f -- \"$HOME\"/.zed_server/zed-remote-server-stable-1.17.2-jp2"
            ],
            "no positional parameter, which WSL's extra shell expands to nothing"
        );
        assert!(removal(RemoteOs::MacOs, Some(binary)).is_some());
        let nightly = ".zed_server/zed-remote-server-nightly-1.17.2+nightly.0123abc-jp2";
        assert!(
            removal(RemoteOs::Linux, Some(nightly)).is_some(),
            "{nightly}"
        );

        for kept in [
            "zed-remote-server-stable-1.17.2-jp2",
            ".zed_server/nested/zed-remote-server-stable-1.17.2-jp2",
            ".zed_server/other-file",
            ".zed_server/zed-remote-server",
            "other/zed-remote-server-stable-1.17.2-jp2",
            ".zed_server/zed-remote-server-dev-build;rm -rf ~",
            ".zed_server/zed-remote-server-dev-$HOME",
            ".zed_server/zed-remote-server-dev-a b",
            ".zed_server/zed-remote-server-dev-\"x\"",
        ] {
            assert!(removal(RemoteOs::Linux, Some(kept)).is_none(), "{kept}");
        }
        assert!(removal(RemoteOs::Linux, None).is_none());
        assert!(
            removal(RemoteOs::Windows, Some(binary)).is_none(),
            "a running binary cannot be removed on Windows"
        );
    }

    fn position(received: &[String], name: &str) -> Option<usize> {
        received.iter().position(|received| received == name)
    }

    fn only_the_first_steps(received: &[String]) -> bool {
        received.iter().all(|name| {
            matches!(
                name.as_str(),
                "RemoteStarted" | "Ack" | "Ping" | "CheckWireRevision"
            )
        })
    }

    /// The client's relay would make HTTP requests for the server, so a
    /// server it has not checked yet is refused, not served.
    #[gpui::test]
    async fn the_client_serves_nothing_before_the_server_has_been_checked(cx: &mut TestAppContext) {
        let (result, received) = connect_to(ScriptedServer::AsksBeforeTheCheck, cx).await;
        result
            .expect("the connection should succeed")
            .expect("nothing cancelled it");
        let refusal = received
            .iter()
            .find(|name| name.starts_with("Error: "))
            .unwrap_or_else(|| panic!("the relay request went unanswered: {received:?}"));
        assert!(
            refusal.contains(&format!("wire revision {JP_ENHANCE_WIRE_REVISION}")),
            "{received:?}"
        );
    }

    #[gpui::test]
    async fn a_server_of_this_revision_is_checked_before_anything_of_the_forks(
        cx: &mut TestAppContext,
    ) {
        let (result, received) =
            connect_to(ScriptedServer::Answers(JP_ENHANCE_WIRE_REVISION), cx).await;
        let client = result
            .expect("the connection should succeed")
            .expect("nothing cancelled it");
        let check = position(&received, "CheckWireRevision").expect("the client asks");
        let ping = position(&received, "Ping").expect("the client pings first");
        let allowlist =
            position(&received, "SetEgressAllowlist").expect("the allowlist follows the check");
        assert!(ping < check && check < allowlist, "{received:?}");
        client.read_with(cx, |client, _| {
            assert!(client.client.wire_revision_confirmed());
        });
    }

    #[gpui::test]
    async fn a_server_of_another_revision_fails_the_connection_and_hears_nothing_more(
        cx: &mut TestAppContext,
    ) {
        let (result, received) =
            connect_to(ScriptedServer::Answers(JP_ENHANCE_WIRE_REVISION - 1), cx).await;
        let message = format!("{:#}", result.expect_err("the revisions differ"));
        assert!(message.contains("does not match this Zed"), "{message}");
        assert!(
            message.contains(&format!("{} on the server", JP_ENHANCE_WIRE_REVISION - 1)),
            "{message}"
        );
        // Nothing to remove on this connection, so the user is told how.
        assert!(message.contains("zed-remote-server-"), "{message}");
        assert!(only_the_first_steps(&received), "{received:?}");
    }

    #[gpui::test]
    async fn a_server_that_never_answers_fails_the_connection_and_hears_nothing_more(
        cx: &mut TestAppContext,
    ) {
        let (result, received) = connect_to(ScriptedServer::Silent, cx).await;
        let message = format!("{:#}", result.expect_err("no answer is no match"));
        assert!(
            message.contains("did not answer the protocol revision check"),
            "{message}"
        );
        assert!(
            position(&received, "CheckWireRevision").is_some(),
            "{received:?}"
        );
        assert!(only_the_first_steps(&received), "{received:?}");
    }

    /// A mismatch removes the server binary on the host, so a connection that
    /// merely went away while the check was out must not pass for one.
    #[gpui::test]
    async fn a_connection_lost_during_the_check_is_not_a_mismatch(cx: &mut TestAppContext) {
        let (result, received) = connect_to(ScriptedServer::GoesAway, cx).await;
        let error = result.expect_err("nothing answered");
        assert!(
            error.downcast_ref::<WireRevisionMismatch>().is_none(),
            "{error:#}"
        );
        let message = format!("{error:#}");
        assert!(
            message.contains("connection to the remote server was lost"),
            "{message}"
        );
        assert!(!message.contains("does not match this Zed"), "{message}");
        assert_eq!(
            received.iter().filter(|name| *name == "Ping").count(),
            2,
            "{received:?}"
        );
    }

    #[gpui::test]
    async fn a_mock_server_of_another_build_is_refused_too(
        cx: &mut TestAppContext,
        server_cx: &mut TestAppContext,
    ) {
        cx.update(|cx| release_channel::init(semver::Version::new(0, 0, 0), cx));
        server_cx.update(|cx| cx.set_global(WireRevisionAnswerForTests(None)));
        let (opts, server_session, connect_guard) = RemoteClient::fake_server(cx, server_cx);
        let ping_handler = server_cx.new(|_| ());
        server_session.add_request_handler::<proto::Ping, _, _, _>(
            ping_handler.downgrade(),
            |_, _, _| async { Ok(proto::Ack {}) },
        );
        drop(connect_guard);
        let mut async_cx = cx.to_async();
        let connection = connect(opts, Arc::new(MockDelegate), &mut async_cx)
            .await
            .expect("the transport connects");
        let (_cancel_tx, cancel_rx) = oneshot::channel();
        let connecting = cx.update(|cx| {
            RemoteClient::new(
                ConnectionIdentifier::setup(),
                connection,
                cancel_rx,
                Arc::new(MockDelegate),
                cx,
            )
        });
        cx.run_until_parked();
        server_cx.run_until_parked();
        cx.executor().advance_clock(WIRE_REVISION_TIMEOUT);
        cx.run_until_parked();
        // The ping that tells a server that does not know the check from one
        // that is gone.
        server_cx.run_until_parked();
        cx.run_until_parked();
        let message = format!("{:#}", connecting.await.expect_err("no answer is no match"));
        assert!(
            message.contains("did not answer the protocol revision check"),
            "{message}"
        );
    }

    /// The server end serves nothing but the check until a client of its own
    /// revision has asked, and a client of another revision is told which
    /// revision the server speaks.
    #[gpui::test]
    async fn a_server_session_serves_nothing_until_the_revisions_match(cx: &mut TestAppContext) {
        let (incoming_tx, incoming_rx) = mpsc::unbounded::<Envelope>();
        let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded::<Envelope>();
        let session = cx.update(|cx| {
            RemoteClient::server_proto_client_from_channels(
                incoming_rx,
                outgoing_tx,
                cx,
                "server",
                false,
            )
        });
        let handled = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let handler_owner = cx.new(|_| ());
        session.add_request_handler::<proto::AllocateWorktreeId, _, _, _>(
            handler_owner.downgrade(),
            {
                let handled = handled.clone();
                move |_, _, _| {
                    handled.fetch_add(1, SeqCst);
                    async { Ok(proto::AllocateWorktreeIdResponse { worktree_id: 1 }) }
                }
            },
        );

        let mut answer_to = async |envelope: Envelope, cx: &mut TestAppContext| {
            let id = envelope.id;
            incoming_tx
                .unbounded_send(envelope)
                .expect("the session listens");
            cx.run_until_parked();
            loop {
                let answer = outgoing_rx.next().await.expect("the session answers");
                if answer.responding_to == Some(id) {
                    return answer;
                }
            }
        };
        let allocate =
            |id| proto::AllocateWorktreeId { project_id: 0 }.into_envelope(id, None, None);
        let check = |id, revision| {
            proto::CheckWireRevision {
                revision,
                display_language: None,
            }
            .into_envelope(id, None, None)
        };
        let is_error = |envelope: &Envelope| matches!(envelope.payload, Some(Payload::Error(_)));

        let refused = answer_to(allocate(1), cx).await;
        assert!(is_error(&refused), "{refused:?}");

        let told = answer_to(check(2, JP_ENHANCE_WIRE_REVISION - 1), cx).await;
        assert_eq!(
            proto::CheckWireRevisionResponse::from_envelope(told).map(|told| told.revision),
            Some(JP_ENHANCE_WIRE_REVISION)
        );
        let refused = answer_to(allocate(3), cx).await;
        assert!(is_error(&refused), "{refused:?}");
        assert_eq!(
            handled.load(SeqCst),
            0,
            "a refused request reaches no handler"
        );

        answer_to(check(4, JP_ENHANCE_WIRE_REVISION), cx).await;
        let served = answer_to(allocate(5), cx).await;
        assert!(
            proto::AllocateWorktreeIdResponse::from_envelope(served.clone()).is_some(),
            "{served:?}"
        );
        assert_eq!(handled.load(SeqCst), 1);

        // A later check of another revision closes the session again.
        answer_to(check(6, JP_ENHANCE_WIRE_REVISION + 1), cx).await;
        let refused = answer_to(allocate(7), cx).await;
        assert!(is_error(&refused), "{refused:?}");
        assert_eq!(handled.load(SeqCst), 1);
    }

    /// The client handled the server's messages since the first connection
    /// matched; a reconnection that does not, or loses the connection while
    /// asking, shuts them out again.
    #[gpui::test]
    async fn a_failed_check_on_a_reconnection_closes_the_client_again(cx: &mut TestAppContext) {
        let (_incoming_tx, incoming_rx) = mpsc::unbounded::<Envelope>();
        let (outgoing_tx, _outgoing_rx) = mpsc::unbounded::<Envelope>();
        let client =
            cx.update(|cx| ChannelClient::new(incoming_rx, outgoing_tx, cx, "client", false));
        client.require_confirmed_wire_revision();
        // Whether the failure reads as a mismatch, which reinstalls the server.
        let answers: [(Result<proto::CheckWireRevisionResponse>, bool); 3] = [
            (
                Ok(proto::CheckWireRevisionResponse {
                    revision: JP_ENHANCE_WIRE_REVISION + 1,
                }),
                true,
            ),
            (Err(anyhow!("refused")), true),
            (
                Err(anyhow::Error::new(oneshot::Canceled).context("connection lost")),
                false,
            ),
        ];
        for (answer, is_mismatch) in answers {
            let described = format!("{answer:?}");
            let matched = confirmed(
                &client,
                async {
                    Ok(proto::CheckWireRevisionResponse {
                        revision: JP_ENHANCE_WIRE_REVISION,
                    })
                },
                futures::future::pending::<()>(),
                async { Ok(()) },
            )
            .await;
            assert!(matched.is_ok() && client.wire_revision_confirmed());

            let result = confirmed(
                &client,
                async { answer },
                futures::future::pending::<()>(),
                async { Ok(()) },
            )
            .await;
            let error = result.expect_err(&described);
            assert_eq!(
                error.downcast_ref::<WireRevisionMismatch>().is_some(),
                is_mismatch,
                "{described}: {error:#}"
            );
            assert!(!client.wire_revision_confirmed(), "{described}");
        }
    }

    /// A reconnection checks the revision again, ahead of the replay: a server
    /// that answers another one does not get the connection back.
    #[gpui::test]
    async fn a_reconnection_checks_the_revision_again(
        cx: &mut TestAppContext,
        server_cx: &mut TestAppContext,
    ) {
        cx.update(|cx| release_channel::init(semver::Version::new(0, 0, 0), cx));
        let (opts, server_session, connect_guard) = RemoteClient::fake_server(cx, server_cx);
        let ping_handler = server_cx.new(|_| ());
        server_session.add_request_handler::<proto::Ping, _, _, _>(
            ping_handler.downgrade(),
            |_, _, _| async { Ok(proto::Ack {}) },
        );
        drop(connect_guard);
        let client = RemoteClient::connect_mock(opts, cx).await;
        assert_eq!(
            client.read_with(cx, |client, _| client.connection_state()),
            super::super::ConnectionState::Connected
        );

        server_cx.update(|cx| {
            cx.set_global(WireRevisionAnswerForTests(Some(
                JP_ENHANCE_WIRE_REVISION + 1,
            )))
        });
        client
            .update(cx, |client, cx| client.simulate_disconnect(cx))
            .detach();
        cx.run_until_parked();
        server_cx.run_until_parked();
        cx.run_until_parked();
        assert_ne!(
            client.read_with(cx, |client, _| client.connection_state()),
            super::super::ConnectionState::Connected,
            "a server of another revision must not get the connection back"
        );
    }
}
