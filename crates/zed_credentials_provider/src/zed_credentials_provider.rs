use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, LazyLock};

use anyhow::Result;
use credentials_provider::CredentialsProvider;
use futures::FutureExt as _;
use gpui::{App, AppContext as _, AsyncApp, Global, TaskExt as _};
use release_channel::ReleaseChannel;

/// An environment variable whose presence indicates that the system keychain
/// should be used in development.
///
/// By default, running Zed in development uses the development credentials
/// provider. Setting this environment variable allows you to interact with the
/// system keychain (for instance, if you need to test something).
///
/// Only works in development. Setting this environment variable in other
/// release channels is a no-op.
static ZED_DEVELOPMENT_USE_KEYCHAIN: LazyLock<bool> = LazyLock::new(|| {
    std::env::var("ZED_DEVELOPMENT_USE_KEYCHAIN").is_ok_and(|value| !value.is_empty())
});

pub struct ZedCredentialsProvider(pub Arc<dyn CredentialsProvider>);

impl Global for ZedCredentialsProvider {}

/// Returns the global [`CredentialsProvider`].
pub fn init_global(cx: &mut App) {
    // The `CredentialsProvider` trait has `Send + Sync` bounds on it, so it
    // seems like this is a false positive from Clippy.
    #[allow(clippy::arc_with_non_send_sync)]
    let provider = new(cx);
    cx.set_global(ZedCredentialsProvider(provider));
}

pub fn global(cx: &App) -> Arc<dyn CredentialsProvider> {
    cx.try_global::<ZedCredentialsProvider>()
        .map(|provider| provider.0.clone())
        .unwrap_or_else(|| new(cx))
}

fn new(cx: &App) -> Arc<dyn CredentialsProvider> {
    if uses_development_provider(cx) {
        Arc::new(DevelopmentCredentialsProvider::new())
    } else {
        Arc::new(KeychainCredentialsProvider)
    }
}

fn uses_development_provider(cx: &App) -> bool {
    match ReleaseChannel::try_global(cx) {
        Some(ReleaseChannel::Dev) => {
            // In development we default to using the development
            // credentials provider to avoid getting spammed by relentless
            // keychain access prompts.
            //
            // However, if the `ZED_DEVELOPMENT_USE_KEYCHAIN` environment
            // variable is set, we will use the actual keychain.
            //
            // ForgeKairos (ADR-F88): only macOS asks again after every
            // rebuild. Elsewhere development builds are used day to day, and
            // the file keeps API keys in plain text, so they use the store.
            cfg!(target_os = "macos") && !*ZED_DEVELOPMENT_USE_KEYCHAIN
        }
        Some(ReleaseChannel::Nightly | ReleaseChannel::Preview | ReleaseChannel::Stable) | None => {
            false
        }
    }
}

/// ForgeKairos (ADR-F88): moves what an earlier development build left in the
/// plain-text `development_credentials` file into the system's credential
/// store, on the platforms where development builds now use the store. The
/// file is deleted only once every entry is in the store, so a store that
/// refuses one loses nothing. Runs in the background so as not to hold up the
/// start.
pub fn migrate_development_credentials(cx: &App) {
    // On macOS the store is used in development only while
    // `ZED_DEVELOPMENT_USE_KEYCHAIN` is set; the next start without it reads
    // the file again, so the file must stay.
    if cfg!(target_os = "macos")
        || ReleaseChannel::try_global(cx) != Some(ReleaseChannel::Dev)
        || uses_development_provider(cx)
    {
        return;
    }
    let path = DevelopmentCredentialsProvider::new().path;
    let store = global(cx);
    cx.spawn(async move |cx| move_credentials_file_into(path, store.as_ref(), cx).await)
        .detach_and_log_err(cx);
}

async fn move_credentials_file_into(
    path: PathBuf,
    store: &dyn CredentialsProvider,
    cx: &AsyncApp,
) -> Result<()> {
    let credentials = cx
        .background_spawn({
            let path = path.clone();
            async move {
                match std::fs::read(&path) {
                    Ok(json) => serde_json::from_slice::<HashMap<String, (String, Vec<u8>)>>(&json)
                        .map(Some)
                        .map_err(anyhow::Error::from),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                    Err(error) => Err(error.into()),
                }
            }
        })
        .await?;
    let Some(credentials) = credentials else {
        return Ok(());
    };

    let mut failed = 0;
    for (url, (username, password)) in &credentials {
        // The store is shared with the installed release build. A different
        // value it already holds is not overwritten (the release build uses
        // it), nor is the file's dropped: which is current cannot be told, so
        // the file stays until the user settles it.
        let moved = match store.read_credentials(url, cx).await {
            Ok(Some((_, stored))) if stored == *password => Ok(()),
            Ok(Some(_)) => Err(anyhow::anyhow!(
                "the credential store already holds a different value"
            )),
            Ok(None) => {
                async {
                    store.write_credentials(url, username, password, cx).await?;
                    let read_back = store.read_credentials(url, cx).await?;
                    anyhow::ensure!(
                        read_back.is_some_and(|(_, stored)| stored == *password),
                        "the credential store did not give back what was written"
                    );
                    Ok(())
                }
                .await
            }
            Err(error) => Err(error),
        };
        if let Err(error) = moved {
            failed += 1;
            log::warn!(
                "couldn't move the development credentials for {url} into the credential store: {error:#}"
            );
        }
    }
    anyhow::ensure!(
        failed == 0,
        "{failed} of {} development credentials stay in {path:?}",
        credentials.len()
    );
    cx.background_spawn(async move { std::fs::remove_file(&path) })
        .await?;
    log::info!(
        "moved {} development credentials into the credential store",
        credentials.len()
    );
    Ok(())
}

/// A credentials provider that stores credentials in the system keychain.
struct KeychainCredentialsProvider;

impl CredentialsProvider for KeychainCredentialsProvider {
    fn read_credentials<'a>(
        &'a self,
        url: &'a str,
        cx: &'a AsyncApp,
    ) -> Pin<Box<dyn Future<Output = Result<Option<(String, Vec<u8>)>>> + 'a>> {
        async move { cx.update(|cx| cx.read_credentials(url)).await }.boxed_local()
    }

    fn write_credentials<'a>(
        &'a self,
        url: &'a str,
        username: &'a str,
        password: &'a [u8],
        cx: &'a AsyncApp,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + 'a>> {
        async move {
            cx.update(move |cx| cx.write_credentials(url, username, password))
                .await
        }
        .boxed_local()
    }

    fn delete_credentials<'a>(
        &'a self,
        url: &'a str,
        cx: &'a AsyncApp,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + 'a>> {
        async move { cx.update(move |cx| cx.delete_credentials(url)).await }.boxed_local()
    }
}

/// A credentials provider that stores credentials in a local file.
///
/// This MUST only be used in development, as this is not a secure way of storing
/// credentials on user machines.
///
/// Its existence is purely to work around the annoyance of having to constantly
/// re-allow access to the system keychain when developing Zed.
struct DevelopmentCredentialsProvider {
    path: PathBuf,
}

impl DevelopmentCredentialsProvider {
    fn new() -> Self {
        let path = paths::config_dir().join("development_credentials");

        Self { path }
    }

    fn load_credentials(&self) -> Result<HashMap<String, (String, Vec<u8>)>> {
        let json = std::fs::read(&self.path)?;
        let credentials: HashMap<String, (String, Vec<u8>)> = serde_json::from_slice(&json)?;

        Ok(credentials)
    }

    fn save_credentials(&self, credentials: &HashMap<String, (String, Vec<u8>)>) -> Result<()> {
        let json = serde_json::to_string(credentials)?;
        std::fs::write(&self.path, json)?;

        Ok(())
    }
}

impl CredentialsProvider for DevelopmentCredentialsProvider {
    fn read_credentials<'a>(
        &'a self,
        url: &'a str,
        _cx: &'a AsyncApp,
    ) -> Pin<Box<dyn Future<Output = Result<Option<(String, Vec<u8>)>>> + 'a>> {
        async move {
            Ok(self
                .load_credentials()
                .unwrap_or_default()
                .get(url)
                .cloned())
        }
        .boxed_local()
    }

    fn write_credentials<'a>(
        &'a self,
        url: &'a str,
        username: &'a str,
        password: &'a [u8],
        _cx: &'a AsyncApp,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + 'a>> {
        async move {
            let mut credentials = self.load_credentials().unwrap_or_default();
            credentials.insert(url.to_string(), (username.to_string(), password.to_vec()));

            self.save_credentials(&credentials)
        }
        .boxed_local()
    }

    fn delete_credentials<'a>(
        &'a self,
        url: &'a str,
        _cx: &'a AsyncApp,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + 'a>> {
        async move {
            let mut credentials = self.load_credentials()?;
            credentials.remove(url);

            self.save_credentials(&credentials)
        }
        .boxed_local()
    }
}

/// An in-memory [`CredentialsProvider`] for tests, which can be told to fail.
#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    use std::collections::HashMap;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

    use anyhow::Result;
    use credentials_provider::CredentialsProvider;
    use futures::FutureExt as _;
    use gpui::{App, AsyncApp};

    use crate::ZedCredentialsProvider;

    #[derive(Default)]
    pub struct InMemoryCredentialsProvider {
        credentials: Mutex<HashMap<String, (String, Vec<u8>)>>,
        fail_reads: AtomicBool,
        fail_writes: AtomicBool,
        lose_writes: AtomicBool,
    }

    impl InMemoryCredentialsProvider {
        /// Makes a new, empty one the global provider and returns it.
        pub fn install(cx: &mut App) -> Arc<Self> {
            let provider = Arc::new(Self::default());
            cx.set_global(ZedCredentialsProvider(provider.clone()));
            provider
        }

        pub fn get(&self, url: &str) -> Option<(String, Vec<u8>)> {
            self.lock().get(url).cloned()
        }

        pub fn insert(&self, url: &str, username: &str, password: &[u8]) {
            self.lock()
                .insert(url.to_string(), (username.to_string(), password.to_vec()));
        }

        pub fn len(&self) -> usize {
            self.lock().len()
        }

        pub fn is_empty(&self) -> bool {
            self.lock().is_empty()
        }

        pub fn set_fail_reads(&self, fail: bool) {
            self.fail_reads.store(fail, Ordering::SeqCst);
        }

        /// Also makes deletes fail: both change the store.
        pub fn set_fail_writes(&self, fail: bool) {
            self.fail_writes.store(fail, Ordering::SeqCst);
        }

        /// Makes writes report success and keep nothing, as a store that
        /// cannot be trusted with what it is given.
        pub fn set_lose_writes(&self, lose: bool) {
            self.lose_writes.store(lose, Ordering::SeqCst);
        }

        fn lock(&self) -> MutexGuard<'_, HashMap<String, (String, Vec<u8>)>> {
            self.credentials
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
        }
    }

    impl CredentialsProvider for InMemoryCredentialsProvider {
        fn read_credentials<'a>(
            &'a self,
            url: &'a str,
            _cx: &'a AsyncApp,
        ) -> Pin<Box<dyn Future<Output = Result<Option<(String, Vec<u8>)>>> + 'a>> {
            async move {
                anyhow::ensure!(
                    !self.fail_reads.load(Ordering::SeqCst),
                    "the test credential store refuses reads"
                );
                Ok(self.get(url))
            }
            .boxed_local()
        }

        fn write_credentials<'a>(
            &'a self,
            url: &'a str,
            username: &'a str,
            password: &'a [u8],
            _cx: &'a AsyncApp,
        ) -> Pin<Box<dyn Future<Output = Result<()>> + 'a>> {
            async move {
                anyhow::ensure!(
                    !self.fail_writes.load(Ordering::SeqCst),
                    "the test credential store refuses writes"
                );
                if !self.lose_writes.load(Ordering::SeqCst) {
                    self.insert(url, username, password);
                }
                Ok(())
            }
            .boxed_local()
        }

        // Like the Windows and macOS stores, deleting a missing entry fails.
        fn delete_credentials<'a>(
            &'a self,
            url: &'a str,
            _cx: &'a AsyncApp,
        ) -> Pin<Box<dyn Future<Output = Result<()>> + 'a>> {
            async move {
                anyhow::ensure!(
                    !self.fail_writes.load(Ordering::SeqCst),
                    "the test credential store refuses deletes"
                );
                anyhow::ensure!(
                    self.lock().remove(url).is_some(),
                    "no such entry in the test credential store"
                );
                Ok(())
            }
            .boxed_local()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::InMemoryCredentialsProvider;
    use super::*;
    use gpui::TestAppContext;

    fn write_file(path: &std::path::Path, entries: &[(&str, &str, &[u8])]) {
        let credentials = entries
            .iter()
            .map(|(url, username, password)| {
                (url.to_string(), (username.to_string(), password.to_vec()))
            })
            .collect::<HashMap<_, _>>();
        std::fs::write(
            path,
            serde_json::to_string(&credentials).expect("serializable"),
        )
        .expect("the file can be written");
    }

    #[gpui::test]
    async fn the_file_goes_once_every_entry_is_in_the_store(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = directory.path().join("development_credentials");
        write_file(
            &path,
            &[
                ("https://api.example.com", "Bearer", b"key-one"),
                ("https://other.example.com", "Bearer", b"key-two"),
            ],
        );
        let store = InMemoryCredentialsProvider::default();

        let async_cx = cx.to_async();
        move_credentials_file_into(path.clone(), &store, &async_cx)
            .await
            .expect("every entry moves");

        assert_eq!(
            store.get("https://api.example.com"),
            Some(("Bearer".to_string(), b"key-one".to_vec()))
        );
        assert_eq!(
            store.get("https://other.example.com"),
            Some(("Bearer".to_string(), b"key-two".to_vec()))
        );
        assert!(!path.exists(), "the plain-text file is deleted");
    }

    #[gpui::test]
    async fn the_file_stays_when_the_store_refuses_an_entry(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = directory.path().join("development_credentials");
        write_file(&path, &[("https://api.example.com", "Bearer", b"key-one")]);
        let store = InMemoryCredentialsProvider::default();
        store.set_fail_writes(true);

        let async_cx = cx.to_async();
        move_credentials_file_into(path.clone(), &store, &async_cx)
            .await
            .expect_err("an entry could not be moved");

        assert!(path.exists(), "nothing is lost: the file stays");
        assert!(store.is_empty());
    }

    /// Neither copy of a key the store and the file disagree on is lost: the
    /// store's is not overwritten, and the file stays.
    #[gpui::test]
    async fn a_value_the_store_holds_differently_keeps_both(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = directory.path().join("development_credentials");
        write_file(
            &path,
            &[("https://api.example.com", "Bearer", b"from-file")],
        );
        let store = InMemoryCredentialsProvider::default();
        store.insert("https://api.example.com", "Bearer", b"from-store");

        let async_cx = cx.to_async();
        move_credentials_file_into(path.clone(), &store, &async_cx)
            .await
            .expect_err("the two disagree");

        assert_eq!(
            store.get("https://api.example.com"),
            Some(("Bearer".to_string(), b"from-store".to_vec())),
            "the release build's key is not overwritten"
        );
        assert!(path.exists(), "nor is the file's dropped");
    }

    #[gpui::test]
    async fn a_value_the_store_already_holds_lets_the_file_go(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = directory.path().join("development_credentials");
        write_file(&path, &[("https://api.example.com", "Bearer", b"same")]);
        let store = InMemoryCredentialsProvider::default();
        store.insert("https://api.example.com", "Bearer", b"same");

        let async_cx = cx.to_async();
        move_credentials_file_into(path.clone(), &store, &async_cx)
            .await
            .expect("nothing to write");
        assert!(!path.exists());
    }

    /// A store that takes the write but does not give the value back is not
    /// trusted with it: the file stays.
    #[gpui::test]
    async fn the_file_stays_when_the_store_cannot_give_the_value_back(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = directory.path().join("development_credentials");
        write_file(&path, &[("https://api.example.com", "Bearer", b"key-one")]);
        let store = InMemoryCredentialsProvider::default();
        store.set_lose_writes(true);

        let async_cx = cx.to_async();
        move_credentials_file_into(path.clone(), &store, &async_cx)
            .await
            .expect_err("the write did not take");
        assert!(path.exists());
    }

    /// ADR-F88: outside macOS a development build keeps credentials in the
    /// system's store, not in the plain-text file.
    #[gpui::test]
    fn development_builds_use_the_store_outside_macos(cx: &mut TestAppContext) {
        cx.update(|cx| {
            release_channel::init_test(semver::Version::new(0, 0, 0), ReleaseChannel::Dev, cx);
            if !cfg!(target_os = "macos") {
                assert!(!uses_development_provider(cx));
            }
            release_channel::init_test(semver::Version::new(0, 0, 0), ReleaseChannel::Stable, cx);
            assert!(!uses_development_provider(cx));
        });
    }

    #[gpui::test]
    async fn no_file_is_nothing_to_do(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = directory.path().join("development_credentials");
        let store = InMemoryCredentialsProvider::default();

        let async_cx = cx.to_async();
        move_credentials_file_into(path, &store, &async_cx)
            .await
            .expect("no file, no error");
        assert!(store.is_empty());
    }
}
